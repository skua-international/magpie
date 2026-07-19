package cmd

import (
	"fmt"

	"github.com/spf13/cobra"

	"github.com/skua-international/magpie/cli/internal/actions"
	"github.com/skua-international/magpie/cli/internal/steamlogin"
)

func adminCmd() *cobra.Command {
	root := &cobra.Command{Use: "admin", Short: "Cluster administration"}
	root.AddCommand(
		adminDiskUsageCmd(),
		adminRefreshSteamAuthCmd(),
		adminArmaConfigCmd(),
		adminExportStateCmd(),
		adminImportStateCmd(),
	)
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

// adminExportStateCmd/adminImportStateCmd move exactly what
// registry.v1.AdminService.ExportState/ImportState cover -- mod source
// registrations, ConfigMaps, ArmaServer specs -- between clusters. See
// that RPC's own proto doc for what's deliberately excluded (Postgres
// data, synced mod/mission file content, ACL grants, live credentials)
// and why.
func adminExportStateCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "export-state <file>",
		Short: "Export mod sources, ConfigMaps, and server specs to a JSON file",
		Args:  cobra.ExactArgs(1),
		RunE: func(c *cobra.Command, args []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			state, err := actions.ExportState(c.Context(), cl)
			if err != nil {
				return err
			}
			if err := actions.WriteStateFile(args[0], state); err != nil {
				return err
			}
			fmt.Printf("exported %d mod source(s), %d ConfigMap(s), %d server(s) to %s\n",
				len(state.ModSources), len(state.ConfigMaps), len(state.Servers), args[0])
			for _, w := range state.Warnings {
				fmt.Println("warning:", w)
			}
			return nil
		},
	}
}

func adminImportStateCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "import-state <file>",
		Short: "Import a file written by export-state",
		Long: "Re-creates whatever export-state produced. Idempotent per item, not transactional: " +
			"each mod source/ConfigMap/server is applied independently, and one failing doesn't roll " +
			"back the others -- see the printed warnings for exactly what was skipped and why (a " +
			"`local`-kind mod source, whose file content was never part of the export, is the common one).",
		Args: cobra.ExactArgs(1),
		RunE: func(c *cobra.Command, args []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			req, err := actions.ReadStateFile(args[0])
			if err != nil {
				return err
			}
			resp, err := actions.ImportState(c.Context(), cl, req)
			if err != nil {
				return err
			}
			fmt.Println("import complete.")
			for _, w := range resp.Warnings {
				fmt.Println("warning:", w)
			}
			return nil
		},
	}
}
