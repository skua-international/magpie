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
	"net/http"
	"os"
	"strconv"
	"time"

	"github.com/container-storage-interface/spec/lib/go/csi"
	"golang.org/x/net/http2"
	"golang.org/x/net/http2/h2c"
	"google.golang.org/grpc"

	"github.com/skua-international/magpie/generated/go/csi/v1/csiv1connect"
	"github.com/skua-international/magpie/services/magpie-csi/internal/blob"
	"github.com/skua-international/magpie/services/magpie-csi/internal/capacity"
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
	// 30s, not the original 5m: confirmed live (2026-07-26) that a bulk
	// sync-daemon startup sync (a dozen-plus depots downloading in
	// parallel) can blow through the 5GiB headroom well inside a 5-minute
	// gap between checks, with no other trigger catching it in between --
	// mount-time EnsureCapacity calls (NodeStageVolume/NodePublishVolume)
	// only fire once per Pod lifecycle, not per write. This narrows the
	// exposure window; it doesn't eliminate the race outright (a fast
	// enough burst can still outrun any fixed poll interval) -- see
	// magpie#42 for the actual fix (sync-daemon reserving capacity
	// upfront, sized to what it's about to download, instead of this
	// driver polling blind).
	capacityCheckInterval, err := time.ParseDuration(envOr("CAPACITY_CHECK_INTERVAL", "30s"))
	if err != nil {
		log.Fatalf("invalid CAPACITY_CHECK_INTERVAL: %v", err)
	}
	// Where csi.v1.CapacityService listens (Node role only). Not 9808 --
	// the livenessprobe sidecar has that, and sidecars share this Pod's
	// network namespace.
	capacityAddr := envOr("CAPACITY_LISTEN_ADDR", "0.0.0.0:9809")

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

	// Both gated on being the Node role, for the same reason: only it has
	// a blob to manage. The watchdog stays regardless of the reservation
	// endpoint -- it's the fallback for anything that writes without
	// announcing itself first, which is every writer other than
	// sync-daemon's batch downloads.
	if rawNodeID != "" {
		go d.Blob().Watch(context.Background(), capacityCheckInterval)
		go serveCapacity(capacityAddr, d.Blob())
	}

	log.Printf("magpie-csi listening on %s (node_id=%s)", endpoint, nodeID)
	if err := server.Serve(listener); err != nil {
		log.Fatalf("gRPC server failed: %v", err)
	}
}

// serveCapacity runs magpie's own csi.v1.CapacityService alongside the
// CSI socket. Blocking, so main runs it in a goroutine; a failure here
// is logged rather than fatal -- losing the reservation endpoint costs
// the upfront-sizing optimization, but the watchdog still keeps the blob
// from filling, and taking the whole CSI driver down with it would stop
// every volume mount on this node instead.
func serveCapacity(addr string, b *blob.Manager) {
	mux := http.NewServeMux()
	mux.Handle(csiv1connect.NewCapacityServiceHandler(capacity.New(b)))

	// h2c so the Connect client can use HTTP/2 without TLS -- this is a
	// node-local endpoint on a private port, reached over the node's own
	// network.
	srv := &http.Server{
		Addr:              addr,
		Handler:           h2c.NewHandler(mux, &http2.Server{}),
		ReadHeaderTimeout: 10 * time.Second,
	}
	log.Printf("magpie-csi capacity service listening on %s", addr)
	if err := srv.ListenAndServe(); err != nil {
		log.Printf("capacity service stopped: %v (blob watchdog still running)", err)
	}
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}
