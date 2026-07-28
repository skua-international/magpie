// Package capacity serves magpie's own csi.v1.CapacityService, the side
// channel sync-daemon uses to say "I am about to write N bytes" before
// it starts writing them.
//
// Deliberately not part of the CSI gRPC surface in internal/driver: that
// socket belongs to kubelet and the CSI spec has no call meaning this.
// Served on a normal TCP port, and only by the Node role -- the
// Controller Deployment has no hostPath access to a blob at all.
package capacity

import (
	"context"
	"errors"
	"fmt"
	"log"
	"time"

	"connectrpc.com/connect"

	csiv1 "github.com/skua-international/magpie/generated/go/csi/v1"
	"github.com/skua-international/magpie/services/magpie-csi/internal/blob"
)

// maxTTL bounds how long a single reservation can hold space, however
// long the caller asked for. A reservation suppresses the shrink path
// for its whole lifetime, so an implausible TTL from a buggy or
// misconfigured caller would strand the blob at its high-water mark for
// that long with no way to reclaim it short of restarting this process.
const maxTTL = 6 * time.Hour

// defaultTTL is used when a caller sends 0. Long enough to cover a cold
// full-content sync, short enough to self-heal in one working session.
const defaultTTL = time.Hour

// Service implements csiv1connect.CapacityServiceHandler against the
// node's one shared blob.Manager. It must be the same *Manager the CSI
// driver and the watchdog use -- that Manager's mutex is what serializes
// a reservation against an in-progress NodeStageVolume or watchdog tick,
// and a second instance would each get its own uncontended lock.
type Service struct {
	blob *blob.Manager
}

func New(b *blob.Manager) *Service {
	return &Service{blob: b}
}

func (s *Service) ReserveCapacity(
	ctx context.Context,
	req *connect.Request[csiv1.ReserveCapacityRequest],
) (*connect.Response[csiv1.ReserveCapacityResponse], error) {
	key := req.Msg.GetKey()
	if key == "" {
		return nil, connect.NewError(connect.CodeInvalidArgument, errors.New("key is required"))
	}

	// Reservations are summed into a signed int64 target, so a bytes
	// value past the int64 range would wrap negative and silently shrink
	// the blob instead of growing it.
	rawBytes := req.Msg.GetBytes()
	if rawBytes > uint64(1<<62) {
		return nil, connect.NewError(connect.CodeInvalidArgument,
			fmt.Errorf("bytes %d is implausibly large", rawBytes))
	}

	ttl := time.Duration(req.Msg.GetTtlSeconds()) * time.Second
	switch {
	case ttl <= 0:
		ttl = defaultTTL
	case ttl > maxTTL:
		log.Printf("capacity: clamping %s reservation TTL from %s to %s", key, ttl, maxTTL)
		ttl = maxTTL
	}

	outcome, satisfied, err := s.blob.Reserve(ctx, key, int64(rawBytes), ttl)
	if err != nil {
		return nil, connect.NewError(connect.CodeInternal, err)
	}
	if !satisfied {
		// Not an RPC error: the caller is expected to go ahead anyway
		// (there may still be room, and failing the whole sync would be
		// worse than letting it try), so this has to be loud here.
		log.Printf("capacity: reservation %s for %d bytes NOT satisfied -- blob is %d bytes with %d free, likely capped by MAX_SIZE_GIB",
			key, rawBytes, outcome.TotalBytes, outcome.FreeBytes)
	} else {
		log.Printf("capacity: reserved %d bytes for %s (ttl %s); blob now %d bytes, %d free",
			rawBytes, key, ttl, outcome.TotalBytes, outcome.FreeBytes)
	}

	return connect.NewResponse(&csiv1.ReserveCapacityResponse{
		TotalBytes: uint64(outcome.TotalBytes),
		FreeBytes:  uint64(outcome.FreeBytes),
		Satisfied:  satisfied,
	}), nil
}

func (s *Service) ReleaseCapacity(
	_ context.Context,
	req *connect.Request[csiv1.ReleaseCapacityRequest],
) (*connect.Response[csiv1.ReleaseCapacityResponse], error) {
	key := req.Msg.GetKey()
	if key == "" {
		return nil, connect.NewError(connect.CodeInvalidArgument, errors.New("key is required"))
	}
	s.blob.Release(key)
	log.Printf("capacity: released reservation %s", key)
	return connect.NewResponse(&csiv1.ReleaseCapacityResponse{}), nil
}
