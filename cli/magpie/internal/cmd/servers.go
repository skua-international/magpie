package cmd

import (
	"fmt"

	"github.com/spf13/cobra"

	"github.com/skua-international/magpie/cli/internal/actions"
)

func serversCmd() *cobra.Command {
	root := &cobra.Command{Use: "servers", Short: "Manage ArmaServer instances"}
	root.AddCommand(
		serversListCmd(),
		serversCreateCmd(),
		serversDeleteCmd(),
		serversStartCmd(),
		serversStopCmd(),
		serversUpdateCmd(),
	)
	return root
}

func serversListCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "list",
		Short: "List every server",
		RunE: func(c *cobra.Command, _ []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			servers, err := actions.ListServers(c.Context(), cl)
			if err != nil {
				return err
			}
			if len(servers) == 0 {
				fmt.Println("No servers.")
				return nil
			}
			for _, s := range servers {
				fmt.Printf("%-20s port=%-5d phase=%-12s desired=%-10s mods=%d\n",
					s.Id, s.Port, s.Phase.String(), s.DesiredState.String(), len(s.ModSourceIds))
			}
			return nil
		},
	}
}

func serversCreateCmd() *cobra.Command {
	var (
		port         uint32
		modSourceIDs []string
	)
	c := &cobra.Command{
		Use:   "create <name>",
		Short: "Create and start a new server",
		Args:  cobra.ExactArgs(1),
		RunE: func(cc *cobra.Command, args []string) error {
			cl, err := clients(cc.Context())
			if err != nil {
				return err
			}
			info, err := actions.CreateServer(cc.Context(), cl, actions.CreateServerParams{
				Name:         args[0],
				Port:         port,
				ModSourceIDs: modSourceIDs,
			})
			if err != nil {
				return err
			}
			fmt.Printf("created %s (port %d)\n", info.Id, info.Port)
			return nil
		},
	}
	c.Flags().Uint32Var(&port, "port", 2302, "base game port (Arma binds a 5-port range starting here)")
	c.Flags().StringSliceVar(&modSourceIDs, "mod-source", nil, "mod source ID (repeatable)")
	return c
}

func serversDeleteCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "delete <id>",
		Short: "Delete a server",
		Args:  cobra.ExactArgs(1),
		RunE: func(c *cobra.Command, args []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			if err := actions.DeleteServer(c.Context(), cl, args[0]); err != nil {
				return err
			}
			fmt.Println("deleted", args[0])
			return nil
		},
	}
}

func serversStartCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "start <id>",
		Short: "Start a stopped server (or force-resync content onto a running one)",
		Args:  cobra.ExactArgs(1),
		RunE: func(c *cobra.Command, args []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			info, err := actions.StartServer(c.Context(), cl, args[0])
			if err != nil {
				return err
			}
			fmt.Println("phase:", info.Phase.String())
			return nil
		},
	}
}

func serversStopCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "stop <id>",
		Short: "Stop a server",
		Args:  cobra.ExactArgs(1),
		RunE: func(c *cobra.Command, args []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			info, err := actions.StopServer(c.Context(), cl, args[0])
			if err != nil {
				return err
			}
			fmt.Println("phase:", info.Phase.String())
			return nil
		},
	}
}

func serversUpdateCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "update <id>",
		Short: "Force a resync of a running server's mod sources",
		Args:  cobra.ExactArgs(1),
		RunE: func(c *cobra.Command, args []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}
			info, err := actions.UpdateServer(c.Context(), cl, args[0])
			if err != nil {
				return err
			}
			fmt.Println("phase:", info.Phase.String())
			return nil
		},
	}
}
