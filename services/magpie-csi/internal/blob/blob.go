// Package blob owns the actual privileged host operations: creating/
// growing a loop-mounted btrfs blob per node so reflink CoW claims work
// regardless of what filesystem the host actually has. Shells out to
// standard tools (truncate, losetup, mkfs.btrfs, mount, btrfs, df)
// rather than binding raw ioctls directly -- ported from magpie's
// earlier volume-manager service (services/volume-manager/src/blob.rs
// in git history), which did exactly this over a bespoke Connect-RPC
// API instead of CSI's NodeStageVolume/ControllerExpandVolume.
//
// mountPath is a node-local path the Node plugin's own NodeStageVolume
// mounts the loop-mounted blob at -- privileged: true, since loop-
// device attach and btrfs's resize ioctl both genuinely need real host
// device access.
package blob

import (
	"bytes"
	"context"
	"fmt"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"
)

// nonrootUID is the fixed UID/GID every non-distroless container in
// this chart that needs to run non-root uses explicitly (distroless
// images bake it into their own USER directive instead -- see
// cli/magpie/internal/deploy/bootstrap.go's own distrolessNonrootID,
// the same value, kept in sync by convention rather than by sharing
// code across these two separate Go modules).
const nonrootUID = "65532"

// headroomBytes is the free space EnsureCapacity always tries to
// maintain -- hardcoded, not chart-configurable, on the same reasoning
// as the initial floor below: this is a correctness property of the
// driver (enough room for an in-flight download or snapshot to land
// without racing the filesystem filling up), not a deployment-specific
// tuning knob.
const headroomBytes = 5 << 30

// shrinkHysteresisBytes guards against grow/shrink thrashing every tick
// -- only shrink once there's meaningfully more free space than the
// target actually needs, not the instant it's 1 byte over.
const shrinkHysteresisBytes = 1 << 30

type Manager struct {
	imagePath    string
	mountPath    string
	maxSizeBytes int64 // 0 == unlimited

	mu sync.Mutex
}

type GrowOutcome struct {
	TotalBytes int64
	FreeBytes  int64
	Grew       bool
	Shrunk     bool
}

// NewManager builds a Manager for the one shared node-local blob.
// maxSizeBytes caps how large EnsureCapacity will ever grow it to; 0
// means unlimited (chart default -- see reflinkStorage.maxSizeGiB).
func NewManager(imagePath, mountPath string, maxSizeBytes int64) *Manager {
	return &Manager{
		imagePath:    imagePath,
		mountPath:    mountPath,
		maxSizeBytes: maxSizeBytes,
	}
}

// EnsureCapacity bootstraps the blob if it isn't mounted yet, then
// reconciles its size toward (current non-workshop game content +
// headroomBytes) at minimum, and (current total usage + headroomBytes)
// in general -- growing if that's short, shrinking back down if there's
// well more free space than that (e.g. after mod sources/snapshots got
// deleted). This is deliberately parameter-free: there is no
// "bytesNeeded" a caller could pass 0 for and silently skip real
// reconciliation -- every single call, from every call site (see
// node.go's NodeStageVolume/NodePublishVolume, and Watch below), does
// the same real check. That's a direct fix for a live incident
// (2026-07-25): every existing call site used to pass a hardcoded
// bytesNeeded=0, which is trivially satisfied by any amount of free
// space including zero, so the blob silently filled to 100% and never
// grew until nothing could be written at all (sync-daemon crash-looping
// on SIGBUS from an mmap page fault into a completely full filesystem).
func (m *Manager) EnsureCapacity(ctx context.Context) (GrowOutcome, error) {
	m.mu.Lock()
	defer m.mu.Unlock()

	mounted, err := m.isMounted(ctx)
	if err != nil {
		return GrowOutcome{}, err
	}
	if !mounted {
		// Nothing's been synced onto a not-yet-mounted blob, so
		// gameContentBytes is always 0 here -- this naturally bootstraps
		// at just headroomBytes, then grows for real once sync-daemon's
		// startup game-file sync (main.rs: "syncing game files at
		// startup") actually lands content and the next tick sees it.
		floor, err := m.desiredFloor(ctx)
		if err != nil {
			return GrowOutcome{}, err
		}
		if err := m.bootstrap(ctx, m.clamp(floor)); err != nil {
			return GrowOutcome{}, fmt.Errorf("failed to bootstrap blob filesystem: %w", err)
		}
		total, free, err := m.statvfs(ctx)
		if err != nil {
			return GrowOutcome{}, err
		}
		return GrowOutcome{TotalBytes: total, FreeBytes: free, Grew: true}, nil
	}

	total, free, err := m.statvfs(ctx)
	if err != nil {
		return GrowOutcome{}, err
	}
	floor, err := m.desiredFloor(ctx)
	if err != nil {
		return GrowOutcome{}, err
	}
	used := total - free
	target := m.clamp(max(used+headroomBytes, floor))

	switch {
	case target > total:
		if err := m.growTo(ctx, target); err != nil {
			return GrowOutcome{}, fmt.Errorf("failed to grow blob filesystem: %w", err)
		}
	case total-target >= shrinkHysteresisBytes:
		if err := m.shrinkTo(ctx, target); err != nil {
			return GrowOutcome{}, fmt.Errorf("failed to shrink blob filesystem: %w", err)
		}
	default:
		return GrowOutcome{TotalBytes: total, FreeBytes: free}, nil
	}

	total, free, err = m.statvfs(ctx)
	if err != nil {
		return GrowOutcome{}, err
	}
	return GrowOutcome{TotalBytes: total, FreeBytes: free, Grew: target > total, Shrunk: target < total}, nil
}

// clamp caps target at maxSizeBytes, if one's configured (0 == no cap).
// A configured max below what headroomBytes/the game-content floor
// actually needs is a misconfiguration, not something to silently
// enforce past the point of being able to hold the game's own files --
// the caller (EnsureCapacity) logs when this actually bites.
func (m *Manager) clamp(target int64) int64 {
	if m.maxSizeBytes > 0 && target > m.maxSizeBytes {
		return m.maxSizeBytes
	}
	return target
}

// desiredFloor is (current on-disk size of everything under content/
// except content/workshop) + headroomBytes -- the minimum this blob
// should ever be, regardless of how much workshop content has been
// deleted. Computed fresh every call (via `du`, same shell-out
// convention as the rest of this package) rather than cached: the game
// content tree only changes on a CDLC release, so this is cheap relative
// to how rarely it actually changes, and caching it would just be one
// more thing that could go stale.
func (m *Manager) desiredFloor(ctx context.Context) (int64, error) {
	contentPath := filepath.Join(m.mountPath, "content")
	totalBytes, err := duBytes(ctx, contentPath)
	if err != nil {
		return 0, err
	}
	workshopBytes, err := duBytes(ctx, filepath.Join(contentPath, "workshop"))
	if err != nil {
		return 0, err
	}
	return (totalBytes - workshopBytes) + headroomBytes, nil
}

// duBytes returns 0, not an error, for a path that doesn't exist yet --
// both content/ (pre-bootstrap) and content/workshop (before the first
// mod source ever syncs) are legitimately missing at times this gets
// called.
func duBytes(ctx context.Context, path string) (int64, error) {
	if _, err := os.Stat(path); os.IsNotExist(err) {
		return 0, nil
	}
	var stdout, stderr bytes.Buffer
	cmd := exec.CommandContext(ctx, "du", "-sb", path)
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return 0, fmt.Errorf("du -sb %s failed: %s", path, strings.TrimSpace(stderr.String()))
	}
	fields := strings.Fields(stdout.String())
	if len(fields) < 1 {
		return 0, fmt.Errorf("unexpected du output: %s", stdout.String())
	}
	n, err := strconv.ParseInt(fields[0], 10, 64)
	if err != nil {
		return 0, fmt.Errorf("unexpected du output: %s", stdout.String())
	}
	return n, nil
}

// Watch periodically reconciles capacity on its own schedule -- the
// only thing that guarantees this happens even when no volume RPC comes
// in for a while (sync-daemon writes directly to the mounted blob, not
// through any CSI call the Node plugin would otherwise observe). Blocks
// until ctx is cancelled; call this in its own goroutine.
func (m *Manager) Watch(ctx context.Context, interval time.Duration) {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			outcome, err := m.EnsureCapacity(ctx)
			if err != nil {
				log.Printf("capacity watchdog: EnsureCapacity failed: %v", err)
				continue
			}
			switch {
			case outcome.Grew:
				log.Printf("capacity watchdog: grew blob to %d bytes (%d free)", outcome.TotalBytes, outcome.FreeBytes)
			case outcome.Shrunk:
				log.Printf("capacity watchdog: shrunk blob to %d bytes (%d free)", outcome.TotalBytes, outcome.FreeBytes)
			}
		}
	}
}

func (m *Manager) Status(ctx context.Context) (total, free int64, err error) {
	m.mu.Lock()
	defer m.mu.Unlock()
	mounted, err := m.isMounted(ctx)
	if err != nil {
		return 0, 0, err
	}
	if !mounted {
		return 0, 0, nil
	}
	return m.statvfs(ctx)
}

// IsReady is true only once the blob is actually mounted -- a health
// check has nothing more useful to do with an error than report
// unhealthy, so any failure collapses to false rather than propagating.
func (m *Manager) IsReady(ctx context.Context) bool {
	mounted, err := m.isMounted(ctx)
	return err == nil && mounted
}

func (m *Manager) isMounted(ctx context.Context) (bool, error) {
	// --mountpoint, not --target: --target walks up to the nearest
	// containing filesystem and succeeds for *any* ordinary directory
	// (confirmed live -- a plain, never-mounted directory happily
	// "matches" its host filesystem), so it can never actually report
	// "not mounted". --mountpoint only matches when the path itself is
	// a real mount point.
	cmd := exec.CommandContext(ctx, "findmnt", "--noheadings", "--mountpoint", m.mountPath)
	return cmd.Run() == nil, nil
}

// bootstrap does first-time setup: creates the backing file if it
// doesn't exist yet (never re-creates or re-formats an image that's
// already there -- this path also runs on every restart until the
// mount succeeds, so it has to be safe to call against a real,
// populated image after a host reboot dropped the loop attachment/
// mount).
func (m *Manager) bootstrap(ctx context.Context, minSizeBytes int64) error {
	isNewImage := false
	if _, err := os.Stat(m.imagePath); os.IsNotExist(err) {
		isNewImage = true
	}

	if err := os.MkdirAll(filepath.Dir(m.imagePath), 0o755); err != nil {
		return fmt.Errorf("failed to create %s: %w", filepath.Dir(m.imagePath), err)
	}

	if isNewImage {
		if err := run(ctx, "truncate", "-s", strconv.FormatInt(minSizeBytes, 10), m.imagePath); err != nil {
			return err
		}
	}

	loopDev, err := m.attachLoopDevice(ctx)
	if err != nil {
		return err
	}

	if isNewImage {
		if err := run(ctx, "mkfs.btrfs", "-f", loopDev); err != nil {
			return err
		}
	}

	if err := os.MkdirAll(m.mountPath, 0o755); err != nil {
		return fmt.Errorf("failed to create %s: %w", m.mountPath, err)
	}
	if err := run(ctx, "mount", loopDev, m.mountPath); err != nil {
		return err
	}
	return m.ensureContentSubvolume(ctx)
}

// ensureContentSubvolume makes sure <mountPath>/content is a real btrfs
// subvolume, not a plain directory -- driver.NodeStageVolume needs a
// genuine subvolume as the source for `btrfs subvolume snapshot`, one
// per ModeSnapshot volume (every ArmaServer's own PVC); sync-daemon's
// own ModeGolden PVC bind-mounts this path directly instead, read-write,
// no snapshot involved. Idempotent: a plain `os.Stat` existence check is
// enough to skip re-creating it on every restart -- `btrfs subvolume
// create` on a path that already exists (subvolume or not) just fails,
// so this only ever runs once per blob's lifetime, on the very first
// bootstrap.
//
// <mountPath>/claims (the parent directory individual per-volume
// snapshot subvolumes get created under -- driver.snapshotPath) stays a
// plain directory; only content itself needs to be a subvolume.
func (m *Manager) ensureContentSubvolume(ctx context.Context) error {
	contentPath := filepath.Join(m.mountPath, "content")
	claimsPath := filepath.Join(m.mountPath, "claims")

	if _, err := os.Stat(contentPath); os.IsNotExist(err) {
		if err := run(ctx, "btrfs", "subvolume", "create", contentPath); err != nil {
			return fmt.Errorf("failed to create content subvolume: %w", err)
		}
	} else if err != nil {
		return fmt.Errorf("failed to stat %s: %w", contentPath, err)
	}
	// A pre-existing plain directory from before this subvolume
	// requirement existed is left alone rather than converted in place
	// (btrfs has no in-place directory->subvolume conversion) --
	// deliberately not handled here, this deployment predates any real
	// upgrade-compatibility guarantee.

	// Unconditional, not just on first creation -- confirmed live this
	// has to run every bootstrap, not only when the subvolume is brand
	// new: a content subvolume created before this chown existed at all
	// stayed root:root forever otherwise (chown only ever ran once, at
	// creation, same class of bug as the kubelet hostPath
	// DirectoryOrCreate gotcha documented elsewhere in this codebase --
	// "only fixes it if freshly created" quietly never fixes a
	// pre-existing wrong state). `btrfs subvolume create` runs as root
	// (this Node plugin is the one privileged component in the whole
	// stack) -- chown to the fixed nonroot UID/GID every other
	// container in this chart runs as, so sync-daemon (which writes
	// here directly) can actually run non-root too. Every per-server
	// snapshot taken from this tree (driver.NodePublishVolume)
	// inherits this same ownership automatically -- btrfs snapshot
	// preserves it -- so the launcher Pods reading/writing those need
	// no chown of their own.
	if err := run(ctx, "chown", nonrootUID+":"+nonrootUID, contentPath); err != nil {
		return fmt.Errorf("failed to chown content subvolume: %w", err)
	}

	return os.MkdirAll(claimsPath, 0o755)
}

func (m *Manager) growTo(ctx context.Context, newSizeBytes int64) error {
	if err := run(ctx, "truncate", "-s", strconv.FormatInt(newSizeBytes, 10), m.imagePath); err != nil {
		return err
	}
	loopDev, err := m.attachLoopDevice(ctx)
	if err != nil {
		return err
	}
	// Tells the kernel to re-read the backing file's now-larger size.
	if err := run(ctx, "losetup", "-c", loopDev); err != nil {
		return err
	}
	return run(ctx, "btrfs", "filesystem", "resize", "max", m.mountPath)
}

// shrinkTo is growTo's mirror image, in reverse order for the same
// reason growTo's order matters: the btrfs resize has to happen first,
// while the loop device (and its backing file) are still the *old*,
// larger size, so btrfs has room to actually relocate any data/metadata
// currently living past the new, smaller boundary. Only once that
// succeeds is it safe to truncate the backing file down and tell the
// kernel (losetup -c) to notice the smaller size -- doing this before
// the btrfs resize could truncate away data btrfs hadn't relocated yet.
func (m *Manager) shrinkTo(ctx context.Context, newSizeBytes int64) error {
	if err := run(ctx, "btrfs", "filesystem", "resize", strconv.FormatInt(newSizeBytes, 10), m.mountPath); err != nil {
		return err
	}
	if err := run(ctx, "truncate", "-s", strconv.FormatInt(newSizeBytes, 10), m.imagePath); err != nil {
		return err
	}
	loopDev, err := m.attachLoopDevice(ctx)
	if err != nil {
		return err
	}
	// Tells the kernel to re-read the backing file's now-smaller size.
	return run(ctx, "losetup", "-c", loopDev)
}

// attachLoopDevice is idempotent: reuses an existing loop association
// for this image if one's already there (the common case after a
// restart) instead of attaching a second one.
func (m *Manager) attachLoopDevice(ctx context.Context) (string, error) {
	out, _ := exec.CommandContext(ctx, "losetup", "-j", m.imagePath).Output()
	if listing := strings.TrimSpace(string(out)); listing != "" {
		if dev, _, ok := strings.Cut(listing, ":"); ok && dev != "" {
			return dev, nil
		}
	}

	var stdout, stderr bytes.Buffer
	cmd := exec.CommandContext(ctx, "losetup", "-f", "--show", m.imagePath)
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return "", fmt.Errorf("losetup -f --show %s failed: %s", m.imagePath, strings.TrimSpace(stderr.String()))
	}
	return strings.TrimSpace(stdout.String()), nil
}

// statvfs returns (total, free) bytes via `df` -- shelling out rather
// than binding statvfs(2) directly, same reasoning as everything else
// in this package.
func (m *Manager) statvfs(ctx context.Context) (total, free int64, err error) {
	var stdout, stderr bytes.Buffer
	cmd := exec.CommandContext(ctx, "df", "--output=size,avail", "--block-size=1", m.mountPath)
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return 0, 0, fmt.Errorf("df %s failed: %s", m.mountPath, strings.TrimSpace(stderr.String()))
	}
	lines := strings.Split(strings.TrimSpace(stdout.String()), "\n")
	if len(lines) < 2 {
		return 0, 0, fmt.Errorf("unexpected df output: %s", stdout.String())
	}
	fields := strings.Fields(lines[1])
	if len(fields) < 2 {
		return 0, 0, fmt.Errorf("unexpected df output: %s", stdout.String())
	}
	total, err = strconv.ParseInt(fields[0], 10, 64)
	if err != nil {
		return 0, 0, fmt.Errorf("unexpected df output: %s", stdout.String())
	}
	free, err = strconv.ParseInt(fields[1], 10, 64)
	if err != nil {
		return 0, 0, fmt.Errorf("unexpected df output: %s", stdout.String())
	}
	return total, free, nil
}

func run(ctx context.Context, name string, args ...string) error {
	var stderr bytes.Buffer
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("%s %s failed: %s", name, strings.Join(args, " "), strings.TrimSpace(stderr.String()))
	}
	return nil
}
