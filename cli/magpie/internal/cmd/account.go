package cmd

import (
	"fmt"

	"github.com/spf13/cobra"

	"github.com/skua-international/magpie/cli/internal/auth"
)

func accountCmd() *cobra.Command {
	root := &cobra.Command{Use: "account", Short: "Manage the logged-in account"}
	root.AddCommand(accountLinkCmd())
	return root
}

// accountLinkCmd attaches an additional OAuth2/OIDC provider to the
// already-logged-in account. If that provider identity already belongs
// to a different, previously-separate account, identity merges the two
// (scopes unioned, the other account deleted) rather than erroring --
// see registry-db's link_account_to_user for the merge semantics.
func accountLinkCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "link <provider>",
		Short: "Link another login provider (steam, discord, github, google) to this account",
		Args:  cobra.ExactArgs(1),
		RunE: func(c *cobra.Command, args []string) error {
			creds, err := ensureCredentials(c.Context())
			if err != nil {
				return err
			}
			fmt.Println("Opening browser to link", args[0], "...")
			fresh, err := auth.LinkAccount(c.Context(), apiURL, args[0], creds.AccessToken)
			if err != nil {
				return err
			}
			if err := auth.Save(fresh); err != nil {
				return fmt.Errorf("linked, but failed to save the refreshed session: %w", err)
			}
			fmt.Println("Linked.")
			return nil
		},
	}
}
