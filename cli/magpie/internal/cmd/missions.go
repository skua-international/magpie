package cmd

import (
	"fmt"
	"time"

	"github.com/spf13/cobra"

	"github.com/skua-international/magpie/cli/internal/actions"
)

func missionsCmd() *cobra.Command {
	root := &cobra.Command{Use: "missions", Short: "Manage uploaded missions"}
	root.AddCommand(missionsListCmd(), missionsUploadCmd(), missionsDeleteCmd())
	return root
}

func missionsListCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "list",
		Short: "List every uploaded mission",
		RunE: func(c *cobra.Command, _ []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			missions, err := actions.ListMissions(c.Context(), cl)
			if err != nil {
				return err
			}
			if len(missions) == 0 {
				fmt.Println("No missions.")
				return nil
			}
			for _, m := range missions {
				age := time.Since(time.UnixMilli(m.CreatedAtUnixMs)).Round(time.Second)
				fmt.Printf("%-38s size=%-10s uploaded %s ago by %s  %s\n", m.Id, humanBytes(m.Filesize), age, m.CreatedBy, m.Name)
			}
			return nil
		},
	}
}

func missionsUploadCmd() *cobra.Command {
	var overwriteID string
	c := &cobra.Command{
		Use:   "upload <path-to.pbo>",
		Short: "Upload a mission",
		Args:  cobra.ExactArgs(1),
		RunE: func(cc *cobra.Command, args []string) error {
			cl, err := clients(cc.Context())
			if err != nil {
				return err
			}
			info, err := actions.UploadMission(cc.Context(), cl, args[0], overwriteID)
			if err != nil {
				return err
			}
			fmt.Println("uploaded", info.Id, info.Name)
			return nil
		},
	}
	c.Flags().StringVar(&overwriteID, "overwrite", "", "overwrite an existing mission's content in place instead of creating a new one")
	return c
}

func missionsDeleteCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "delete <id>",
		Short: "Delete a mission",
		Args:  cobra.ExactArgs(1),
		RunE: func(c *cobra.Command, args []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			if err := actions.DeleteMission(c.Context(), cl, args[0]); err != nil {
				return err
			}
			fmt.Println("deleted", args[0])
			return nil
		},
	}
}
