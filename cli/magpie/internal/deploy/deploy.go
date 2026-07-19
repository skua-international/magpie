// Package deploy is a Go port of scripts/deploy.sh's core logic --
// pull the published chart from its OCI publish, apply its CRDs (helm
// upgrade never touches charts/crds/ once installed, a deliberate Helm
// safety choice -- see https://helm.sh/docs/chart_best_practices/custom_resource_definitions/),
// then helm upgrade (optionally --install). Shells out to the real
// helm/kubectl binaries rather than a native Go client library --
// reasonable external tools for a Kubernetes admin CLI to assume, same
// as scripts/deploy.sh itself already does, and it means this can never
// silently drift out of sync with what that script does.
package deploy

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"runtime"
	"strings"
)

const (
	chartRef   = "oci://ghcr.io/skua-international/magpie/charts/magpie"
	githubRepo = "skua-international/magpie"
)

type Options struct {
	Version       string
	ImageTag      string
	Namespace     string
	Release       string
	Timeout       string
	Install       bool
	DryRun        bool
	ExtraHelmArgs []string

	// Only meaningful with Install -- see BootstrapK3s/EnsureInstallSecrets
	// for what each actually does.
	BootstrapK3s        bool
	DataDir             string
	ExternalPostgresURL string
	GHCRUser            string
	GHCRToken           string
	IdentityBaseURL     string
	IngressBaseDomain   string
	// Only meaningful with Install -- see EnsureVolumeManagerUser. Must
	// match charts/magpie's values.yaml hostPaths.blobImage/
	// blobMountPath defaults unless both are overridden together (the
	// chart and this provisioning step have to agree on where the blob
	// actually lives).
	BlobImagePath string
	BlobMountPath string

	// Set by RunRemoteInstall once it's already run EnsureVolumeManagerUser
	// remotely (on the actual --ssh target, the only place that
	// provisioning can correctly happen) -- Run() uses these directly
	// instead of calling EnsureVolumeManagerUser itself in that case. Not
	// meaningful outside RunRemoteInstall's own use.
	SkipVolumeManagerProvisioning bool
	VolumeManagerUID              int
	VolumeManagerGID              int
}

// CheckTools verifies helm and kubectl are on PATH, printing an
// OS/distro-appropriate install hint for whichever is missing rather
// than just failing with "executable file not found in $PATH".
func CheckTools() error {
	var missing []string
	if _, err := exec.LookPath("helm"); err != nil {
		missing = append(missing, "helm")
	}
	if _, err := exec.LookPath("kubectl"); err != nil {
		missing = append(missing, "kubectl")
	}
	if len(missing) == 0 {
		return nil
	}

	fmt.Fprintf(os.Stderr, "missing required tool(s): %s\n\n", strings.Join(missing, ", "))
	for _, tool := range missing {
		fmt.Fprintln(os.Stderr, installHint(tool))
	}
	return fmt.Errorf("install the missing tool(s) above and try again")
}

func installHint(tool string) string {
	switch runtime.GOOS {
	case "darwin":
		return fmt.Sprintf("  brew install %s", brewPackage(tool))
	case "windows":
		return fmt.Sprintf("  winget install %s   (or: choco install %s)", wingetPackage(tool), tool)
	default:
		return linuxInstallHint(tool)
	}
}

func brewPackage(tool string) string {
	if tool == "kubectl" {
		return "kubernetes-cli"
	}
	return tool
}

func wingetPackage(tool string) string {
	if tool == "kubectl" {
		return "Kubernetes.kubectl"
	}
	return "Helm.Helm"
}

// linuxInstallHint reads /etc/os-release's ID (and ID_LIKE as a
// fallback) to pick a package-manager-specific command -- falls back to
// pointing at the upstream install docs for anything unrecognized rather
// than guessing wrong.
func linuxInstallHint(tool string) string {
	id, idLike := osRelease()
	pm := packageManagerFor(id, idLike)
	switch pm {
	case "apt":
		if tool == "kubectl" {
			return "  sudo apt-get update && sudo apt-get install -y kubectl"
		}
		return "  curl -fsSL https://baltocdn.com/helm/signing.asc | sudo apt-key add - && sudo apt-get install -y helm"
	case "dnf":
		return fmt.Sprintf("  sudo dnf install -y %s", tool)
	case "pacman":
		return fmt.Sprintf("  sudo pacman -S %s", tool)
	case "zypper":
		return fmt.Sprintf("  sudo zypper install -y %s", tool)
	default:
		if tool == "kubectl" {
			return "  see https://kubernetes.io/docs/tasks/tools/install-kubectl-linux/"
		}
		return "  curl https://raw.githubusercontent.com/helm/helm/main/scripts/get-helm-3 | bash"
	}
}

func osRelease() (id, idLike string) {
	data, err := os.ReadFile("/etc/os-release")
	if err != nil {
		return "", ""
	}
	for _, line := range strings.Split(string(data), "\n") {
		line = strings.TrimSpace(line)
		val := strings.Trim(strings.TrimPrefix(line, strings.SplitN(line, "=", 2)[0]+"="), `"`)
		switch {
		case strings.HasPrefix(line, "ID="):
			id = val
		case strings.HasPrefix(line, "ID_LIKE="):
			idLike = val
		}
	}
	return id, idLike
}

func packageManagerFor(id, idLike string) string {
	combined := id + " " + idLike
	switch {
	case strings.Contains(combined, "debian") || strings.Contains(combined, "ubuntu"):
		return "apt"
	case strings.Contains(combined, "fedora") || strings.Contains(combined, "rhel") || strings.Contains(combined, "centos"):
		return "dnf"
	case strings.Contains(combined, "arch"):
		return "pacman"
	case strings.Contains(combined, "suse"):
		return "zypper"
	default:
		return ""
	}
}

// ResolveVersion returns explicitVersion unchanged if set, otherwise the
// latest GitHub release's version (this repo is private, so the lookup
// needs `gh auth token`, same as the download-on-first-use path in
// internal/steamlogin).
func ResolveVersion(ctx context.Context, explicitVersion string) (string, error) {
	if explicitVersion != "" {
		return explicitVersion, nil
	}

	token, err := ghToken()
	if err != nil {
		return "", fmt.Errorf("no version given and couldn't resolve the latest release: %w", err)
	}

	out, err := runCaptured(ctx, "curl", "-sSf",
		"-H", "Authorization: token "+token,
		"-H", "Accept: application/vnd.github+json",
		fmt.Sprintf("https://api.github.com/repos/%s/releases/latest", githubRepo))
	if err != nil {
		return "", fmt.Errorf("failed to look up the latest release: %w", err)
	}

	var rel struct {
		TagName string `json:"tag_name"`
	}
	if err := json.Unmarshal(out, &rel); err != nil || rel.TagName == "" {
		return "", fmt.Errorf("couldn't parse the latest release's tag from GitHub's response")
	}
	return strings.TrimPrefix(rel.TagName, "v"), nil
}

func ghToken() (string, error) {
	out, err := exec.Command("gh", "auth", "token").Output()
	if err != nil {
		return "", fmt.Errorf("`gh auth token` failed -- install and log in to the GitHub CLI (gh auth login), this repo is private: %w", err)
	}
	return strings.TrimSpace(string(out)), nil
}

// Run pulls the chart, applies its CRDs, and runs helm upgrade (with
// --install if requested) -- streaming helm/kubectl's own output
// straight through, same as running them by hand would.
func Run(ctx context.Context, opts Options) error {
	if opts.BootstrapK3s {
		if !opts.Install {
			return fmt.Errorf("--bootstrap-k3s only makes sense with --install (or via `magpiectl install`)")
		}
		if err := BootstrapK3s(ctx, opts.DataDir); err != nil {
			return err
		}
	}

	if err := CheckTools(); err != nil {
		return err
	}

	version, err := ResolveVersion(ctx, opts.Version)
	if err != nil {
		return err
	}
	fmt.Printf("==> Deploying version %s\n", version)

	if !opts.Install {
		if err := run(ctx, "helm", "status", opts.Release, "-n", opts.Namespace); err != nil {
			return fmt.Errorf("no existing '%s' release in namespace '%s' to upgrade -- pass --install for a first install: %w", opts.Release, opts.Namespace, err)
		}
	}

	tmpDir, err := os.MkdirTemp("", "magpie-deploy-*")
	if err != nil {
		return err
	}
	defer os.RemoveAll(tmpDir)

	fmt.Printf("==> Pulling %s version %s\n", chartRef, version)
	if err := run(ctx, "helm", "pull", chartRef, "--version", version, "--untar", "--destination", tmpDir); err != nil {
		return err
	}
	chartDir := filepath.Join(tmpDir, "magpie")

	helmArgs := []string{"--namespace", opts.Namespace, "--wait", "--timeout", opts.Timeout}

	if !opts.Install {
		fmt.Printf("==> Carrying over %s's existing user-supplied values\n", opts.Release)
		values, err := runCaptured(ctx, "helm", "get", "values", opts.Release, "-n", opts.Namespace, "-o", "yaml")
		if err != nil {
			return err
		}
		valuesFile := filepath.Join(tmpDir, "current-values.yaml")
		if err := os.WriteFile(valuesFile, values, 0o600); err != nil {
			return err
		}
		helmArgs = append(helmArgs, "-f", valuesFile)
	} else {
		fmt.Println("==> --install: no prior release, resolving secrets + values")
		helmArgs = append(helmArgs, "--create-namespace")

		secretArgs, err := EnsureInstallSecrets(ctx, opts.ExternalPostgresURL, opts.GHCRUser, opts.GHCRToken)
		if err != nil {
			return err
		}
		opts.ExtraHelmArgs = append(opts.ExtraHelmArgs, secretArgs...)

		if opts.IngressBaseDomain != "" {
			opts.ExtraHelmArgs = append(opts.ExtraHelmArgs, "--set", "ingress.baseDomain="+opts.IngressBaseDomain)
		}
		identityBaseURL := opts.IdentityBaseURL
		if identityBaseURL == "" && opts.IngressBaseDomain != "" {
			identityBaseURL = "http://identity." + opts.IngressBaseDomain
		}
		if identityBaseURL != "" {
			opts.ExtraHelmArgs = append(opts.ExtraHelmArgs, "--set", "identity.baseUrl="+identityBaseURL)
		}

		var uid, gid int
		if opts.SkipVolumeManagerProvisioning {
			// Already done -- RunRemoteInstall ran this on the actual
			// --ssh target (the only place it can correctly happen: it's
			// root-level host provisioning) before calling Run() locally
			// for everything else.
			uid, gid = opts.VolumeManagerUID, opts.VolumeManagerGID
		} else {
			fmt.Println("==> Provisioning the magpie-volume host user for volume-manager")
			uid, gid, err = EnsureVolumeManagerUser(ctx, opts.BlobImagePath, opts.BlobMountPath)
			if err != nil {
				return fmt.Errorf("failed to provision volume-manager's host user: %w", err)
			}
		}
		// contentPath/claimsPath must stay nested under blobMountPath for
		// the same reason they always have to be on one real filesystem
		// (see values.yaml's own comment) -- deriving them here instead
		// of leaving the chart's own hardcoded defaults means overriding
		// --blob-mount-path can't silently break that invariant.
		opts.ExtraHelmArgs = append(opts.ExtraHelmArgs,
			"--set", fmt.Sprintf("volumeManager.runAsUser=%d", uid),
			"--set", fmt.Sprintf("volumeManager.runAsGroup=%d", gid),
			"--set", "hostPaths.blobImage="+opts.BlobImagePath,
			"--set", "hostPaths.blobMountPath="+opts.BlobMountPath,
			"--set", "hostPaths.contentPath="+path.Join(opts.BlobMountPath, "content"),
			"--set", "hostPaths.claimsPath="+path.Join(opts.BlobMountPath, "claims"),
		)
	}

	if opts.ImageTag != "" {
		fmt.Printf("==> Overriding image tag: %s\n", opts.ImageTag)
		helmArgs = append(helmArgs, "--set", "image.tag="+opts.ImageTag)
	}

	fmt.Println("==> Applying CRDs")
	if err := run(ctx, "kubectl", "apply", "-f", filepath.Join(chartDir, "crds")); err != nil {
		return err
	}

	verb := "upgrade"
	args := []string{verb, opts.Release, chartDir}
	args = append(args, helmArgs...)
	args = append(args, opts.ExtraHelmArgs...)
	if opts.Install {
		args = append(args, "--install")
	}
	if opts.DryRun {
		args = append(args, "--dry-run")
	}

	fmt.Printf("==> helm %s %s -> %s\n", verb, opts.Release, version)
	if err := run(ctx, "helm", args...); err != nil {
		return err
	}

	if !opts.DryRun {
		fmt.Println("==> Deployed. Pods:")
		_ = run(ctx, "kubectl", "get", "pods", "-n", opts.Namespace, "-o", "wide")
	}
	return nil
}

// RunNodeSetup does only the root-level, host-specific provisioning a
// first install needs -- installing k3s and creating the magpie-volume
// user -- and nothing else. Used by RunRemoteInstall as the one thing it
// actually runs on the --ssh target: k3s and the volume-manager user
// genuinely have to be provisioned on that machine, but nothing else
// does (secrets, helm pull/upgrade all run locally afterward, against
// the kubeconfig this leaves behind) -- keeping the remote host's own
// footprint to exactly that, rather than mirroring the whole install
// onto it (which would also mean requiring helm/kubectl to be installed
// there too, for no real reason).
//
// Prints MAGPIECTL_VOLUME_UID=<uid> and MAGPIECTL_VOLUME_GID=<gid> on
// their own lines so RunRemoteInstall can parse the result back out of
// this command's own stdout over SSH.
func RunNodeSetup(ctx context.Context, dataDir, blobImagePath, blobMountPath string) error {
	if err := BootstrapK3s(ctx, dataDir); err != nil {
		return err
	}
	uid, gid, err := EnsureVolumeManagerUser(ctx, blobImagePath, blobMountPath)
	if err != nil {
		return fmt.Errorf("failed to provision volume-manager's host user: %w", err)
	}
	fmt.Printf("MAGPIECTL_VOLUME_UID=%d\n", uid)
	fmt.Printf("MAGPIECTL_VOLUME_GID=%d\n", gid)
	return nil
}

func run(ctx context.Context, name string, args ...string) error {
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

func runCaptured(ctx context.Context, name string, args ...string) ([]byte, error) {
	cmd := exec.CommandContext(ctx, name, args...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return nil, fmt.Errorf("%s %s: %w: %s", name, strings.Join(args, " "), err, strings.TrimSpace(stderr.String()))
	}
	return stdout.Bytes(), nil
}
