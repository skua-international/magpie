package cmd

import (
	"bufio"
	"context"
	"fmt"
	"os"
	"os/exec"
	"strconv"
	"strings"

	"github.com/spf13/cobra"

	"github.com/skua-international/magpie/cli/internal/config"
)

// targetCmd lets the user pick a cluster once and save it, so every
// other magpiectl invocation defaults to it -- see defaults.go for how
// the saved file feeds back into every flag's default value. Deliberately
// a plain numbered stdin prompt (bufio.Reader), matching promptProvider
// above rather than reaching for Bubble Tea: this only ever runs once in
// a blue moon (on setup, or when switching clusters), so a Bubble Tea
// picker would be more machinery than the interaction warrants.
func targetCmd() *cobra.Command {
	c := &cobra.Command{
		Use:   "target",
		Short: "Pick and save the cluster this magpiectl targets by default",
		Long: "Interactively pick a kubeconfig context (informational only -- see below) and enter " +
			"the cluster's identity/server-api/registry URLs plus namespace/release, then save all of " +
			"it to magpiectl's config dir. Every subsequent invocation (any subcommand, and the bare " +
			"TUI) uses the saved values as its default -- no more --identity-url/--namespace/etc on " +
			"every command. An explicit flag or env var still always wins over the saved target.\n\n" +
			"The kubeconfig context is saved for display only (`magpiectl target show`) -- magpiectl " +
			"itself never switches kubeconfig context; the one already active in your shell " +
			"(KUBECONFIG/current-context) is what `servers create`'s ConfigMap flow actually uses.",
		RunE: func(cc *cobra.Command, _ []string) error {
			return runTargetPicker(cc.Context())
		},
	}
	c.AddCommand(targetShowCmd())
	return c
}

func targetShowCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "show",
		Short: "Print the currently saved target, if any",
		RunE: func(cc *cobra.Command, _ []string) error {
			t, err := config.LoadTarget()
			if err != nil {
				return err
			}
			if *t == (config.Target{}) {
				fmt.Println("No target saved -- run `magpiectl target` to pick one. " +
					"Using built-in/env-var defaults until then.")
				return nil
			}
			if t.Context != "" {
				fmt.Println("kubeconfig context:", t.Context)
			}
			fmt.Println("identity-url:  ", t.IdentityURL)
			fmt.Println("server-api-url:", t.ServerAPIURL)
			fmt.Println("registry-url:  ", t.RegistryURL)
			fmt.Println("namespace:     ", t.Namespace)
			fmt.Println("release:       ", t.Release)
			return nil
		},
	}
}

func runTargetPicker(_ context.Context) error {
	existing, err := config.LoadTarget()
	if err != nil {
		return err
	}

	contexts, err := kubeContexts()
	if err != nil {
		// kubectl not on PATH, or no kubeconfig -- non-fatal, the context
		// field is informational only (see targetCmd's Long).
		fmt.Fprintln(os.Stderr, "warning: couldn't list kubeconfig contexts:", err)
	}

	reader := bufio.NewReader(os.Stdin)
	chosenContext := existing.Context
	if len(contexts) > 0 {
		fmt.Println("? Which kubeconfig context is this cluster (for reference only, doesn't switch anything)?")
		for i, ctxName := range contexts {
			fmt.Printf("  %d. %s\n", i+1, ctxName)
		}
		fmt.Println("  0. (none / skip)")
		fmt.Printf("> ")
		line, err := reader.ReadString('\n')
		if err != nil {
			return err
		}
		line = strings.TrimSpace(line)
		if n, err := strconv.Atoi(line); err == nil && n >= 1 && n <= len(contexts) {
			chosenContext = contexts[n-1]
		}
	}

	t := &config.Target{
		Context:      chosenContext,
		IdentityURL:  promptWithDefault(reader, "identity-url", firstNonEmpty(existing.IdentityURL, identityURL)),
		ServerAPIURL: promptWithDefault(reader, "server-api-url", firstNonEmpty(existing.ServerAPIURL, serverAPIURL)),
		RegistryURL:  promptWithDefault(reader, "registry-url", firstNonEmpty(existing.RegistryURL, registryURL)),
		Namespace:    promptWithDefault(reader, "namespace", firstNonEmpty(existing.Namespace, namespace)),
		Release:      promptWithDefault(reader, "release (helm)", firstNonEmpty(existing.Release, release)),
	}

	if err := config.SaveTarget(t); err != nil {
		return fmt.Errorf("failed to save target: %w", err)
	}
	fmt.Println("Saved. Future magpiectl invocations default to this target.")
	return nil
}

func promptWithDefault(reader *bufio.Reader, label, def string) string {
	fmt.Printf("%s [%s]: ", label, def)
	line, err := reader.ReadString('\n')
	if err != nil {
		return def
	}
	line = strings.TrimSpace(line)
	if line == "" {
		return def
	}
	return line
}

// kubeContexts shells out to kubectl rather than pulling in client-go
// just to parse a kubeconfig -- consistent with the rest of this CLI,
// which already treats kubectl as an external dependency (see
// internal/actions/configmap.go) instead of linking a Kubernetes client
// library.
func kubeContexts() ([]string, error) {
	out, err := exec.Command("kubectl", "config", "get-contexts", "-o", "name").Output()
	if err != nil {
		return nil, err
	}
	var contexts []string
	for line := range strings.SplitSeq(strings.TrimSpace(string(out)), "\n") {
		if line = strings.TrimSpace(line); line != "" {
			contexts = append(contexts, line)
		}
	}
	return contexts, nil
}
