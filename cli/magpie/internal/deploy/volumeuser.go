package deploy

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

const (
	volumeUserName  = "magpie-volume"
	volumeGroupName = "magpie-volume"
	udevRulesPath   = "/etc/udev/rules.d/70-magpie-loop.rules"
)

// EnsureVolumeManagerUser provisions the dedicated, minimally-privileged
// host user/group volume-manager's DaemonSet runs as -- see
// charts/magpie/templates/volume-manager-daemonset.yaml's own doc for why
// this exists instead of privileged: true. Creates the system user/group
// if missing, grants it access to loop devices via a dedicated udev rule
// (not the broad `disk` group, which would also cover every other block
// device on the host), and creates+chowns the two host directories the
// blob needs (its own backing file's parent dir, and the mount target).
// Returns the resulting UID/GID for the chart's
// volumeManager.runAsUser/runAsGroup values.
//
// Idempotent -- safe to re-run against an already-provisioned host.
// Assumes it's already running as root or via passwordless sudo, same
// assumption BootstrapK3s makes -- this only makes sense to run on the
// actual node volume-manager's DaemonSet will land on, which `Run()`
// only calls this from when that's true (locally with --bootstrap-k3s,
// or via RunRemoteInstall, which re-execs this same command on the
// remote host directly).
func EnsureVolumeManagerUser(ctx context.Context, blobImagePath, blobMountPath string) (uid, gid int, err error) {
	if err := ensureGroup(ctx, volumeGroupName); err != nil {
		return 0, 0, err
	}
	if err := ensureUser(ctx, volumeUserName, volumeGroupName); err != nil {
		return 0, 0, err
	}

	uid, err = resolveID(ctx, "-u", volumeUserName)
	if err != nil {
		return 0, 0, err
	}
	gid, err = resolveID(ctx, "-g", volumeGroupName)
	if err != nil {
		return 0, 0, err
	}

	if err := ensureLoopDeviceUdevRule(ctx, volumeGroupName); err != nil {
		return 0, 0, err
	}

	if err := ensureOwnedDir(ctx, filepath.Dir(blobImagePath), volumeUserName, volumeGroupName); err != nil {
		return 0, 0, err
	}
	if err := ensureOwnedDir(ctx, blobMountPath, volumeUserName, volumeGroupName); err != nil {
		return 0, 0, err
	}

	return uid, gid, nil
}

func ensureGroup(ctx context.Context, name string) error {
	if err := exec.CommandContext(ctx, "getent", "group", name).Run(); err == nil {
		return nil // already exists
	}
	return run(ctx, "groupadd", "--system", name)
}

func ensureUser(ctx context.Context, name, group string) error {
	if err := exec.CommandContext(ctx, "id", "-u", name).Run(); err == nil {
		return nil // already exists
	}
	return run(ctx, "useradd", "--system", "--gid", group, "--no-create-home", "--shell", "/usr/sbin/nologin", name)
}

func resolveID(ctx context.Context, args ...string) (int, error) {
	out, err := exec.CommandContext(ctx, "id", args...).Output()
	if err != nil {
		return 0, fmt.Errorf("id %s: %w", strings.Join(args, " "), err)
	}
	var id int
	if _, err := fmt.Sscanf(strings.TrimSpace(string(out)), "%d", &id); err != nil {
		return 0, fmt.Errorf("failed to parse id output %q: %w", out, err)
	}
	return id, nil
}

// Loop devices (and /dev/loop-control, which allocates them) are
// normally root:disk 0660 -- granting the broad `disk` group would also
// hand over every other block device on the host, not just loop ones.
// This rule scopes access to exactly loop devices instead.
func ensureLoopDeviceUdevRule(ctx context.Context, group string) error {
	rule := fmt.Sprintf(
		"SUBSYSTEM==\"block\", KERNEL==\"loop[0-9]*\", GROUP=\"%s\", MODE=\"0660\"\nKERNEL==\"loop-control\", GROUP=\"%s\", MODE=\"0660\"\n",
		group, group,
	)
	existing, _ := os.ReadFile(udevRulesPath)
	if string(existing) == rule {
		return nil // already correct, nothing to reload
	}
	if err := os.WriteFile(udevRulesPath, []byte(rule), 0o644); err != nil {
		return fmt.Errorf("failed to write %s: %w", udevRulesPath, err)
	}
	if err := run(ctx, "udevadm", "control", "--reload-rules"); err != nil {
		return err
	}
	return run(ctx, "udevadm", "trigger", "--subsystem-match=block")
}

func ensureOwnedDir(ctx context.Context, dir, user, group string) error {
	if err := os.MkdirAll(dir, 0o750); err != nil {
		return fmt.Errorf("failed to create %s: %w", dir, err)
	}
	return run(ctx, "chown", fmt.Sprintf("%s:%s", user, group), dir)
}
