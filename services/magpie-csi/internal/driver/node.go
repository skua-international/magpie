package driver

import (
	"context"
	"os"
	"os/exec"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/skua-international/magpie/services/magpie-csi/internal/blob"
)

// blobManager is created once, lazily, on the first NodeStageVolume --
// every volume this driver ever stages shares the one node-local blob
// (see blob.Manager's own doc for why: reflink needs content/claims on
// one real filesystem, and this chart only ever has one PVC in
// practice), so there's no per-volume state to track here at all.
func (d *Driver) blobManager() *blob.Manager {
	return blob.NewManager(d.blobImage, d.blobMount, d.initialGB<<30)
}

// NodeStageVolume mounts the shared node-local blob at
// StagingTargetPath -- the actual privileged loop/losetup/mkfs.btrfs/
// mount work (blob.Manager.EnsureCapacity) lives here, not in
// CreateVolume (see controller.go's own doc for why).
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

	// The blob is mounted at d.blobMount (a fixed node-local path, see
	// main.go's flags) -- bind-mount that into the CO-provided staging
	// path so every volume's staging path, whatever kubelet chose,
	// actually resolves to the one shared blob.
	if err := os.MkdirAll(req.GetStagingTargetPath(), 0o755); err != nil {
		return nil, status.Errorf(codes.Internal, "failed to create %s: %v", req.GetStagingTargetPath(), err)
	}
	if mounted, _ := isMounted(ctx, req.GetStagingTargetPath()); !mounted {
		if err := bindMount(ctx, d.blobMount, req.GetStagingTargetPath(), false); err != nil {
			return nil, status.Errorf(codes.Internal, "failed to stage volume: %v", err)
		}
	}

	return &csi.NodeStageVolumeResponse{}, nil
}

// NodeUnstageVolume deliberately leaves the shared blob itself mounted
// -- it's shared across every volume this driver has ever staged on
// this node (see blobManager's own doc), so unmounting it here would be
// wrong if anything else is still using it. Only the bind mount this
// specific volume's staging path got is undone.
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

// NodePublishVolume bind-mounts the already-staged path into the
// target path kubelet actually gives the Pod -- kubelet's own subPath
// handling (content vs. claims, see the chart's sync-daemon-
// deployment.yaml) applies on top of whatever this exposes, so this
// just needs to expose the full staged volume, read-write or read-only
// per the request.
func (d *Driver) NodePublishVolume(ctx context.Context, req *csi.NodePublishVolumeRequest) (*csi.NodePublishVolumeResponse, error) {
	if req.GetStagingTargetPath() == "" {
		return nil, status.Error(codes.InvalidArgument, "staging_target_path is required")
	}
	if req.GetTargetPath() == "" {
		return nil, status.Error(codes.InvalidArgument, "target_path is required")
	}

	if err := os.MkdirAll(req.GetTargetPath(), 0o755); err != nil {
		return nil, status.Errorf(codes.Internal, "failed to create %s: %v", req.GetTargetPath(), err)
	}
	if mounted, _ := isMounted(ctx, req.GetTargetPath()); !mounted {
		if err := bindMount(ctx, req.GetStagingTargetPath(), req.GetTargetPath(), req.GetReadonly()); err != nil {
			return nil, status.Errorf(codes.Internal, "failed to publish volume: %v", err)
		}
	}
	return &csi.NodePublishVolumeResponse{}, nil
}

func (d *Driver) NodeUnpublishVolume(ctx context.Context, req *csi.NodeUnpublishVolumeRequest) (*csi.NodeUnpublishVolumeResponse, error) {
	if req.GetTargetPath() == "" {
		return nil, status.Error(codes.InvalidArgument, "target_path is required")
	}
	if mounted, _ := isMounted(ctx, req.GetTargetPath()); mounted {
		if err := run(ctx, "umount", req.GetTargetPath()); err != nil {
			return nil, status.Errorf(codes.Internal, "failed to unpublish volume: %v", err)
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
