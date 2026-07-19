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

// RunRemoteInstall runs only the root-level, host-specific part of a
// first install (k3s) on a remote host over ssh -- everything else
// (secrets, helm pull/upgrade) runs locally afterward, against the
// kubeconfig that leaves behind. Deliberately keeps the remote host's
// own footprint to exactly what actually has to run there: k3s install
// is a genuinely root-level operation on that specific machine, but
// helm/kubectl operations aren't tied to any particular host at all,
// and requiring helm to be installed on the target too (the original
// design, which mirrored this entire process onto it) was needless --
// confirmed live, a fresh VM had no helm and there was no real reason
// it needed one.
//
// Doesn't scp over the binary currently running this process either --
// that binary matches whatever platform/arch is controlling this
// session, which has no guaranteed relationship to the remote target's
// (a Windows or macOS/arm64 machine driving a Linux/amd64 VM is exactly
// the normal case here, not an edge case). k3s itself only targets
// Linux, so the remote OS is always "linux"; only the architecture needs
// detecting (via `uname -m` on the remote host itself), then the
// matching magpiectl_linux_<arch> release asset is downloaded there
// directly -- same private-repo GitHub API dance internal/steamlogin
// uses for its own release assets, just resolving magpiectl's own asset.
//
// Assumes the ssh target already has root or passwordless sudo -- no TTY
// for an interactive sudo password prompt over a non-interactive ssh
// command.
func RunRemoteInstall(ctx context.Context, sshHost, sshIdentity string, opts Options) error {
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

	tag := "v" + opts.Version
	fmt.Printf("==> Resolving %s from release %s...\n", assetName, tag)
	assetURL, err := releaseAssetURL(ctx, tag, assetName, token)
	if err != nil {
		return err
	}

	// A single remote shell command, not scp+ssh separately: downloads
	// the correct-platform magpiectl straight to the remote host (never
	// transits through this machine at all), runs it with only the
	// node-setup-only invocation (k3s, nothing else), and cleans up
	// after itself regardless of exit status.
	remoteArgs := []string{
		"install", "--bootstrap-k3s", "--node-setup-only",
		"--data-dir", opts.DataDir,
	}
	remoteScript := fmt.Sprintf(
		`set -e; curl -sSf -H "Authorization: token %s" -H "Accept: application/octet-stream" -L %s -o /tmp/magpiectl.tar.gz && tar xzf /tmp/magpiectl.tar.gz -C /tmp magpiectl && chmod +x /tmp/magpiectl; rc=$?; if [ $rc -eq 0 ]; then /tmp/magpiectl %s; rc=$?; fi; rm -f /tmp/magpiectl /tmp/magpiectl.tar.gz; exit $rc`,
		token, shellSingleQuote(assetURL), shellJoin(remoteArgs),
	)

	fmt.Printf("==> Provisioning %s (k3s)...\n", sshHost)
	sshRunArgs := append(append([]string{}, sshArgs...), sshHost, remoteScript)
	cmd := exec.CommandContext(ctx, "ssh", sshRunArgs...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	if err := cmd.Run(); err != nil {
		return fmt.Errorf("remote node setup failed: %w", err)
	}

	fmt.Printf("==> Fetching kubeconfig from %s...\n", sshHost)
	catArgs := append(append([]string{}, sshArgs...), sshHost, `cat "$HOME/.kube/config"`)
	kubeconfig, err := exec.CommandContext(ctx, "ssh", catArgs...).Output()
	if err != nil {
		return fmt.Errorf("failed to fetch kubeconfig from %s: %w", sshHost, err)
	}

	// Not just sshHost's own hostname part: it may be a bare alias
	// defined in ~/.ssh/config (a `Host` block with its own `HostName`),
	// which only ssh itself knows how to resolve -- confirmed live,
	// kubectl (a separate tool, doesn't read ~/.ssh/config at all) failed
	// outright trying to literally resolve "magpie-vm" as a real
	// hostname. `ssh -G` resolves the same way an actual ssh connection
	// would, alias or not, so use that instead of assuming sshHost is
	// already a real hostname/IP.
	remoteHost, err := resolveSSHHostname(ctx, sshHost, sshArgs)
	if err != nil {
		return fmt.Errorf("failed to resolve %s's real hostname via ssh -G: %w", sshHost, err)
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
	fmt.Printf("==> kubeconfig saved to %s\n", localPath)
	if err := os.Setenv("KUBECONFIG", localPath); err != nil {
		return err
	}

	// Everything else -- CheckTools, secrets, helm pull/upgrade -- runs
	// locally from here, against the cluster over the network via the
	// kubeconfig above. helm/kubectl only need to be installed on this
	// machine, never the remote one.
	opts.BootstrapK3s = false
	// kubeletRootDir is deterministic (same dataDir -> same path), so
	// this doesn't need anything fetched back from the remote host --
	// the k3s install that just happened there (via --node-setup-only)
	// used this exact same derivation. See Options.KubeletDir's own doc
	// for why this has to be exactly right.
	opts.KubeletDir = kubeletRootDir(opts.DataDir)
	fmt.Println("==> Continuing the install locally against the new cluster...")
	return Run(ctx, opts)
}

// resolveSSHHostname returns whatever `ssh -G` says it would actually
// connect to for host -- if host is a plain hostname/IP this is just
// host itself, but if it's a ~/.ssh/config alias, this resolves that
// alias's HostName the same way a real ssh connection would, instead of
// assuming the literal string passed to --ssh is already something
// other tools (kubectl, not ssh) can resolve on their own.
func resolveSSHHostname(ctx context.Context, host string, sshArgs []string) (string, error) {
	target := host
	if idx := strings.Index(target, "@"); idx >= 0 {
		target = target[idx+1:]
	}
	out, err := exec.CommandContext(ctx, "ssh", append(append([]string{}, sshArgs...), "-G", target)...).Output()
	if err != nil {
		return "", err
	}
	for _, line := range strings.Split(string(out), "\n") {
		if name, ok := strings.CutPrefix(line, "hostname "); ok {
			return strings.TrimSpace(name), nil
		}
	}
	return "", fmt.Errorf("no hostname in `ssh -G %s` output", target)
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
