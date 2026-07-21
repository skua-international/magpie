package driver

import (
	"context"
	"crypto/sha256"
	"encoding/hex"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// TopologyKey identifies which node a volume's blob actually lives on
// -- this is local storage, pinned to whichever node NodeStageVolume
// first mounts it on, same as any other local-storage CSI driver.
const TopologyKey = "topology.magpie.skua.io/node"

// CreateVolume deliberately does no host work at all -- the Controller
// Deployment isn't guaranteed to run on the same node the volume will
// actually be used on (that's the whole reason CSI splits Controller
// from Node), so the real loop-mount/blob work happens in
// NodeStageVolume instead, once WaitForFirstConsumer has resolved which
// node the volume is actually needed on. This just derives a stable
// VolumeId from the requested name (idempotent -- repeated CreateVolume
// calls for the same name return the same ID, per the CSI spec) and
// echoes back whichever topology segment the CO asked for.
func (d *Driver) CreateVolume(_ context.Context, req *csi.CreateVolumeRequest) (*csi.CreateVolumeResponse, error) {
	if req.GetName() == "" {
		return nil, status.Error(codes.InvalidArgument, "name is required")
	}

	sizeBytes := int64(1 << 30) // 1GiB floor if the CO didn't ask for anything specific
	if cr := req.GetCapacityRange(); cr != nil && cr.GetRequiredBytes() > 0 {
		sizeBytes = cr.GetRequiredBytes()
	}

	var topology []*csi.Topology
	if segs := req.GetAccessibilityRequirements().GetRequisite(); len(segs) > 0 {
		topology = append(topology, segs[0])
	} else if segs := req.GetAccessibilityRequirements().GetPreferred(); len(segs) > 0 {
		topology = append(topology, segs[0])
	}

	return &csi.CreateVolumeResponse{
		Volume: &csi.Volume{
			VolumeId:           volumeID(req.GetName()),
			CapacityBytes:      sizeBytes,
			AccessibleTopology: topology,
		},
	}, nil
}

// DeleteVolume is a no-op beyond acknowledging the request -- the only
// volume that ever goes through the Controller service at all is the
// one golden content PVC (golden-content-pvc.yaml); every ArmaServer's
// own content is a CSI inline ephemeral volume instead (see
// NodePublishVolume's own doc), which never calls CreateVolume/
// DeleteVolume in the first place. The golden PVC's backing directory
// is intentionally left in place either way -- it's the continuously-
// synced content tree, not something a PVC delete should destroy; the
// StorageClass's own reclaimPolicy: Retain already makes that explicit
// at the Kubernetes level.
func (d *Driver) DeleteVolume(context.Context, *csi.DeleteVolumeRequest) (*csi.DeleteVolumeResponse, error) {
	return &csi.DeleteVolumeResponse{}, nil
}

func (d *Driver) ControllerGetCapabilities(context.Context, *csi.ControllerGetCapabilitiesRequest) (*csi.ControllerGetCapabilitiesResponse, error) {
	capability := func(t csi.ControllerServiceCapability_RPC_Type) *csi.ControllerServiceCapability {
		return &csi.ControllerServiceCapability{
			Type: &csi.ControllerServiceCapability_Rpc{
				Rpc: &csi.ControllerServiceCapability_RPC{Type: t},
			},
		}
	}
	return &csi.ControllerGetCapabilitiesResponse{
		Capabilities: []*csi.ControllerServiceCapability{
			capability(csi.ControllerServiceCapability_RPC_CREATE_DELETE_VOLUME),
		},
	}, nil
}

// volumeID is deterministic from name alone -- CreateVolume's own
// idempotency requirement (the CSI spec: repeated calls for the same
// name must not provision more than one volume) falls out of this for
// free, no separate bookkeeping needed.
func volumeID(name string) string {
	sum := sha256.Sum256([]byte(name))
	return "magpie-" + hex.EncodeToString(sum[:])[:16]
}
