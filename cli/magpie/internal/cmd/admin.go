package cmd

import (
	"bufio"
	"fmt"
	"os"
	"strings"
	"syscall"

	"github.com/spf13/cobra"
	"golang.org/x/term"

	"github.com/skua-international/magpie/cli/internal/actions"
)

func adminCmd() *cobra.Command {
	root := &cobra.Command{Use: "admin", Short: "Cluster administration"}
	root.AddCommand(adminDiskUsageCmd(), adminRefreshSteamAuthCmd())
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
			fmt.Printf("mods:      %s\n", humanBytes(usage.ModsBytes))
			fmt.Printf("missions:  %s\n", humanBytes(usage.MissionsBytes))
			fmt.Printf("game files: %s\n", humanBytes(usage.GameFilesBytes))
			fmt.Printf("total:     %s\n", humanBytes(usage.TotalBytes))
			return nil
		},
	}
}

// adminRefreshSteamAuthCmd is the "zero pre-existing Steam credentials
// anywhere in the cluster" bootstrap path (see identity/sync-daemon's own
// docs for the full flow) -- prompts for a username/password (password
// hidden, never echoed or logged) and, if Steam Guard confirmation is
// needed, prompts for the code and retries automatically.
func adminRefreshSteamAuthCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "refresh-steam-auth",
		Short: "Establish (or replace) the cluster's Steam session interactively",
		RunE: func(c *cobra.Command, _ []string) error {
			cl, err := clients(c.Context())
			if err != nil {
				return err
			}

			reader := bufio.NewReader(os.Stdin)
			fmt.Print("Steam username: ")
			username, err := reader.ReadString('\n')
			if err != nil {
				return err
			}
			username = strings.TrimSpace(username)

			password, err := readPassword("Steam password: ")
			if err != nil {
				return err
			}

			result, err := actions.RefreshSteamAuth(c.Context(), cl, username, password, "")
			if err != nil {
				return err
			}
			if result.NeedsGuard {
				fmt.Printf("Steam Guard (%s) code: ", result.GuardType)
				code, err := reader.ReadString('\n')
				if err != nil {
					return err
				}
				code = strings.TrimSpace(code)
				if _, err := actions.RefreshSteamAuth(c.Context(), cl, username, password, code); err != nil {
					return err
				}
			}

			fmt.Println("Steam session established. sync-daemon is restarting to pick it up.")
			return nil
		},
	}
}

func readPassword(prompt string) (string, error) {
	fmt.Print(prompt)
	bytes, err := term.ReadPassword(int(syscall.Stdin))
	fmt.Println()
	if err != nil {
		return "", err
	}
	return string(bytes), nil
}
