package cmd

import (
	"context"
	"fmt"
	"os"

	"github.com/spf13/cobra"
	"golang.org/x/term"

	"github.com/skua-international/magpie/cli/internal/actions"
)

// editBaselineConfigMap runs actions.ConfigMapEditCmd (kubectl edit)
// directly -- the plain CLI path, as opposed to the TUI's, which hands
// the same *exec.Cmd to tea.ExecProcess instead (see tui/create_server.go).
func editBaselineConfigMap(ctx context.Context, namespace, release string) error {
	return editConfigMap(ctx, namespace, actions.BaselineConfigMapName(release))
}

// editConfigMap wires actions.ConfigMapEditCmd's unstarted *exec.Cmd to
// this process's own stdio and runs it -- also used by servers.go's
// per-server config override flow (see maybePromptServerConfigMap).
func editConfigMap(ctx context.Context, namespace, name string) error {
	cmd := actions.ConfigMapEditCmd(ctx, namespace, name)
	cmd.Stdin = os.Stdin
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

// maybePromptEditBaselineConfigMap is called after a successful install/
// deploy. Skips the prompt entirely (no hang) when stdin isn't a
// terminal -- CI/scripted `magpiectl install` runs must never block
// waiting for input.
func maybePromptEditBaselineConfigMap(ctx context.Context, namespace, release string) {
	name := actions.BaselineConfigMapName(release)
	if !term.IsTerminal(int(os.Stdin.Fd())) {
		fmt.Printf("==> Edit the cluster-wide Arma config baseline any time via: kubectl edit configmap %s -n %s\n", name, namespace)
		return
	}

	fmt.Print("==> Edit the cluster-wide Arma config baseline now? [y/N] ")
	var answer string
	_, _ = fmt.Scanln(&answer)
	if answer == "y" || answer == "Y" {
		if err := editBaselineConfigMap(ctx, namespace, release); err != nil {
			fmt.Fprintf(os.Stderr, "warning: kubectl edit failed: %v\n", err)
		}
	}
	fmt.Printf("==> Edit it later any time via: kubectl edit configmap %s -n %s\n", name, namespace)
}

// maybePromptServerConfigMap offers the same "edit it now" flow
// maybePromptEditBaselineConfigMap gives the cluster-wide baseline, but
// for a single server's own override ConfigMap right after creating it
// -- skipped entirely (no hang, no ConfigMap created) when stdin isn't a
// terminal, same reasoning as the baseline prompt. Returns the ConfigMap
// name to pass as CreateServerRequest.config_map, or "" if the operator
// declined (server gets baseline-only config).
func maybePromptServerConfigMap(ctx context.Context, namespace, serverName string) string {
	if !term.IsTerminal(int(os.Stdin.Fd())) {
		return ""
	}

	fmt.Print("==> Configure a per-server config override now? [y/N] ")
	var answer string
	_, _ = fmt.Scanln(&answer)
	if answer != "y" && answer != "Y" {
		return ""
	}

	name := serverName + "-config"
	fmt.Printf("==> ConfigMap name [%s]: ", name)
	var input string
	_, _ = fmt.Scanln(&input)
	if input != "" {
		name = input
	}

	if err := actions.EnsureConfigMapExists(ctx, namespace, name); err != nil {
		fmt.Fprintf(os.Stderr, "warning: failed to create ConfigMap %s: %v\n", name, err)
		return ""
	}
	if err := editConfigMap(ctx, namespace, name); err != nil {
		fmt.Fprintf(os.Stderr, "warning: kubectl edit failed: %v\n", err)
	}
	fmt.Printf("==> Edit it later any time via: kubectl edit configmap %s -n %s\n", name, namespace)
	return name
}

func adminArmaConfigCmd() *cobra.Command {
	var namespace, release string
	c := &cobra.Command{
		Use:   "armaconfig",
		Short: "Edit the cluster-wide Arma config baseline (main.cfg/basic.cfg defaults)",
		Long: "Opens the cluster-wide Arma config baseline ConfigMap in $EDITOR via `kubectl edit`, " +
			"the same flow magpiectl install/deploy offers right after a successful run. " +
			"See README.md's \"Arma server config\" section for the full field/placeholder reference.",
		RunE: func(c *cobra.Command, _ []string) error {
			return editBaselineConfigMap(c.Context(), namespace, release)
		},
	}
	c.Flags().StringVar(&namespace, "namespace", "magpie", "target namespace")
	c.Flags().StringVar(&release, "release", "arma", "helm release name")
	return c
}
