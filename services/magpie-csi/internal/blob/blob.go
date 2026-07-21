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
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
)

// nonrootUID is the fixed UID/GID every non-distroless container in
// this chart that needs to run non-root uses explicitly (distroless
// images bake it into their own USER directive instead -- see
// cli/magpie/internal/deploy/bootstrap.go's own distrolessNonrootID,
// the same value, kept in sync by convention rather than by sharing
// code across these two separate Go modules).
const nonrootUID = "65532"

type Manager struct {
	imagePath        string
	mountPath        string
	initialSizeBytes int64

	mu sync.Mutex
}

type GrowOutcome struct {
	TotalBytes int64
	FreeBytes  int64
	Grew       bool
}

func NewManager(imagePath, mountPath string, initialSizeBytes int64) *Manager {
	return &Manager{
		imagePath:        imagePath,
		mountPath:        mountPath,
		initialSizeBytes: initialSizeBytes,
	}
}

// EnsureCapacity ensures bytesNeeded free space exists, growing (or
// first-time bootstrapping) if not. Idempotent -- safe to call even
// when nothing actually needs to happen.
func (m *Manager) EnsureCapacity(ctx context.Context, bytesNeeded int64) (GrowOutcome, error) {
	m.mu.Lock()
	defer m.mu.Unlock()

	mounted, err := m.isMounted(ctx)
	if err != nil {
		return GrowOutcome{}, err
	}
	if !mounted {
		minSize := bytesNeeded
		if m.initialSizeBytes > minSize {
			minSize = m.initialSizeBytes
		}
		if err := m.bootstrap(ctx, minSize); err != nil {
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
	if free >= bytesNeeded {
		return GrowOutcome{TotalBytes: total, FreeBytes: free, Grew: false}, nil
	}

	target := total + (bytesNeeded - free)
	if err := m.growTo(ctx, target); err != nil {
		return GrowOutcome{}, fmt.Errorf("failed to grow blob filesystem: %w", err)
	}
	total, free, err = m.statvfs(ctx)
	if err != nil {
		return GrowOutcome{}, err
	}
	return GrowOutcome{TotalBytes: total, FreeBytes: free, Grew: true}, nil
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
