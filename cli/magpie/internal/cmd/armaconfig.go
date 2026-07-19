package cmd

import (
	"context"
	"fmt"
	"os"
	"os/exec"

	"github.com/spf13/cobra"
	"golang.org/x/term"
)

// baselineConfigMapName mirrors the chart's own naming
// (magpie.fullname-arma-config-baseline, see charts/magpie/templates/
// arma-config-baseline-configmap.yaml) -- fullname is just the Helm
// release name in this chart, so this needs no live cluster lookup.
func baselineConfigMapName(release string) string {
	return release + "-arma-config-baseline"
}

// editBaselineConfigMap shells out to `kubectl edit`, reusing its own
// $EDITOR handling and get/apply-on-save flow directly rather than
// hand-rolling a temp-file/diff/apply cycle -- the same reasoning
// `kubectl edit` itself exists for.
func editBaselineConfigMap(ctx context.Context, namespace, release string) error {
	cmd := exec.CommandContext(ctx, "kubectl", "edit", "configmap", baselineConfigMapName(release), "-n", namespace)
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
	name := baselineConfigMapName(release)
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
