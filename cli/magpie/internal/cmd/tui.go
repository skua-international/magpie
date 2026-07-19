package cmd

import (
	"context"

	tea "charm.land/bubbletea/v2"

	"github.com/skua-international/magpie/cli/internal/client"
	"github.com/skua-international/magpie/cli/internal/tui"
)

// runTUI is what a bare `magpie` invocation (no subcommand) launches --
// same authenticated clients as every direct subcommand, just handed to
// the Bubble Tea program instead of one action call.
func runTUI(ctx context.Context) error {
	// Not just clients(ctx): the "Account" screen's link flow needs the
	// raw access token directly (identity reads it to learn which
	// existing user to link to, same as `account link` -- see
	// auth.LinkAccount's own doc), which client.Clients doesn't expose.
	creds, err := ensureCredentials(ctx)
	if err != nil {
		return err
	}
	cl := client.New(client.Config{ServerAPIURL: serverAPIURL, RegistryURL: registryURL}, creds.AccessToken)
	// "magpie" matches every other subcommand's own --namespace default
	// (servers.go, armaconfig.go) -- not configurable here yet since the
	// bare TUI entrypoint takes no flags of its own at all. The access
	// token is a point-in-time snapshot -- if it expires mid-session, the
	// Account screen's link flow just fails and the operator restarts
	// the TUI, same as any other subcommand's session ultimately would.
	_, err = tea.NewProgram(tui.New(ctx, cl, "magpie", identityURL, creds.AccessToken), tea.WithContext(ctx)).Run()
	return err
}
