package cmd

import (
	"fmt"

	"github.com/spf13/cobra"

	"github.com/skua-international/magpie/cli/internal/actions"
)

func modsCmd() *cobra.Command {
	root := &cobra.Command{Use: "mods", Short: "Manage mod sources"}
	root.AddCommand(
		modsListCmd(),
		modsAddCmd(),
		modsDeleteCmd(),
		modsSyncCmd(),
		modsListSyncedCmd(),
		modsInvalidateCmd(),
	)
	return root
}

func modsListCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "list",
		Short: "List registered mod sources",
		RunE: func(c *cobra.Command, _ []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			sources, err := actions.ListModSources(c.Context(), cl)
			if err != nil {
				return err
			}
			if len(sources) == 0 {
				fmt.Println("No mod sources.")
				return nil
			}
			for _, s := range sources {
				fmt.Printf("%-38s kind=%-11s size=%-10s %s\n", s.Id, s.Kind.String(), actions.HumanBytes(s.SizeBytes), actions.ModSourceLabel(s))
			}
			return nil
		},
	}
}

func modsAddCmd() *cobra.Command {
	var (
		steamURL  string
		presetURL string
		localHTML string
		localZip  string
		localID   string
	)
	c := &cobra.Command{
		Use:   "add",
		Short: "Register a mod source (exactly one of --steam-url, --preset-url, --local-html, --local-zip)",
		RunE: func(cc *cobra.Command, _ []string) error {
			cl, err := clients(cc.Context())
			if err != nil {
				return err
			}
			var id string
			switch {
			case steamURL != "":
				id, err = actions.AddModSourceSteamURL(cc.Context(), cl, steamURL)
			case presetURL != "":
				id, err = actions.AddModSourceHTMLURL(cc.Context(), cl, presetURL)
			case localHTML != "":
				id, err = actions.AddModSourceHTMLContent(cc.Context(), cl, localHTML)
			case localZip != "":
				if localID == "" {
					return fmt.Errorf("--local-id is required with --local-zip")
				}
				id, err = actions.AddModSourceLocalZip(cc.Context(), cl, localID, localZip)
			default:
				return fmt.Errorf("exactly one of --steam-url, --preset-url, --local-html, --local-zip is required")
			}
			if err != nil {
				return err
			}
			fmt.Println("added", id)
			return nil
		},
	}
	c.Flags().StringVar(&steamURL, "steam-url", "", "a single Steam Workshop URL (mod or collection)")
	c.Flags().StringVar(&presetURL, "preset-url", "", "URL to a preset HTML export")
	c.Flags().StringVar(&localHTML, "local-html", "", "path to a local preset HTML export file (uploaded directly, not fetched from a URL)")
	c.Flags().StringVar(&localZip, "local-zip", "", "path to a local mod's zip file")
	c.Flags().StringVar(&localID, "local-id", "", "stable unique ID for a --local-zip upload")
	return c
}

func modsDeleteCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "delete <id>",
		Short: "Delete a mod source",
		Args:  cobra.ExactArgs(1),
		RunE: func(c *cobra.Command, args []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			if err := actions.DeleteModSource(c.Context(), cl, args[0]); err != nil {
				return err
			}
			fmt.Println("deleted", args[0])
			return nil
		},
	}
}

func modsSyncCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "sync <id>",
		Short: "Force a Steam-backed source to re-resolve and kick off a content sync",
		Args:  cobra.ExactArgs(1),
		RunE: func(c *cobra.Command, args []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			if err := actions.SyncModSource(c.Context(), cl, args[0]); err != nil {
				return err
			}
			fmt.Println("sync started")
			return nil
		},
	}
}

func modsListSyncedCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "list-synced",
		Short: "List every currently-synced workshop mod",
		RunE: func(c *cobra.Command, _ []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			mods, err := actions.ListSyncedMods(c.Context(), cl)
			if err != nil {
				return err
			}
			if len(mods) == 0 {
				fmt.Println("No synced mods.")
				return nil
			}
			for _, m := range mods {
				fmt.Printf("%-12d size=%-10s %s\n", m.ModId, actions.HumanBytes(m.SizeBytes), m.Title)
			}
			return nil
		},
	}
}

func modsInvalidateCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "invalidate <mod-id>",
		Short: "Clear a mod's verification cache (never deletes its files) -- restricted scope",
		Args:  cobra.ExactArgs(1),
		RunE: func(c *cobra.Command, args []string) error {
			var modID uint64
			if _, err := fmt.Sscanf(args[0], "%d", &modID); err != nil {
				return fmt.Errorf("mod-id must be a number: %w", err)
			}
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			if err := actions.InvalidateMod(c.Context(), cl, modID); err != nil {
				return err
			}
			fmt.Println("invalidated", modID)
			return nil
		},
	}
}
