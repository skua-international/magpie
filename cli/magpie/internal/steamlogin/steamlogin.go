// Package steamlogin downloads (and caches) the steam-login helper
// binary -- a companion Rust binary (crates/steam-sync's src/bin/
// steam-login.rs) that does the actual interactive Steam login
// negotiation, since there's no usable Go implementation of Steam's
// login protocol to build on. Reused rather than reimplemented so a
// Steam password never has to reach any deployed service: the
// negotiation happens entirely on the operator's own machine, and only
// the resulting refresh token is ever sent to the cluster (see
// RefreshSteamAuth's own proto doc).
//
// From the operator's side this is invisible -- `magpiectl admin
// refresh-steam-auth` downloads the helper transparently on first use,
// the same way many Go CLIs shell out to a companion binary for one
// piece of platform-specific work (kubectl plugins, git remote helpers,
// docker buildx).
package steamlogin

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
)

// Version is set by main.go to the exact same build-time-stamped version
// as the running magpiectl binary -- steam-login ships from the same
// release tag as magpiectl itself, so pinning the download to this version
// (rather than "latest") means upgrading magpiectl automatically fetches a
// matching helper next time. No separate "check for updates, prompt to
// upgrade" logic needed at all: a version mismatch between magpie and
// its helper is structurally impossible this way, not something to
// detect after the fact.
var Version = "dev"

const githubRepo = "skua-international/magpie"

// EnsureBinary returns the path to a local, version-matched steam-login
// binary, downloading it from this build's own GitHub release if it
// isn't already cached. Any other cached version found in the same
// directory is removed first, so repeated magpiectl upgrades don't leave a
// pile of stale helper binaries behind.
func EnsureBinary(ctx context.Context) (string, error) {
	if Version == "dev" {
		return "", fmt.Errorf("steam-login isn't published for a dev build of magpiectl -- build it yourself: cargo run -p steam-sync --bin steam-login")
	}

	dir, err := cacheDir()
	if err != nil {
		return "", err
	}
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", err
	}

	target := filepath.Join(dir, fmt.Sprintf("steam-login-v%s%s", Version, binExt()))
	if _, err := os.Stat(target); err == nil {
		return target, nil
	}

	if err := pruneStale(dir, target); err != nil {
		fmt.Fprintf(os.Stderr, "warning: failed to prune old steam-login binaries: %v\n", err)
	}

	fmt.Println("Downloading steam-login helper (first use for this magpiectl version)...")
	if err := download(ctx, assetName(), target); err != nil {
		return "", err
	}
	if err := os.Chmod(target, 0o700); err != nil {
		return "", err
	}
	return target, nil
}

// NegotiateResult mirrors steam-login's own JSON output shape.
type NegotiateResult struct {
	SteamUser    string
	RefreshToken string
}

// Negotiate runs the (already-downloaded) helper binary once, with no
// arguments at all -- it does a QR-code login (see steam-login.rs), so
// there's no password or guard code to pass in, and nothing to prompt
// the operator for here either. Stderr is inherited so the QR code and
// "waiting for confirmation" status render directly in the operator's
// own terminal in real time; only the final JSON line on stdout is
// captured.
func Negotiate(ctx context.Context, binPath string) (*NegotiateResult, error) {
	cmd := exec.CommandContext(ctx, binPath)
	cmd.Stderr = os.Stderr
	out, err := cmd.Output()
	if err != nil {
		return nil, fmt.Errorf("steam-login failed: %w", err)
	}

	var raw struct {
		SteamUser    string `json:"steam_user"`
		RefreshToken string `json:"refresh_token"`
	}
	if err := json.Unmarshal(out, &raw); err != nil {
		return nil, fmt.Errorf("failed to parse steam-login output: %w", err)
	}
	return &NegotiateResult{SteamUser: raw.SteamUser, RefreshToken: raw.RefreshToken}, nil
}

func cacheDir() (string, error) {
	dir, err := os.UserConfigDir()
	if err != nil {
		return "", err
	}
	return filepath.Join(dir, "magpiectl", "bin"), nil
}

func pruneStale(dir, keep string) error {
	entries, err := os.ReadDir(dir)
	if err != nil {
		if os.IsNotExist(err) {
			return nil
		}
		return err
	}
	for _, e := range entries {
		path := filepath.Join(dir, e.Name())
		if path == keep || !strings.HasPrefix(e.Name(), "steam-login-") {
			continue
		}
		_ = os.Remove(path)
	}
	return nil
}

func binExt() string {
	if runtime.GOOS == "windows" {
		return ".exe"
	}
	return ""
}

// assetName matches the naming convention .github/workflows/release-cli.yml
// publishes steam-login binaries under -- runtime.GOOS/GOARCH already use
// the same values that convention is built on (linux/darwin/windows,
// amd64/arm64), so no translation table is needed.
func assetName() string {
	return fmt.Sprintf("steam-login_%s_%s%s", runtime.GOOS, runtime.GOARCH, binExt())
}

func ghToken() (string, error) {
	out, err := exec.Command("gh", "auth", "token").Output()
	if err != nil {
		return "", fmt.Errorf("failed to get a GitHub token via `gh auth token` -- steam-login is a release asset in a private repo, install and log in to the GitHub CLI (gh auth login) to download it: %w", err)
	}
	return strings.TrimSpace(string(out)), nil
}

type releaseAsset struct {
	Name string `json:"name"`
	URL  string `json:"url"`
}

type release struct {
	Assets []releaseAsset `json:"assets"`
}

func download(ctx context.Context, assetFileName, target string) error {
	token, err := ghToken()
	if err != nil {
		return err
	}

	tag := "v" + Version
	rel, err := fetchRelease(ctx, tag, token)
	if err != nil {
		return err
	}

	var assetURL string
	for _, a := range rel.Assets {
		if a.Name == assetFileName {
			assetURL = a.URL
			break
		}
	}
	if assetURL == "" {
		return fmt.Errorf("no %s asset found on release %s -- steam-login may not have been published for this platform/version", assetFileName, tag)
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, assetURL, nil)
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "token "+token)
	req.Header.Set("Accept", "application/octet-stream")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return fmt.Errorf("failed to download steam-login: %w", err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("failed to download steam-login: server returned %s", resp.Status)
	}

	f, err := os.Create(target)
	if err != nil {
		return err
	}
	defer f.Close()
	if _, err := io.Copy(f, resp.Body); err != nil {
		os.Remove(target)
		return fmt.Errorf("failed to write steam-login binary: %w", err)
	}
	return nil
}

func fetchRelease(ctx context.Context, tag, token string) (*release, error) {
	url := fmt.Sprintf("https://api.github.com/repos/%s/releases/tags/%s", githubRepo, tag)
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "token "+token)
	req.Header.Set("Accept", "application/vnd.github+json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return nil, fmt.Errorf("failed to look up release %s: %w", tag, err)
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("failed to look up release %s: server returned %s", tag, resp.Status)
	}
	var rel release
	if err := json.NewDecoder(resp.Body).Decode(&rel); err != nil {
		return nil, err
	}
	return &rel, nil
}
