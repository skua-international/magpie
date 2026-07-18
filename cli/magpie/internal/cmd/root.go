// Package cmd is the cobra command tree -- every subcommand here is a
// thin wrapper calling into internal/actions, the same functions the TUI
// (internal/tui) calls. `magpie` with no subcommand launches the TUI;
// everything else is a direct, scriptable invocation of one action.
package cmd

import (
	"context"
	"errors"
	"fmt"
	"time"

	"github.com/spf13/cobra"

	"github.com/skua-international/magpie/cli/internal/auth"
	"github.com/skua-international/magpie/cli/internal/client"
)

var (
	identityURL   string
	serverAPIURL  string
	registryURL   string
	loginProvider string
)

func Root() *cobra.Command {
	root := &cobra.Command{
		Use:   "magpie",
		Short: "Manage a magpie-orchestrated Arma 3 cluster",
		Long: "magpie is both a direct CLI and, with no subcommand, an interactive TUI " +
			"for managing servers, mod sources, and missions on a magpie cluster.",
		RunE: func(c *cobra.Command, _ []string) error {
			return runTUI(c.Context())
		},
	}

	root.PersistentFlags().StringVar(&identityURL, "identity-url", "http://identity.magpie.local", "base URL of the identity service")
	root.PersistentFlags().StringVar(&serverAPIURL, "server-api-url", "http://server-api.magpie.local", "base URL of server-api")
	root.PersistentFlags().StringVar(&registryURL, "registry-url", "http://registry.magpie.local", "base URL of registry")
	root.PersistentFlags().StringVar(&loginProvider, "provider", "steam", "login provider: steam, discord, github, or google")

	root.AddCommand(loginCmd(), accountCmd(), serversCmd(), modsCmd(), missionsCmd(), adminCmd())
	return root
}

func loginCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "login",
		Short: "Log in via browser and save the session",
		RunE: func(c *cobra.Command, _ []string) error {
			return doLogin(c.Context())
		},
	}
}

func doLogin(ctx context.Context) error {
	loginCtx, cancel := context.WithTimeout(ctx, 5*time.Minute)
	defer cancel()

	fmt.Println("Opening browser to log in via", loginProvider, "...")
	creds, err := auth.Login(loginCtx, identityURL, loginProvider)
	if err != nil {
		return err
	}
	if err := auth.Save(creds); err != nil {
		return fmt.Errorf("logged in, but failed to save credentials: %w", err)
	}
	fmt.Println("Logged in.")
	return nil
}

// clients loads (refreshing/logging in as needed -- see auth.go's own
// doc) a session and returns authenticated clients for every subcommand
// besides `login` itself to use.
func clients(ctx context.Context) (*client.Clients, error) {
	creds, err := ensureCredentials(ctx)
	if err != nil {
		return nil, err
	}
	return client.New(client.Config{ServerAPIURL: serverAPIURL, RegistryURL: registryURL}, creds.AccessToken), nil
}

// ensureCredentials loads a stored session, refreshing it or triggering
// an interactive login as needed, so a normal invocation only ever
// prompts the very first time or once a refresh token has actually
// expired/been revoked.
func ensureCredentials(ctx context.Context) (*auth.Credentials, error) {
	creds, err := auth.Load()
	switch {
	case errors.Is(err, auth.ErrNotLoggedIn):
		if loginErr := doLogin(ctx); loginErr != nil {
			return nil, loginErr
		}
		return auth.Load()
	case err != nil:
		return nil, err
	}

	if time.Now().Add(30 * time.Second).Before(creds.ExpiresAt) {
		return creds, nil
	}

	refreshCtx, cancel := context.WithTimeout(ctx, 30*time.Second)
	defer cancel()
	fresh, err := auth.Refresh(refreshCtx, identityURL, creds.RefreshToken)
	if err != nil {
		// The stored refresh token is dead -- fall back to a real login
		// rather than leaving the caller stuck.
		if loginErr := doLogin(ctx); loginErr != nil {
			return nil, loginErr
		}
		return auth.Load()
	}
	if saveErr := auth.Save(fresh); saveErr != nil {
		return nil, fmt.Errorf("refreshed session, but failed to save it: %w", saveErr)
	}
	return fresh, nil
}
