package cmd

import (
	"fmt"

	"github.com/spf13/cobra"

	"github.com/skua-international/magpie/cli/internal/actions"
	"github.com/skua-international/magpie/cli/internal/steamlogin"
)

func adminCmd() *cobra.Command {
	root := &cobra.Command{Use: "admin", Short: "Cluster administration"}
	root.AddCommand(adminDiskUsageCmd(), adminRefreshSteamAuthCmd(), adminArmaConfigCmd())
	return root
}

func adminDiskUsageCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "disk-usage",
		Short: "Show cluster-wide storage accounting",
		RunE: func(c *cobra.Command, _ []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			usage, err := actions.GetDiskUsage(c.Context(), cl)
			if err != nil {
				return err
			}
			fmt.Printf("mods:      %s\n", actions.HumanBytes(usage.ModsBytes))
			fmt.Printf("missions:  %s\n", actions.HumanBytes(usage.MissionsBytes))
			fmt.Printf("game files: %s\n", actions.HumanBytes(usage.GameFilesBytes))
			fmt.Printf("total:     %s\n", actions.HumanBytes(usage.TotalBytes))
			return nil
		},
	}
}

// adminRefreshSteamAuthCmd is the "zero Steam credentials anywhere in
// the cluster" bootstrap path. The QR-code login happens entirely on
// this machine, with no password ever typed in anywhere -- scan the
// printed QR code with the Steam account you want the cluster to use,
// and only the resulting refresh token is ever sent to the cluster.
func adminRefreshSteamAuthCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "refresh-steam-auth",
		Short: "Establish (or replace) the cluster's Steam session via QR-code login",
		RunE: func(c *cobra.Command, _ []string) error {
			ctx := c.Context()
			cl, err := clients(ctx)
			if err != nil {
				return err
			}

			result, err := steamlogin.Negotiate(ctx)
			if err != nil {
				return err
			}

			if err := actions.RefreshSteamAuth(ctx, cl, result.SteamUser, result.RefreshToken); err != nil {
				return err
			}

			fmt.Println("Steam session established. sync-daemon is restarting to pick it up.")
			return nil
		},
	}
}
