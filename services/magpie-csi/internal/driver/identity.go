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
	blobImage string
	blobMount string
	initialGB int64
}

// New builds a Driver -- nodeID is only meaningful for the Node role
// (NodeGetInfo), blobImage/blobMount/initialGB only for the Node role's
// actual blob management (NodeStageVolume). The Controller role ignores
// all three; see controller.go's own doc for why CreateVolume doesn't
// need to touch the blob at all.
func New(nodeID, blobImage, blobMount string, initialGB int64) *Driver {
	return &Driver{
		nodeID:    nodeID,
		blobImage: blobImage,
		blobMount: blobMount,
		initialGB: initialGB,
	}
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
