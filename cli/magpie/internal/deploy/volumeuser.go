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
// Idempotent -- safe to re-run against an already-provisioned host. Uses
// sudo throughout, deliberately: unlike k3s's own kubeconfig access
// (fixable by handing out group-readable access instead, see
// BootstrapK3s), creating a system user/group, writing udev rules, and
// chowning host directories are inherently root-only operations with no
// non-root alternative -- confirmed live, this failed outright
// ("groupadd: Permission denied") without it, running as a real non-root
// --ssh user the same way BootstrapK3s's own kubeconfig calls used to.
// Relies on passwordless sudo being available, same assumption
// BootstrapK3s's own k3s install already makes (its install script
// self-elevates via sudo internally) -- this only makes sense to run on
// the actual node volume-manager's DaemonSet will land on, which `Run()`
// only calls this from when that's true (locally with --bootstrap-k3s,
// or via RunNodeSetup on a --ssh target).
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
	return run(ctx, "sudo", "groupadd", "--system", name)
}

func ensureUser(ctx context.Context, name, group string) error {
	if err := exec.CommandContext(ctx, "id", "-u", name).Run(); err == nil {
		return nil // already exists
	}
	return run(ctx, "sudo", "useradd", "--system", "--gid", group, "--no-create-home", "--shell", "/usr/sbin/nologin", name)
}

// resolveID doesn't need sudo -- `id` just reads /etc/passwd, no
// privilege required to look up another user's numeric ID.
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
	// A plain read, not sudo -- /etc/udev/rules.d/*.rules files are
	// world-readable by convention (0644), only writing needs root.
	existing, _ := os.ReadFile(udevRulesPath)
	if string(existing) == rule {
		return nil // already correct, nothing to reload
	}

	// `sudo tee` rather than os.WriteFile: sudo only elevates the
	// subprocess it spawns, not this Go process's own direct syscalls,
	// so writing a root-owned path needs to go through a privileged
	// subprocess, not os.WriteFile itself.
	write := exec.CommandContext(ctx, "sudo", "tee", udevRulesPath)
	write.Stdin = strings.NewReader(rule)
	if out, err := write.CombinedOutput(); err != nil {
		return fmt.Errorf("failed to write %s: %w: %s", udevRulesPath, err, strings.TrimSpace(string(out)))
	}

	if err := run(ctx, "sudo", "udevadm", "control", "--reload-rules"); err != nil {
		return err
	}
	// /dev/loop-control is a "misc"-subsystem device (created by the loop
	// kernel module), not "block" -- the loopN device nodes themselves
	// are "block", but loop-control isn't one of them. A block-only
	// trigger silently never re-applies this rule's GROUP/MODE to
	// loop-control, leaving it at its original (root-only) permissions,
	// which then makes `losetup -f` itself fail with "Operation not
	// permitted" -- confirmed live: losetup needs to open loop-control to
	// allocate a free loop device before it ever touches a specific
	// /dev/loopN. Both subsystems need triggering for the rule to
	// actually take effect on every device it names.
	if err := run(ctx, "sudo", "udevadm", "trigger", "--subsystem-match=block"); err != nil {
		return err
	}
	return run(ctx, "sudo", "udevadm", "trigger", "--subsystem-match=misc")
}

func ensureOwnedDir(ctx context.Context, dir, user, group string) error {
	if err := run(ctx, "sudo", "mkdir", "-p", dir); err != nil {
		return fmt.Errorf("failed to create %s: %w", dir, err)
	}
	return run(ctx, "sudo", "chown", fmt.Sprintf("%s:%s", user, group), dir)
}
