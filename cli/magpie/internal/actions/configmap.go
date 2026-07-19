package actions

import (
	"context"
	"fmt"
	"os/exec"
	"strings"
)

// EnsureConfigMapExists creates an empty ConfigMap if one by this name
// doesn't already exist -- `kubectl edit` (unlike `kubectl apply`) fails
// outright against a nonexistent object, and neither a fresh per-server
// config override nor (in principle) the chart-managed baseline are
// guaranteed to already be there. Uses CombinedOutput rather than
// streaming to os.Stderr directly: this runs the same way under the TUI
// (mid-render, alt-screen active) as it does under the plain CLI, and a
// stray direct terminal write from the TUI's side would corrupt the
// display.
func EnsureConfigMapExists(ctx context.Context, namespace, name string) error {
	if err := exec.CommandContext(ctx, "kubectl", "get", "configmap", name, "-n", namespace).Run(); err == nil {
		return nil
	}
	out, err := exec.CommandContext(ctx, "kubectl", "create", "configmap", name, "-n", namespace).CombinedOutput()
	if err != nil {
		return fmt.Errorf("kubectl create configmap %s: %w: %s", name, err, strings.TrimSpace(string(out)))
	}
	return nil
}

// ConfigMapEditCmd builds (but doesn't run) the `kubectl edit` command for
// a ConfigMap, reusing kubectl's own $EDITOR handling and get/apply-on-
// save flow rather than hand-rolling a temp-file/diff/apply cycle.
// Returned unstarted with Stdin/Stdout/Stderr left nil -- the direct CLI
// wires those to os.Stdin/Stdout/Stderr itself before Run(), while the
// TUI hands this straight to tea.ExecProcess, which fills them in around
// suspending/resuming the terminal instead.
func ConfigMapEditCmd(ctx context.Context, namespace, name string) *exec.Cmd {
	return exec.CommandContext(ctx, "kubectl", "edit", "configmap", name, "-n", namespace)
}
