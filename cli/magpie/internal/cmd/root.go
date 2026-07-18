// Package cmd is the cobra command tree -- every subcommand here is a
// thin wrapper calling into internal/actions, the same functions the TUI
// (internal/tui) calls. `magpie` with no subcommand launches the TUI;
// everything else is a direct, scriptable invocation of one action.
package cmd

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"golang.org/x/term"

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
			"for managing servers, mod sources, and missions on a magpie cluster.\n\n" +
			"You log in to the cluster itself (identity is the cluster's own account system) -- " +
			"--provider only picks which external OAuth2/OIDC service vouches for who you are. " +
			"The cluster has no username/password of its own to avoid re-implementing what Steam/" +
			"Discord/GitHub/Google already do securely.",
		// Shell completions are hand-rolled in completion.go (with an
		// `install` subcommand cobra's default doesn't offer), so the
		// default completion command is disabled here to avoid a
		// name collision.
		CompletionOptions: cobra.CompletionOptions{DisableDefaultCmd: true},
		RunE: func(c *cobra.Command, _ []string) error {
			return runTUI(c.Context())
		},
	}

	root.PersistentFlags().StringVar(&identityURL, "identity-url", "http://identity.magpie.local", "base URL of the identity service")
	root.PersistentFlags().StringVar(&serverAPIURL, "server-api-url", "http://server-api.magpie.local", "base URL of server-api")
	root.PersistentFlags().StringVar(&registryURL, "registry-url", "http://registry.magpie.local", "base URL of registry")
	root.PersistentFlags().StringVar(&loginProvider, "provider", "steam", "login provider: steam, discord, github, or google (prompts interactively if omitted on a TTY)")

	root.AddCommand(loginCmd(), authCmd(), accountCmd(), serversCmd(), modsCmd(), missionsCmd(), adminCmd(), completionCmd())
	return root
}

// loginCmd is kept as a top-level shortcut for `magpie auth login` --
// same command, just less to type for the common case.
func loginCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "login",
		Short: "Log in to the cluster via your browser (shortcut for `magpie auth login`)",
		RunE: func(c *cobra.Command, _ []string) error {
			return runLogin(c)
		},
	}
}

// runLogin prompts for a provider first if the caller didn't pass
// --provider explicitly and stdin looks interactive -- matching `gh auth
// login`'s "which account do you want to log in with" prompt instead of
// silently defaulting to Steam every time.
func runLogin(c *cobra.Command) error {
	providerChanged := false
	if f := c.Root().PersistentFlags().Lookup("provider"); f != nil {
		providerChanged = f.Changed
	}
	if !providerChanged && term.IsTerminal(int(os.Stdin.Fd())) {
		chosen, err := promptProvider()
		if err != nil {
			return err
		}
		loginProvider = chosen
	}
	return doLogin(c.Context())
}

func promptProvider() (string, error) {
	options := []string{"steam", "discord", "github", "google"}
	fmt.Println("? Which account would you like to log in with?")
	for i, o := range options {
		fmt.Printf("  %d. %s\n", i+1, o)
	}
	fmt.Print("> ")
	line, err := bufio.NewReader(os.Stdin).ReadString('\n')
	if err != nil {
		return "", err
	}
	line = strings.TrimSpace(line)
	if n, err := strconv.Atoi(line); err == nil && n >= 1 && n <= len(options) {
		return options[n-1], nil
	}
	for _, o := range options {
		if strings.EqualFold(o, line) {
			return o, nil
		}
	}
	return "", fmt.Errorf("unrecognized provider %q", line)
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
