package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"time"

	"github.com/skua-international/magpie/cli/internal/auth"
)

func main() {
	baseURL := flag.String("identity-url", "http://identity.magpie.local", "base URL of the identity service")
	provider := flag.String("provider", "steam", "login provider: steam, discord, github, or google")
	flag.Parse()

	switch flag.Arg(0) {
	case "login":
		if err := runLogin(*baseURL, *provider); err != nil {
			fmt.Fprintln(os.Stderr, "login failed:", err)
			os.Exit(1)
		}
	default:
		if _, err := ensureCredentials(*baseURL, *provider); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		fmt.Println("logged in -- TUI not built yet")
	}
}

func runLogin(baseURL, provider string) error {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	defer cancel()

	fmt.Println("Opening browser to log in via", provider, "...")
	creds, err := auth.Login(ctx, baseURL, provider)
	if err != nil {
		return err
	}
	if err := auth.Save(creds); err != nil {
		return fmt.Errorf("logged in, but failed to save credentials: %w", err)
	}
	fmt.Println("Logged in.")
	return nil
}

// ensureCredentials loads a stored session, refreshing or triggering an
// interactive login as needed -- so a normal invocation only ever
// prompts for login the very first time, or once a refresh token has
// actually expired/been revoked.
func ensureCredentials(baseURL, provider string) (*auth.Credentials, error) {
	creds, err := auth.Load()
	switch {
	case errors.Is(err, auth.ErrNotLoggedIn):
		if runErr := runLogin(baseURL, provider); runErr != nil {
			return nil, runErr
		}
		return auth.Load()
	case err != nil:
		return nil, err
	}

	// Refresh a little ahead of actual expiry, not right at the edge.
	if time.Now().Add(30 * time.Second).Before(creds.ExpiresAt) {
		return creds, nil
	}

	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	fresh, err := auth.Refresh(ctx, baseURL, creds.RefreshToken)
	if err != nil {
		// The stored refresh token is dead -- fall back to a real login
		// rather than leaving the user stuck.
		if runErr := runLogin(baseURL, provider); runErr != nil {
			return nil, runErr
		}
		return auth.Load()
	}
	if saveErr := auth.Save(fresh); saveErr != nil {
		return nil, fmt.Errorf("refreshed session, but failed to save it: %w", saveErr)
	}
	return fresh, nil
}
