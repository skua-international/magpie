package driver

import (
	"context"
	"os"
	"os/exec"
	"path/filepath"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/skua-international/magpie/services/magpie-csi/internal/blob"
)

// blobManager is created once, lazily, on the first NodeStageVolume/
// NodePublishVolume that needs it -- every volume this driver ever
// touches shares the one node-local blob (see blob.Manager's own doc
// for why: golden content and every snapshot taken from it need to
// live on one real filesystem).
func (d *Driver) blobManager() *blob.Manager {
	return blob.NewManager(d.blobImage, d.blobMount, d.initialGB<<30)
}

// snapshotPath is where a given ephemeral volume's own btrfs snapshot
// lives, under the shared blob's "claims" directory (see blob.go's
// ensureContentSubvolume) -- one subvolume per ArmaServer content
// volume ID, created and deleted entirely within NodePublishVolume/
// NodeUnpublishVolume (see those functions' own doc for why: these are
// CSI inline ephemeral volumes, so Stage/Unstage and Controller
// CreateVolume/DeleteVolume are never called for them at all).
func (d *Driver) snapshotPath(volumeID string) string {
	return filepath.Join(d.blobMount, "claims", volumeID)
}

// NodeStageVolume mounts the shared node-local blob's content
// subvolume at StagingTargetPath, read-write -- the actual privileged
// loop/losetup/mkfs.btrfs/mount work (blob.Manager.EnsureCapacity)
// lives here, not in CreateVolume (see controller.go's own doc for
// why). Only ever called for the one golden content PVC
// (golden-content-pvc.yaml, the only Persistent-lifecycle volume this
// driver has) -- every ArmaServer's own content is a CSI inline
// ephemeral volume instead (NodePublishVolume's own doc), which skips
// Stage/Unstage entirely per the CSI spec.
func (d *Driver) NodeStageVolume(ctx context.Context, req *csi.NodeStageVolumeRequest) (*csi.NodeStageVolumeResponse, error) {
	if req.GetVolumeId() == "" {
		return nil, status.Error(codes.InvalidArgument, "volume_id is required")
	}
	if req.GetStagingTargetPath() == "" {
		return nil, status.Error(codes.InvalidArgument, "staging_target_path is required")
	}

	if _, err := d.blobManager().EnsureCapacity(ctx, 0); err != nil {
		return nil, status.Errorf(codes.Internal, "failed to mount blob: %v", err)
	}

	contentPath := filepath.Join(d.blobMount, "content")
	if err := os.MkdirAll(req.GetStagingTargetPath(), 0o755); err != nil {
		return nil, status.Errorf(codes.Internal, "failed to create %s: %v", req.GetStagingTargetPath(), err)
	}
	if mounted, _ := isMounted(ctx, req.GetStagingTargetPath()); !mounted {
		if err := bindMount(ctx, contentPath, req.GetStagingTargetPath(), false); err != nil {
			return nil, status.Errorf(codes.Internal, "failed to stage volume: %v", err)
		}
	}

	return &csi.NodeStageVolumeResponse{}, nil
}

// NodeUnstageVolume just undoes NodeStageVolume's own bind mount --
// the shared blob itself stays mounted (it's node-local, outlives any
// one volume's staging), and there's never a snapshot subvolume to
// clean up here (only NodePublishVolume's ephemeral-volume path
// creates those, and it cleans up its own in NodeUnpublishVolume).
func (d *Driver) NodeUnstageVolume(ctx context.Context, req *csi.NodeUnstageVolumeRequest) (*csi.NodeUnstageVolumeResponse, error) {
	if req.GetStagingTargetPath() == "" {
		return nil, status.Error(codes.InvalidArgument, "staging_target_path is required")
	}
	if mounted, _ := isMounted(ctx, req.GetStagingTargetPath()); mounted {
		if err := run(ctx, "umount", req.GetStagingTargetPath()); err != nil {
			return nil, status.Errorf(codes.Internal, "failed to unstage volume: %v", err)
		}
	}
	return &csi.NodeUnstageVolumeResponse{}, nil
}

// NodePublishVolume handles two distinct cases, distinguished purely by
// whether StagingTargetPath is set (CSI leaves it empty for inline
// ephemeral volumes -- there's no separate Stage step for those at
// all, per the spec):
//
//   - Staged (the golden content PVC): bind-mount the already-staged
//     path into the target path kubelet gives the Pod, same as any
//     regular Persistent volume.
//   - Not staged (every ArmaServer's own content -- a CSI inline
//     ephemeral volume declared directly in the launcher Pod spec, see
//     services/controller/src/reconcile.rs): this is the *only* RPC
//     that ever fires for one of these at all (no CreateVolume,
//     ControllerPublishVolume, or NodeStageVolume), so the whole
//     provision-a-fresh-snapshot lifecycle happens right here --
//     bootstrap the blob if needed, take a fresh (writable -- see the
//     snapshot call's own comment) btrfs snapshot of the current golden
//     content under this volume's own ID (or reuse one that's already
//     there, for a retry/already-published call), and bind-mount that.
func (d *Driver) NodePublishVolume(ctx context.Context, req *csi.NodePublishVolumeRequest) (*csi.NodePublishVolumeResponse, error) {
	if req.GetTargetPath() == "" {
		return nil, status.Error(codes.InvalidArgument, "target_path is required")
	}

	if err := os.MkdirAll(req.GetTargetPath(), 0o755); err != nil {
		return nil, status.Errorf(codes.Internal, "failed to create %s: %v", req.GetTargetPath(), err)
	}
	if mounted, _ := isMounted(ctx, req.GetTargetPath()); mounted {
		return &csi.NodePublishVolumeResponse{}, nil
	}

	if req.GetStagingTargetPath() != "" {
		if err := bindMount(ctx, req.GetStagingTargetPath(), req.GetTargetPath(), req.GetReadonly()); err != nil {
			return nil, status.Errorf(codes.Internal, "failed to publish volume: %v", err)
		}
		return &csi.NodePublishVolumeResponse{}, nil
	}

	if req.GetVolumeId() == "" {
		return nil, status.Error(codes.InvalidArgument, "volume_id is required")
	}
	if _, err := d.blobManager().EnsureCapacity(ctx, 0); err != nil {
		return nil, status.Errorf(codes.Internal, "failed to mount blob: %v", err)
	}
	contentPath := filepath.Join(d.blobMount, "content")
	snapshotPath := d.snapshotPath(req.GetVolumeId())
	if _, err := os.Stat(snapshotPath); os.IsNotExist(err) {
		// Not -r (read-only): the launcher Pod mounts this read-write
		// (see reconcile.rs's launcher Pod spec) -- a read-only btrfs
		// subvolume stays read-only at the filesystem level regardless
		// of mount options, so `-r` here would silently defeat that
		// even if NodePublishVolume's own bind mount asked for rw.
		// Nothing's expected to actually persist anything here across
		// this Pod's lifetime; it's a fresh CoW snapshot either way.
		if err := run(ctx, "btrfs", "subvolume", "snapshot", contentPath, snapshotPath); err != nil {
			return nil, status.Errorf(codes.Internal, "failed to snapshot volume: %v", err)
		}
	} else if err != nil {
		return nil, status.Errorf(codes.Internal, "failed to stat %s: %v", snapshotPath, err)
	}
	if err := bindMount(ctx, snapshotPath, req.GetTargetPath(), req.GetReadonly()); err != nil {
		return nil, status.Errorf(codes.Internal, "failed to publish volume: %v", err)
	}
	return &csi.NodePublishVolumeResponse{}, nil
}

// NodeUnpublishVolume unmounts the target, then deletes this volume's
// own snapshot subvolume if it has one -- a no-op for the golden PVC
// (nothing was ever created under snapshotPath for it), and the real
// cleanup for a torn-down ArmaServer content volume. DeleteVolume
// itself (controller.go) is never even called for these (see
// NodePublishVolume's own doc), so this is the only place that
// happens.
func (d *Driver) NodeUnpublishVolume(ctx context.Context, req *csi.NodeUnpublishVolumeRequest) (*csi.NodeUnpublishVolumeResponse, error) {
	if req.GetTargetPath() == "" {
		return nil, status.Error(codes.InvalidArgument, "target_path is required")
	}
	if mounted, _ := isMounted(ctx, req.GetTargetPath()); mounted {
		if err := run(ctx, "umount", req.GetTargetPath()); err != nil {
			return nil, status.Errorf(codes.Internal, "failed to unpublish volume: %v", err)
		}
	}
	if req.GetVolumeId() != "" {
		snapshotPath := d.snapshotPath(req.GetVolumeId())
		if _, err := os.Stat(snapshotPath); err == nil {
			if err := run(ctx, "btrfs", "subvolume", "delete", snapshotPath); err != nil {
				return nil, status.Errorf(codes.Internal, "failed to delete snapshot: %v", err)
			}
		}
	}
	return &csi.NodeUnpublishVolumeResponse{}, nil
}

func (d *Driver) NodeGetCapabilities(context.Context, *csi.NodeGetCapabilitiesRequest) (*csi.NodeGetCapabilitiesResponse, error) {
	capability := func(t csi.NodeServiceCapability_RPC_Type) *csi.NodeServiceCapability {
		return &csi.NodeServiceCapability{
			Type: &csi.NodeServiceCapability_Rpc{
				Rpc: &csi.NodeServiceCapability_RPC{Type: t},
			},
		}
	}
	return &csi.NodeGetCapabilitiesResponse{
		Capabilities: []*csi.NodeServiceCapability{
			capability(csi.NodeServiceCapability_RPC_STAGE_UNSTAGE_VOLUME),
		},
	}, nil
}

func (d *Driver) NodeGetInfo(context.Context, *csi.NodeGetInfoRequest) (*csi.NodeGetInfoResponse, error) {
	return &csi.NodeGetInfoResponse{
		NodeId: d.nodeID,
		AccessibleTopology: &csi.Topology{
			Segments: map[string]string{TopologyKey: d.nodeID},
		},
	}, nil
}

func isMounted(ctx context.Context, path string) (bool, error) {
	// --mountpoint, not --target: see blob.Manager.isMounted's own
	// comment -- the staging/publish paths are os.MkdirAll'd (onto the
	// host's own filesystem) before this check runs, so --target's
	// walk-up-to-nearest-filesystem behavior made every staging/publish
	// directory look "already mounted" on the very first call, and the
	// actual bind mount from the btrfs blob never ran. Confirmed live:
	// sync-daemon's /content ended up as a plain directory on the
	// node's root xfs filesystem instead of the loop-mounted btrfs
	// blob, and `btrfs subvolume snapshot` failed with "Not a Btrfs
	// filesystem" as a result.
	cmd := exec.CommandContext(ctx, "findmnt", "--noheadings", "--mountpoint", path)
	return cmd.Run() == nil, nil
}

func bindMount(ctx context.Context, source, target string, readonly bool) error {
	if err := run(ctx, "mount", "--bind", source, target); err != nil {
		return err
	}
	if readonly {
		return run(ctx, "mount", "-o", "remount,bind,ro", target)
	}
	return nil
}

func run(ctx context.Context, name string, args ...string) error {
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.Stderr = os.Stderr
	return cmd.Run()
}
