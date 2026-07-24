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
	// Set by Run() itself after a local BootstrapK3s call, or by
	// RunRemoteInstall after k3s was installed on the --ssh target
	// (kubeletRootDir(opts.DataDir) is deterministic, so it doesn't need
	// anything fetched back from the remote host to compute) -- Run()'s
	// Install branch passes this straight through as
	// --set magpieCsi.kubeletDir=, which has to exactly match wherever
	// this specific k3s install actually put kubelet's own root-dir, or
	// node-driver-registrar registers into a path kubelet never
	// actually watches (confirmed live). Left empty (chart default
	// applies) when magpiectl didn't bootstrap k3s itself at all.
	KubeletDir string
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
		dir, err := BootstrapK3s(ctx, opts.DataDir)
		if err != nil {
			return err
		}
		opts.KubeletDir = dir
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

	// --force-conflicts: Helm defaults --server-side to "auto", which uses
	// Server-Side Apply on any install with no prior release to inherit a
	// method from (every --install, and any upgrade against a cluster
	// Helm doesn't already have release history for). SSA refuses to
	// reassert a field another field manager currently owns without this
	// -- confirmed live: re-running `install --bootstrap-k3s` against a
	// VM that still had a prior attempt's ConfigMap (edited once via
	// `kubectl edit`, e.g. through maybePromptEditBaselineConfigMap)
	// failed outright ("conflict with \"kubectl-edit\" ... .data.verify_
	// signatures"). A no-op when SSA isn't actually in play, so always
	// passing it is safe -- Helm's own chart-templated value should win
	// on a real deploy regardless, the same as it always implicitly did
	// before SSA (a hand-edited field is live drift, not something a
	// redeploy should get permanently blocked by).
	helmArgs := []string{"--namespace", opts.Namespace, "--timeout", opts.Timeout, "--force-conflicts"}

	if !opts.Install {
		helmArgs = append(helmArgs, "--wait")
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
		fmt.Println("==> --install: no prior release")
		// Deliberately no --wait, and no secrets resolved yet, either --
		// see EnsureInstallSecrets' own doc for why: this first pass just
		// lets Helm create the namespace itself (its own
		// templates/namespace.yaml, properly Helm-owned), then a second
		// pass below creates secrets into it and actually waits for a
		// healthy rollout, once pods have what they need to start.
		//
		// --create-namespace is required here too, confirmed live against
		// a genuinely fresh cluster: Helm creates its own release-tracking
		// Secret in the target namespace as an internal bookkeeping step
		// *before* it ever applies the chart's own templates (including
		// templates/namespace.yaml) -- "Error: create: failed to create:
		// namespaces \"magpie\" not found", not a transient/eventual-
		// consistency race (retrying the same command against the same
		// cluster ten minutes later reproduced it identically). This does
		// *not* reintroduce the earlier "invalid ownership metadata"
		// conflict -- that was specifically from a namespace pre-created
		// via plain kubectl, which carries none of Helm's own ownership
		// annotations; --create-namespace sets meta.helm.sh/release-name
		// and release-namespace itself (confirmed live), so the chart's
		// own namespace.yaml adopts it cleanly on the same install.
		helmArgs = append(helmArgs, "--create-namespace")
		// Phase 1 still has to satisfy the chart's own `required` guard on
		// postgres.existingSecret whenever postgres.enabled is true (its
		// default) -- the secret itself doesn't exist yet (that's phase 2),
		// but the *name* is deterministic (EnsureInstallSecrets always
		// creates "magpie-postgres-creds" for the chart-managed case), so
		// passing it now just lets the template render; the pods that
		// reference it simply won't be Ready until phase 2's real upgrade,
		// which isn't a problem since phase 1 doesn't --wait anyway.
		if opts.ExternalPostgresURL == "" {
			opts.ExtraHelmArgs = append(opts.ExtraHelmArgs, "--set", "postgres.existingSecret=magpie-postgres-creds")
		} else {
			opts.ExtraHelmArgs = append(opts.ExtraHelmArgs,
				"--set", "postgres.enabled=false",
				"--set", "postgres.externalUrl="+opts.ExternalPostgresURL,
			)
		}

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
		if opts.KubeletDir != "" {
			opts.ExtraHelmArgs = append(opts.ExtraHelmArgs, "--set", "magpieCsi.kubeletDir="+opts.KubeletDir)
		}
	}

	if opts.ImageTag != "" {
		fmt.Printf("==> Overriding image tag: %s\n", opts.ImageTag)
		helmArgs = append(helmArgs, "--set", "image.tag="+opts.ImageTag)
	}

	// Only for upgrades of an existing release -- `helm install` (what
	// --install triggers when there's no prior release) already installs
	// crds/ itself, automatically, as part of a first install. Doing
	// this unconditionally used to also run it before a first install,
	// which then made helm's own CRD install fail outright: confirmed
	// live against a genuinely fresh cluster, "conflict occurred while
	// applying object .../armaservers.arma.skua.io: Apply failed with 1
	// conflict: conflict with 'kubectl-client-side-apply'" -- two
	// different apply mechanisms (kubectl's own field manager vs. helm's
	// internal one) fighting over the same object. This manual step only
	// exists at all because of Helm's own documented limitation the
	// other direction: `helm upgrade` on an *existing* release never
	// touches crds/ again after the first install, so without this,
	// upgrading an existing release would silently never pick up CRD
	// schema changes.
	if !opts.Install {
		fmt.Println("==> Applying CRDs")
		if err := run(ctx, "kubectl", "apply", "-f", filepath.Join(chartDir, "crds")); err != nil {
			return err
		}
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

	// Phase 2 of a first install: now that the chart's own
	// templates/namespace.yaml has actually created the namespace
	// (Helm-owned, not kubectl's), the secrets a first install has no
	// prior release to carry over into it can be created without the
	// ownership conflict pre-creating the namespace via kubectl used to
	// cause -- see EnsureInstallSecrets' own doc. --reuse-values carries
	// forward everything the first pass above already set (ingress,
	// identity, any operator --set/-f); only the new
	// secret args need to be added here. This second upgrade is also
	// what actually --waits for a healthy rollout -- the first pass
	// deliberately didn't, since pods referencing these secrets couldn't
	// have become Ready before they existed. Skipped for --dry-run:
	// nothing real was created for secrets to complete against.
	if opts.Install && !opts.DryRun {
		fmt.Println("==> Namespace created -- resolving secrets")
		secretArgs, err := EnsureInstallSecrets(ctx, opts.ExternalPostgresURL, opts.GHCRUser, opts.GHCRToken)
		if err != nil {
			return err
		}
		secondArgs := []string{
			"upgrade", opts.Release, chartDir,
			"--namespace", opts.Namespace,
			"--reuse-values",
			"--wait", "--timeout", opts.Timeout,
			"--force-conflicts",
		}
		secondArgs = append(secondArgs, secretArgs...)
		fmt.Printf("==> helm upgrade %s -> %s (secrets applied)\n", opts.Release, version)
		if err := run(ctx, "helm", secondArgs...); err != nil {
			return err
		}
	}

	if !opts.DryRun {
		fmt.Println("==> Deployed. Pods:")
		_ = run(ctx, "kubectl", "get", "pods", "-n", opts.Namespace, "-o", "wide")
	}
	return nil
}

// RunNodeSetup does only the root-level, host-specific provisioning a
// first install needs -- installing k3s -- and nothing else. Used by
// RunRemoteInstall as the one thing it actually runs on the --ssh
// target: k3s genuinely has to be installed on that machine, but
// nothing else does (secrets, helm pull/upgrade all run locally
// afterward, against the kubeconfig this leaves behind) -- keeping the
// remote host's own footprint to exactly that, rather than mirroring
// the whole install onto it (which would also mean requiring helm/
// kubectl to be installed there too, for no real reason).
func RunNodeSetup(ctx context.Context, dataDir string) error {
	_, err := BootstrapK3s(ctx, dataDir)
	return err
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
