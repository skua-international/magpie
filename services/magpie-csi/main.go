// magpie-csi is a small CSI driver: loop-mounts a growable btrfs blob
// per node so sync-daemon's reflink CoW claims work regardless of what
// filesystem the host actually has. One binary, deployed twice (a
// Controller Deployment with the standard external-provisioner sidecar,
// and a Node DaemonSet with node-driver-registrar) -- both always
// register all three CSI services (Identity/Controller/Node), same as
// the official csi-driver-host-path reference driver does it. See
// README.md's "Architecture" section for why this replaced an earlier
// bespoke Connect-RPC design (volume-manager).
package main

import (
	"context"
	"log"
	"net"
	"os"
	"strconv"
	"time"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"google.golang.org/grpc"

	"github.com/skua-international/magpie/services/magpie-csi/internal/driver"
)

func main() {
	endpoint := envOr("CSI_ENDPOINT", "unix:///csi/csi.sock")
	socketPath := endpoint
	if len(socketPath) > 7 && socketPath[:7] == "unix://" {
		socketPath = socketPath[7:]
	}

	// NODE_ID is only ever set by the Node DaemonSet (a fieldRef to
	// spec.nodeName) -- the Controller Deployment doesn't set it, isn't
	// privileged, and has no hostPath access to the blob at all, so this
	// doubles as this process's only signal for whether it's safe (or
	// even possible) to run the capacity watchdog below.
	rawNodeID := os.Getenv("NODE_ID")
	nodeID := rawNodeID
	if nodeID == "" {
		nodeID = "unknown"
	}
	blobImage := envOr("BLOB_IMAGE_PATH", "/var/lib/magpie-csi/blob/content.img")
	blobMount := envOr("BLOB_MOUNT_PATH", "/var/lib/magpie-csi/mnt")
	maxSizeGB, err := strconv.ParseInt(envOr("MAX_SIZE_GIB", "0"), 10, 64)
	if err != nil {
		log.Fatalf("invalid MAX_SIZE_GIB: %v", err)
	}
	capacityCheckInterval, err := time.ParseDuration(envOr("CAPACITY_CHECK_INTERVAL", "5m"))
	if err != nil {
		log.Fatalf("invalid CAPACITY_CHECK_INTERVAL: %v", err)
	}

	// A stale socket from a previous run (e.g. an unclean restart) makes
	// net.Listen fail with "address already in use" -- safe to remove
	// unconditionally, this process is the only thing that ever creates
	// this specific path.
	if err := os.Remove(socketPath); err != nil && !os.IsNotExist(err) {
		log.Fatalf("failed to remove stale socket %s: %v", socketPath, err)
	}

	listener, err := net.Listen("unix", socketPath)
	if err != nil {
		log.Fatalf("failed to listen on %s: %v", socketPath, err)
	}

	d := driver.New(nodeID, blobImage, blobMount, maxSizeGB)
	server := grpc.NewServer()
	csi.RegisterIdentityServer(server, d)
	csi.RegisterControllerServer(server, d)
	csi.RegisterNodeServer(server, d)

	if rawNodeID != "" {
		go d.Blob().Watch(context.Background(), capacityCheckInterval)
	}

	log.Printf("magpie-csi listening on %s (node_id=%s)", endpoint, nodeID)
	if err := server.Serve(listener); err != nil {
		log.Fatalf("gRPC server failed: %v", err)
	}
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}
