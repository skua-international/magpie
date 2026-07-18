package cmd

import (
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/spf13/cobra"

	"github.com/skua-international/magpie/cli/internal/auth"
)

func authCmd() *cobra.Command {
	root := &cobra.Command{Use: "auth", Short: "Manage your login session with the cluster"}
	root.AddCommand(authLoginCmd(), authLogoutCmd(), authStatusCmd())
	return root
}

func authLoginCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "login",
		Short: "Log in to the cluster via your browser",
		RunE: func(c *cobra.Command, _ []string) error {
			return runLogin(c)
		},
	}
}

func authLogoutCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "logout",
		Short: "Forget the locally stored session",
		RunE: func(_ *cobra.Command, _ []string) error {
			if err := auth.Clear(); err != nil {
				return err
			}
			fmt.Println("Logged out.")
			return nil
		},
	}
}

func authStatusCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "status",
		Short: "Show whether you're logged in",
		RunE: func(_ *cobra.Command, _ []string) error {
			creds, err := auth.Load()
			if errors.Is(err, auth.ErrNotLoggedIn) {
				fmt.Println("Not logged in to", identityURL)
				fmt.Println("Run `magpie auth login`.")
				return nil
			}
			if err != nil {
				return err
			}

			fmt.Println("Logged in to", identityURL)
			if claims, err := decodeJWTPayload(creds.AccessToken); err == nil {
				if sub, ok := claims["sub"].(string); ok {
					fmt.Println("  user:", sub)
				}
			}
			if time.Now().Before(creds.ExpiresAt) {
				fmt.Println("  access token valid for", time.Until(creds.ExpiresAt).Round(time.Second))
			} else {
				fmt.Println("  access token expired -- will refresh automatically on next use")
			}
			return nil
		},
	}
}

// decodeJWTPayload reads the claims out of an access token purely for
// display -- it does NOT verify the signature (nothing here needs to
// trust these claims, the server-side JWKS check is what actually
// matters; this is just so `auth status` can show who you're logged in
// as without a dedicated whoami RPC).
func decodeJWTPayload(token string) (map[string]any, error) {
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		return nil, fmt.Errorf("not a JWT")
	}
	raw, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return nil, err
	}
	var claims map[string]any
	if err := json.Unmarshal(raw, &claims); err != nil {
		return nil, err
	}
	return claims, nil
}
