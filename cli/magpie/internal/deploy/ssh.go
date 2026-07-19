package deploy

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// RunRemoteInstall runs `magpiectl install` on a remote host over ssh
// instead of locally. Deliberately does NOT scp over the binary
// currently running this process -- that binary matches whatever
// platform/arch is controlling this session, which has no guaranteed
// relationship to the remote target's (a Windows or macOS/arm64 machine
// driving a Linux/amd64 VM is exactly the normal case here, not an edge
// case). k3s itself only targets Linux, so the remote OS is always
// "linux"; only the architecture needs detecting (via `uname -m` on the
// remote host itself), then the matching magpiectl_linux_<arch> release
// asset is downloaded there directly -- same private-repo GitHub API
// dance internal/steamlogin uses for its own binary, just resolving
// magpiectl's own asset instead of steam-login's.
//
// Once the remote install finishes, the resulting kubeconfig is fetched
// back and saved locally (server URL rewritten from 127.0.0.1/localhost
// to the real host) so kubectl/helm/magpiectl can target the new
// cluster directly afterward, without an SSH session for every command
// -- same contract deploy/k3s-bootstrap.sh's own --ssh already has.
// Assumes the ssh target already has root or passwordless sudo -- no
// TTY for an interactive sudo password prompt over a non-interactive
// ssh command.
func RunRemoteInstall(ctx context.Context, sshHost, sshIdentity, version string, rawArgs []string) error {
	sshArgs := []string{"-o", "BatchMode=yes"}
	if sshIdentity != "" {
		sshArgs = append(sshArgs, "-i", sshIdentity)
	}

	token, err := ghToken()
	if err != nil {
		return err
	}

	fmt.Printf("==> Detecting %s's architecture...\n", sshHost)
	archOut, err := exec.CommandContext(ctx, "ssh", append(append([]string{}, sshArgs...), sshHost, "uname -m")...).Output()
	if err != nil {
		return fmt.Errorf("failed to detect %s's architecture: %w", sshHost, err)
	}
	arch := archToGoArch(strings.TrimSpace(string(archOut)))
	assetName := fmt.Sprintf("magpiectl_linux_%s.tar.gz", arch)

	tag := "v" + version
	fmt.Printf("==> Resolving %s from release %s...\n", assetName, tag)
	assetURL, err := releaseAssetURL(ctx, tag, assetName, token)
	if err != nil {
		return err
	}

	// A single remote shell command, not scp+ssh separately: downloads
	// the correct-platform magpiectl straight to the remote host (never
	// transits through this machine at all), runs it with the same args
	// this invocation got (minus --ssh/--ssh-identity, which would
	// otherwise just try to ssh again from inside the remote run), and
	// cleans up after itself regardless of exit status.
	remoteScript := fmt.Sprintf(
		`set -e; curl -sSf -H "Authorization: token %s" -H "Accept: application/octet-stream" -L %s -o /tmp/magpiectl.tar.gz && tar xzf /tmp/magpiectl.tar.gz -C /tmp magpiectl && chmod +x /tmp/magpiectl; rc=$?; if [ $rc -eq 0 ]; then /tmp/magpiectl %s; rc=$?; fi; rm -f /tmp/magpiectl /tmp/magpiectl.tar.gz; exit $rc`,
		token, shellSingleQuote(assetURL), shellJoin(filterSSHArgs(rawArgs)),
	)

	fmt.Printf("==> Installing on %s...\n", sshHost)
	sshRunArgs := append(append([]string{}, sshArgs...), sshHost, remoteScript)
	if err := run(ctx, "ssh", sshRunArgs...); err != nil {
		return fmt.Errorf("remote install failed: %w", err)
	}

	fmt.Printf("==> Fetching kubeconfig from %s...\n", sshHost)
	catArgs := append(append([]string{}, sshArgs...), sshHost, `cat "$HOME/.kube/config"`)
	kubeconfig, err := exec.CommandContext(ctx, "ssh", catArgs...).Output()
	if err != nil {
		return fmt.Errorf("failed to fetch kubeconfig from %s: %w", sshHost, err)
	}

	remoteHost := sshHost
	if idx := strings.Index(remoteHost, "@"); idx >= 0 {
		remoteHost = remoteHost[idx+1:]
	}
	rewritten := strings.NewReplacer(
		"https://127.0.0.1:", "https://"+remoteHost+":",
		"https://localhost:", "https://"+remoteHost+":",
	).Replace(string(kubeconfig))

	home, err := os.UserHomeDir()
	if err != nil {
		return err
	}
	kubeDir := filepath.Join(home, ".kube")
	if err := os.MkdirAll(kubeDir, 0o755); err != nil {
		return err
	}
	safeName := strings.NewReplacer(":", "-", ".", "-").Replace(remoteHost)
	localPath := filepath.Join(kubeDir, "config-"+safeName)
	if err := os.WriteFile(localPath, []byte(rewritten), 0o600); err != nil {
		return err
	}
	fmt.Printf("==> kubeconfig saved to %s -- export KUBECONFIG=%s to use it locally\n", localPath, localPath)
	return nil
}

// archToGoArch maps `uname -m`'s output to the Go/goreleaser arch names
// magpiectl's own release assets are named with (see cli/magpie/
// .goreleaser.yaml) -- x86_64/amd64 and aarch64/arm64 are the only two
// magpiectl actually ships for.
func archToGoArch(unameM string) string {
	switch unameM {
	case "aarch64", "arm64":
		return "arm64"
	default:
		return "amd64"
	}
}

// releaseAssetURL looks up a GitHub release by tag and returns the API
// asset URL (not the plain download URL, which 404s for a private repo
// without a browser session) for the named asset -- same dance
// internal/steamlogin.download uses for its own binary.
func releaseAssetURL(ctx context.Context, tag, assetName, token string) (string, error) {
	out, err := runCaptured(ctx, "curl", "-sSf",
		"-H", "Authorization: token "+token,
		"-H", "Accept: application/vnd.github+json",
		fmt.Sprintf("https://api.github.com/repos/%s/releases/tags/%s", githubRepo, tag))
	if err != nil {
		return "", fmt.Errorf("failed to look up release %s: %w", tag, err)
	}

	var rel struct {
		Assets []struct {
			Name string `json:"name"`
			URL  string `json:"url"`
		} `json:"assets"`
	}
	if err := json.Unmarshal(out, &rel); err != nil {
		return "", fmt.Errorf("failed to parse release %s: %w", tag, err)
	}
	for _, a := range rel.Assets {
		if a.Name == assetName {
			return a.URL, nil
		}
	}
	return "", fmt.Errorf("no %s asset found on release %s", assetName, tag)
}

// filterSSHArgs strips --ssh/--ssh-identity (and their values) from the
// args being forwarded to the remote invocation -- otherwise the remote
// run would just try to ssh again from inside itself.
func filterSSHArgs(args []string) []string {
	out := make([]string, 0, len(args))
	for i := 0; i < len(args); i++ {
		if args[i] == "--ssh" || args[i] == "--ssh-identity" {
			i++ // also skip the value
			continue
		}
		out = append(out, args[i])
	}
	return out
}

func shellJoin(args []string) string {
	quoted := make([]string, len(args))
	for i, a := range args {
		quoted[i] = shellSingleQuote(a)
	}
	return strings.Join(quoted, " ")
}

func shellSingleQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}
