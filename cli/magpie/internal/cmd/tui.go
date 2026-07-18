package cmd

import (
	"context"

	tea "charm.land/bubbletea/v2"

	"github.com/skua-international/magpie/cli/internal/tui"
)

// runTUI is what a bare `magpie` invocation (no subcommand) launches --
// same authenticated clients as every direct subcommand, just handed to
// the Bubble Tea program instead of one action call.
func runTUI(ctx context.Context) error {
	cl, err := clients(ctx)
	if err != nil {
		return err
	}
	_, err = tea.NewProgram(tui.New(ctx, cl), tea.WithContext(ctx)).Run()
	return err
}
