// Package driver implements the CSI Identity, Controller, and Node
// services in one type -- kubelet/the standard CSI sidecars
// (external-provisioner, node-driver-registrar) all expect Identity to
// respond regardless of which role (Controller or Node) this specific
// deployment plays, exactly like the official csi-driver-host-path
// reference driver does it: one binary, always registers all three
// services, deployed twice (Controller Deployment + Node DaemonSet)
// with different sidecars in front of each.
package driver

import (
	"context"

	"github.com/container-storage-interface/spec/lib/go/csi"

	"github.com/skua-international/magpie/services/magpie-csi/internal/blob"
)

const (
	DriverName    = "csi.magpie.skua.io"
	DriverVersion = "0.1.0"
)

type Driver struct {
	csi.UnimplementedIdentityServer
	csi.UnimplementedControllerServer
	csi.UnimplementedNodeServer

	nodeID    string
	blobMount string

	// blob is one shared Manager for this Driver's whole lifetime, not
	// constructed fresh per RPC -- it must be shared: blob.Manager's own
	// mutex only ever protects concurrent callers of the *same* instance,
	// and the capacity watchdog (see main.go) now calls EnsureCapacity
	// concurrently with whatever NodeStageVolume/NodePublishVolume are
	// doing, on its own ticker, independent of any kubelet-driven RPC --
	// a fresh Manager per call (the previous behavior here) would give
	// each caller its own never-contended mutex, silently defeating the
	// whole point of locking around the loop-device/btrfs operations.
	blob *blob.Manager
}

// New builds a Driver -- nodeID is only meaningful for the Node role
// (NodeGetInfo), blobImage/blobMount/maxSizeGB only for the Node role's
// actual blob management (NodeStageVolume, and the capacity watchdog).
// The Controller role ignores all three; see controller.go's own doc
// for why CreateVolume doesn't need to touch the blob at all. maxSizeGB
// is 0 for "unlimited" -- there's no equivalent "initial size" parameter
// anymore, see blob.Manager.desiredFloor's own doc for why that's now
// computed instead of configured.
func New(nodeID, blobImage, blobMount string, maxSizeGB int64) *Driver {
	return &Driver{
		nodeID:    nodeID,
		blobMount: blobMount,
		blob:      blob.NewManager(blobImage, blobMount, maxSizeGB<<30),
	}
}

// Blob exposes the Driver's one shared blob.Manager -- main.go's
// capacity watchdog needs it, and it's simpler to reuse this than to
// duplicate Driver's construction logic there.
func (d *Driver) Blob() *blob.Manager {
	return d.blob
}

func (d *Driver) GetPluginInfo(context.Context, *csi.GetPluginInfoRequest) (*csi.GetPluginInfoResponse, error) {
	return &csi.GetPluginInfoResponse{
		Name:          DriverName,
		VendorVersion: DriverVersion,
	}, nil
}

func (d *Driver) GetPluginCapabilities(context.Context, *csi.GetPluginCapabilitiesRequest) (*csi.GetPluginCapabilitiesResponse, error) {
	return &csi.GetPluginCapabilitiesResponse{
		Capabilities: []*csi.PluginCapability{
			{
				Type: &csi.PluginCapability_Service_{
					Service: &csi.PluginCapability_Service{
						Type: csi.PluginCapability_Service_CONTROLLER_SERVICE,
					},
				},
			},
		},
	}, nil
}

func (d *Driver) Probe(context.Context, *csi.ProbeRequest) (*csi.ProbeResponse, error) {
	return &csi.ProbeResponse{}, nil
}
