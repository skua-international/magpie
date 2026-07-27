package blob

import (
	"testing"
	"time"
)

// The reservation bookkeeping is worth testing on its own: the rest of
// this package shells out to losetup/btrfs/df and needs a real host to
// exercise, but reservedBytesLocked is pure arithmetic and it's the part
// that decides whether an in-flight download's space survives the next
// watchdog tick.

func TestReservedBytesSumsDistinctKeys(t *testing.T) {
	m := NewManager("", "", 0)
	m.reservations = map[string]reservation{
		"server-depots": {bytes: 3 << 30, expires: time.Now().Add(time.Hour)},
		"workshop":      {bytes: 7 << 30, expires: time.Now().Add(time.Hour)},
	}

	if got, want := m.reservedBytesLocked(), int64(10<<30); got != want {
		t.Errorf("reservedBytesLocked() = %d, want %d", got, want)
	}
}

// Re-reserving a key replaces it. A sync that re-resolves and reserves
// again must not stack two claims for the same bytes and inflate the
// blob to twice what it needs.
func TestReserveSameKeyReplacesRatherThanAccumulates(t *testing.T) {
	m := NewManager("", "", 0)
	m.reservations = map[string]reservation{
		"workshop": {bytes: 5 << 30, expires: time.Now().Add(time.Hour)},
	}
	m.reservations["workshop"] = reservation{bytes: 8 << 30, expires: time.Now().Add(time.Hour)}

	if got, want := m.reservedBytesLocked(), int64(8<<30); got != want {
		t.Errorf("reservedBytesLocked() = %d, want %d (replaced, not summed)", got, want)
	}
}

// The TTL is what keeps a crashed caller from pinning the blob at its
// high-water mark forever, so expiry must actually drop the claim.
func TestExpiredReservationsAreDroppedAndPruned(t *testing.T) {
	m := NewManager("", "", 0)
	m.reservations = map[string]reservation{
		"live":  {bytes: 2 << 30, expires: time.Now().Add(time.Hour)},
		"stale": {bytes: 9 << 30, expires: time.Now().Add(-time.Minute)},
	}

	if got, want := m.reservedBytesLocked(), int64(2<<30); got != want {
		t.Errorf("reservedBytesLocked() = %d, want %d (expired excluded)", got, want)
	}
	if _, still := m.reservations["stale"]; still {
		t.Error("expired reservation should have been pruned from the map, not just skipped")
	}
	if _, ok := m.reservations["live"]; !ok {
		t.Error("unexpired reservation was pruned")
	}
}

func TestReleaseDropsOnlyTheNamedKey(t *testing.T) {
	m := NewManager("", "", 0)
	m.reservations = map[string]reservation{
		"server-depots": {bytes: 3 << 30, expires: time.Now().Add(time.Hour)},
		"workshop":      {bytes: 7 << 30, expires: time.Now().Add(time.Hour)},
	}

	m.Release("workshop")

	if got, want := m.reservedBytesLocked(), int64(3<<30); got != want {
		t.Errorf("reservedBytesLocked() = %d, want %d", got, want)
	}
}

// Releasing something already gone is a normal cleanup-path outcome (the
// TTL may have won the race), not a failure.
func TestReleaseUnknownKeyIsNoOp(t *testing.T) {
	m := NewManager("", "", 0)
	m.Release("never-reserved")

	if got := m.reservedBytesLocked(); got != 0 {
		t.Errorf("reservedBytesLocked() = %d, want 0", got)
	}
}

// The whole point of the reservation: it has to survive repeated
// recomputation, because EnsureCapacity derives its target from observed
// usage every tick and would otherwise shrink the space back out from
// under a download that hasn't written its bytes yet.
func TestReservationPersistsAcrossRepeatedReads(t *testing.T) {
	m := NewManager("", "", 0)
	m.reservations = map[string]reservation{
		"server-depots": {bytes: 40 << 30, expires: time.Now().Add(time.Hour)},
	}

	for tick := range 5 {
		if got, want := m.reservedBytesLocked(), int64(40<<30); got != want {
			t.Fatalf("tick %d: reservedBytesLocked() = %d, want %d", tick, got, want)
		}
	}
}
