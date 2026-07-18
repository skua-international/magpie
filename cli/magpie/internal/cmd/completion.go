package cmd

import (
	"bytes"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"

	"github.com/spf13/cobra"
)

const installMarker = "# added by `magpiectl completion install`"

func completionCmd() *cobra.Command {
	root := &cobra.Command{
		Use:   "completion [bash|zsh|fish|powershell]",
		Short: "Print or install shell autocompletion",
		Long: "Print the autocompletion script for the given shell to stdout, or run\n" +
			"`magpiectl completion install` to detect your shell and install it automatically.",
		DisableFlagsInUseLine: true,
		ValidArgs:             []string{"bash", "zsh", "fish", "powershell"},
		Args:                  cobra.MatchAll(cobra.MaximumNArgs(1), cobra.OnlyValidArgs),
		RunE: func(c *cobra.Command, args []string) error {
			if len(args) == 0 {
				return c.Help()
			}
			return generateTo(c.Root(), args[0], os.Stdout)
		},
	}
	root.AddCommand(completionInstallCmd())
	return root
}

func generateTo(root *cobra.Command, shell string, w io.Writer) error {
	switch shell {
	case "bash":
		return root.GenBashCompletionV2(w, true)
	case "zsh":
		return root.GenZshCompletion(w)
	case "fish":
		return root.GenFishCompletion(w, true)
	case "powershell":
		return root.GenPowerShellCompletionWithDesc(w)
	default:
		return fmt.Errorf("unsupported shell %q", shell)
	}
}

func completionInstallCmd() *cobra.Command {
	return &cobra.Command{
		Use:   "install",
		Short: "Detect your shell and install its completion script automatically",
		RunE: func(c *cobra.Command, _ []string) error {
			shell := detectShell()
			if shell == "" {
				return fmt.Errorf("couldn't detect your shell from $SHELL -- run " +
					"`magpiectl completion <bash|zsh|fish|powershell>` and install it manually")
			}
			switch shell {
			case "bash":
				return installBash(c.Root())
			case "zsh":
				return installZsh(c.Root())
			case "fish":
				return installFish(c.Root())
			case "powershell":
				return installPowerShell(c.Root())
			default:
				return fmt.Errorf("unrecognized shell %q", shell)
			}
		},
	}
}

// detectShell prefers $SHELL (set by the login shell itself, so it's
// accurate even when invoked from e.g. a script or IDE terminal), and
// falls back to looking for pwsh/powershell on PATH for Windows/WSL
// users where $SHELL is often unset.
func detectShell() string {
	base := filepath.Base(os.Getenv("SHELL"))
	switch {
	case strings.Contains(base, "bash"):
		return "bash"
	case strings.Contains(base, "zsh"):
		return "zsh"
	case strings.Contains(base, "fish"):
		return "fish"
	}
	if runtime.GOOS == "windows" {
		return "powershell"
	}
	if _, err := exec.LookPath("pwsh"); err == nil {
		return "powershell"
	}
	return ""
}

// bash-completion v2's dynamic-loading convention: any interactive bash
// that sources /usr/share/bash-completion/bash_completion (the default
// on effectively every distro) lazily loads completions from here with
// no rc-file edits needed at all.
func bashCompletionDir() string {
	if xdg := os.Getenv("XDG_DATA_HOME"); xdg != "" {
		return filepath.Join(xdg, "bash-completion", "completions")
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".local", "share", "bash-completion", "completions")
}

func installBash(root *cobra.Command) error {
	dir := bashCompletionDir()
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return fmt.Errorf("creating %s: %w", dir, err)
	}
	path := filepath.Join(dir, "magpiectl")
	if err := writeCompletionFile(root, "bash", path); err != nil {
		return err
	}
	fmt.Printf("Installed bash completion to %s\n", path)
	fmt.Println("Picked up automatically by new shells (needs bash-completion installed, the default on most distros). Run `exec bash` to use it now.")
	return nil
}

// fish auto-loads anything dropped in its completions dir, same as bash
// -- no rc-file edits needed.
func installFish(root *cobra.Command) error {
	dir := filepath.Join(os.Getenv("XDG_CONFIG_HOME"), "fish", "completions")
	if os.Getenv("XDG_CONFIG_HOME") == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return err
		}
		dir = filepath.Join(home, ".config", "fish", "completions")
	}
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return fmt.Errorf("creating %s: %w", dir, err)
	}
	path := filepath.Join(dir, "magpiectl.fish")
	if err := writeCompletionFile(root, "fish", path); err != nil {
		return err
	}
	fmt.Printf("Installed fish completion to %s\n", path)
	fmt.Println("Picked up automatically by new shells.")
	return nil
}

// installZsh first asks the user's actual interactive zsh for its real
// $fpath (so oh-my-zsh/prezto/etc's own setup is respected) and, if any
// entry under $HOME is writable, drops the completion there with no
// rc-file edits at all. Only falls back to creating a directory and
// wiring it into ~/.zshrc (idempotently, guarded by installMarker) if
// nothing usable was already on the path.
func installZsh(root *cobra.Command) error {
	home, err := os.UserHomeDir()
	if err != nil {
		return err
	}

	if dir := findWritableFpathDir(home); dir != "" {
		path := filepath.Join(dir, "_magpiectl")
		if err := writeCompletionFile(root, "zsh", path); err != nil {
			return err
		}
		fmt.Printf("Installed zsh completion to %s (already on your $fpath).\n", path)
		fmt.Println("Run `exec zsh` to use it now.")
		return nil
	}

	dir := filepath.Join(home, ".zsh", "completions")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return fmt.Errorf("creating %s: %w", dir, err)
	}
	path := filepath.Join(dir, "_magpiectl")
	if err := writeCompletionFile(root, "zsh", path); err != nil {
		return err
	}

	rcPath := filepath.Join(home, ".zshrc")
	block := fmt.Sprintf("\n%s\nfpath=(%s $fpath)\nautoload -Uz compinit && compinit\n", installMarker, dir)
	added, err := appendOnceGuarded(rcPath, block)
	if err != nil {
		return fmt.Errorf("installed completion to %s, but failed to update %s: %w", path, rcPath, err)
	}
	fmt.Printf("Installed zsh completion to %s\n", path)
	if added {
		fmt.Printf("Added fpath/compinit lines to %s -- run `exec zsh` to use it now.\n", rcPath)
	} else {
		fmt.Println("Run `exec zsh` to use it now.")
	}
	return nil
}

func findWritableFpathDir(home string) string {
	out, err := exec.Command("zsh", "-ic", "echo -n $fpath").Output()
	if err != nil {
		return ""
	}
	for _, dir := range strings.Fields(string(out)) {
		if !strings.HasPrefix(dir, home) {
			continue
		}
		probe := filepath.Join(dir, ".magpiectl-write-test")
		f, err := os.OpenFile(probe, os.O_CREATE|os.O_WRONLY, 0o644)
		if err != nil {
			continue
		}
		f.Close()
		os.Remove(probe)
		return dir
	}
	return ""
}

// installPowerShell needs a live pwsh/powershell on PATH to ask for
// $PROFILE's real location -- there's no portable static convention like
// bash/zsh/fish have, so best-effort only.
func installPowerShell(root *cobra.Command) error {
	pwsh, err := exec.LookPath("pwsh")
	if err != nil {
		pwsh, err = exec.LookPath("powershell")
	}
	if err != nil {
		return fmt.Errorf("no pwsh/powershell found on PATH -- run " +
			"`magpiectl completion powershell > magpiectl-completion.ps1` and dot-source it from your $PROFILE manually")
	}

	out, err := exec.Command(pwsh, "-NoProfile", "-Command", "Write-Output $PROFILE").Output()
	if err != nil {
		return fmt.Errorf("failed to ask %s for $PROFILE: %w", pwsh, err)
	}
	profilePath := strings.TrimSpace(string(out))
	if profilePath == "" {
		return fmt.Errorf("%s returned an empty $PROFILE path", pwsh)
	}

	scriptDir := filepath.Dir(profilePath)
	if err := os.MkdirAll(scriptDir, 0o755); err != nil {
		return fmt.Errorf("creating %s: %w", scriptDir, err)
	}
	scriptPath := filepath.Join(scriptDir, "magpiectl-completion.ps1")
	if err := writeCompletionFile(root, "powershell", scriptPath); err != nil {
		return err
	}

	block := fmt.Sprintf("\n%s\n. \"%s\"\n", installMarker, scriptPath)
	added, err := appendOnceGuarded(profilePath, block)
	if err != nil {
		return fmt.Errorf("installed %s, but failed to update %s: %w", scriptPath, profilePath, err)
	}
	fmt.Printf("Installed completion script to %s\n", scriptPath)
	if added {
		fmt.Printf("Wired it into %s -- restart PowerShell to use it now.\n", profilePath)
	} else {
		fmt.Println("Restart PowerShell to use it now.")
	}
	return nil
}

func writeCompletionFile(root *cobra.Command, shell, path string) error {
	f, err := os.Create(path)
	if err != nil {
		return fmt.Errorf("writing %s: %w", path, err)
	}
	defer f.Close()
	return generateTo(root, shell, f)
}

// appendOnceGuarded appends block to path unless installMarker is
// already present, so repeated `magpiectl completion install` runs don't
// pile up duplicate lines.
func appendOnceGuarded(path, block string) (bool, error) {
	existing, err := os.ReadFile(path)
	if err != nil && !os.IsNotExist(err) {
		return false, err
	}
	if bytes.Contains(existing, []byte(installMarker)) {
		return false, nil
	}
	f, err := os.OpenFile(path, os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o644)
	if err != nil {
		return false, err
	}
	defer f.Close()
	_, err = f.WriteString(block)
	return true, err
}
