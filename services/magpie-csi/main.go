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
	"log"
	"net"
	"os"
	"strconv"

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

	nodeID := envOr("NODE_ID", "unknown")
	blobImage := envOr("BLOB_IMAGE_PATH", "/var/lib/magpie-csi/blob/content.img")
	blobMount := envOr("BLOB_MOUNT_PATH", "/var/lib/magpie-csi/mnt")
	initialGB, err := strconv.ParseInt(envOr("INITIAL_SIZE_GIB", "40"), 10, 64)
	if err != nil {
		log.Fatalf("invalid INITIAL_SIZE_GIB: %v", err)
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

	d := driver.New(nodeID, blobImage, blobMount, initialGB)
	server := grpc.NewServer()
	csi.RegisterIdentityServer(server, d)
	csi.RegisterControllerServer(server, d)
	csi.RegisterNodeServer(server, d)

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
