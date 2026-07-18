// Command magpie is both a direct CLI (`magpie servers list`, `magpie
// login`, ...) and, invoked with no subcommand, an interactive Bubble Tea
// TUI -- both are thin wrappers around internal/actions, so there's
// exactly one implementation of what each action against the cluster
// actually does.
package main

import (
	"os"

	"github.com/skua-international/magpie/cli/internal/cmd"
)

func main() {
	// cobra prints the error itself by default (Execute's own job) --
	// just need to set a real exit code on failure.
	if err := cmd.Root().Execute(); err != nil {
		os.Exit(1)
	}
}
