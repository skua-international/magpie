package deploy

import (
	"context"
	"crypto/rand"
	"encoding/base64"
	"fmt"
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
	"strings"
	"time"
)

// chartNamespace must match the chart's own values.yaml `namespace`
// default -- every resource it creates lands here regardless of what
// namespace `helm install` itself is invoked with (see the chart's
// _helpers.tpl magpie.namespace), so secrets created ahead of the chart
// need to target this same fixed name explicitly, not whatever the
// kubeconfig's current context defaults to. A real bug caught live: the
// bash equivalent of this used to create secrets with no -n flag at all
// (landing in `default`, invisible to the chart's own `magpie`-namespaced
// pods) and never pre-created the namespace either -- both fixed here
// and in deploy/k3s-bootstrap.sh at the same time.
const chartNamespace = "magpie"

// BootstrapK3s installs k3s on this host (if not already installed),
// waits for the node to report Ready, and writes a kubeconfig --
// setting KUBECONFIG for the rest of this process so every subsequent
// helm/kubectl call in Run() picks it up automatically.
//
// Local-only, deliberately -- for a remote host, RunRemoteInstall (see
// ssh.go) runs this (via RunNodeSetup) on the target over ssh instead,
// then continues everything else (secrets, helm) locally against the
// kubeconfig it leaves behind. That split is deliberate too: k3s install
// genuinely has to run on the target machine, but nothing downstream of
// it does, so mirroring this whole process onto the remote host (an
// earlier version of this design) would've required helm/kubectl to be
// installed there for no real reason.
func BootstrapK3s(ctx context.Context, dataDir string) error {
	if _, err := exec.LookPath("k3s"); err == nil {
		fmt.Println("==> k3s already installed, skipping install")
	} else {
		// Without this, k3s puts its own storage (containerd's image/
		// layer cache included) under /var/lib/rancher/k3s on whatever
		// disk the OS itself lives on -- confirmed live to matter, not
		// theoretical: a cloud image's default root partition can be a
		// couple GB, nowhere near enough for containerd's image cache
		// once real workloads start pulling, while dataDir commonly
		// points at a much larger, purpose-provisioned disk. k3sDataDir
		// keeps k3s's own storage on that same disk.
		//
		// --data-dir alone isn't enough, confirmed live: kubelet has its
		// own separate default root (/var/lib/kubelet) that --data-dir
		// doesn't touch at all -- pod logs and the emptyDir/ephemeral-
		// storage kubelet tracks for eviction decisions still landed on
		// the OS disk, and once *that* filled up kubelet's eviction
		// manager started trying (and failing, since they're all
		// critical/static pods) to evict everything to reclaim space.
		// --kubelet-arg=root-dir relocates that too, onto the same disk.
		k3sDataDir := filepath.Join(dataDir, "k3s")
		kubeletDir := filepath.Join(dataDir, "kubelet")

		// k3s's own kubeconfig (/etc/rancher/k3s/k3s.yaml) is root-only
		// by default -- confirmed live: everything downstream of install
		// (this function's own Ready-polling, writeKubeconfig, and any
		// bare `k3s kubectl`/`kubectl` an operator runs by hand) hit
		// permission-denied reading it, since --ssh runs as a real
		// non-root user, and only the install script itself
		// self-elevates via sudo internally. Rather than sprinkling sudo
		// through every subsequent call (this process's own and anyone
		// else's), have k3s hand out group-readable access to whichever
		// user is actually running this at install time -- the standard,
		// k3s-documented way to solve this (the install's own warning
		// output names both flags directly), and it fixes every future
		// caller at once, not just this function's.
		kubeconfigGroup, err := currentGroupName()
		if err != nil {
			return fmt.Errorf("failed to resolve the current user's group for --write-kubeconfig-group: %w", err)
		}

		fmt.Printf("==> Installing k3s (data-dir: %s, kubelet root-dir: %s, kubeconfig group: %s)...\n", k3sDataDir, kubeletDir, kubeconfigGroup)
		cmd := exec.CommandContext(ctx, "sh", "-c", "curl -sfL https://get.k3s.io | sh -")
		cmd.Env = append(os.Environ(),
			"INSTALL_K3S_EXEC=server --data-dir="+k3sDataDir+
				" --kubelet-arg=root-dir="+kubeletDir+
				" --write-kubeconfig-mode=640 --write-kubeconfig-group="+kubeconfigGroup,
		)
		cmd.Stdout = os.Stdout
		cmd.Stderr = os.Stderr
		cmd.Stdin = os.Stdin
		if err := cmd.Run(); err != nil {
			return fmt.Errorf("k3s install failed: %w", err)
		}
	}

	fmt.Println("==> Waiting for the node to report Ready...")
	ready := false
	for range 60 {
		out, err := exec.CommandContext(ctx, "k3s", "kubectl", "get", "nodes").Output()
		if err == nil && strings.Contains(string(out), " Ready ") {
			ready = true
			break
		}
		time.Sleep(2 * time.Second)
	}
	if !ready {
		return fmt.Errorf("node did not become Ready in time -- check 'systemctl status k3s' and 'journalctl -u k3s'")
	}
	fmt.Println("==> Node is Ready")

	kubeconfigPath, err := writeKubeconfig(ctx)
	if err != nil {
		return err
	}
	fmt.Printf("==> kubeconfig written to %s\n", kubeconfigPath)
	return os.Setenv("KUBECONFIG", kubeconfigPath)
}

// currentGroupName resolves the current process's own primary group name
// (not just GID) -- what --write-kubeconfig-group actually needs, since
// k3s matches by name, not numeric ID. Relies on the standard useradd/
// cloud-init convention of a per-user primary group sharing the
// username, which every target this runs against (a fresh Debian/Ubuntu
// k3s node) follows.
func currentGroupName() (string, error) {
	u, err := user.Current()
	if err != nil {
		return "", err
	}
	g, err := user.LookupGroupId(u.Gid)
	if err != nil {
		return "", err
	}
	return g.Name, nil
}

func writeKubeconfig(ctx context.Context) (string, error) {
	out, err := exec.CommandContext(ctx, "k3s", "kubectl", "config", "view", "--raw").Output()
	if err != nil {
		return "", fmt.Errorf("failed to read k3s kubeconfig: %w", err)
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", err
	}
	dir := filepath.Join(home, ".kube")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return "", err
	}
	path := filepath.Join(dir, "config")
	if err := os.WriteFile(path, out, 0o600); err != nil {
		return "", err
	}
	return path, nil
}

// EnsureInstallSecrets creates arma-postgres-creds (random password,
// unless externalPostgresURL is set) and, if ghcrUser/ghcrToken are both
// given, ghcr-pull-secret -- mirroring deploy/k3s-bootstrap.sh's own
// secret-resolution step (kept in sync deliberately; see that script's
// comments for the full rationale on why Steam auth needs no secret
// created here at all anymore). Returns the --set args needed to wire
// the results into the chart. Only ever called for a first install.
//
// Deliberately does NOT create the namespace itself any more (used to,
// via kubectl, so secrets had somewhere to land before helm ran at all)
// -- the chart's own templates/namespace.yaml already creates it as a
// normal Helm-owned resource, and creating it twice through two
// different paths is exactly what caused two separate real conflicts:
// kubectl's version has none of Helm's own ownership labels/annotations,
// so both `--create-namespace` and the chart's own namespace template
// refused to proceed against it ("exists and cannot be imported into
// the current release: invalid ownership metadata"), confirmed live
// against a real cluster both ways. Run()'s Install branch now runs an
// initial `helm upgrade --install` (no --wait -- pods referencing
// secrets that don't exist yet can't become Ready) to let Helm create
// the namespace itself, *then* calls this, *then* runs a second
// `helm upgrade --wait` so it actually confirms a healthy rollout now
// that the secrets these create actually exist.
func EnsureInstallSecrets(ctx context.Context, externalPostgresURL, ghcrUser, ghcrToken string) ([]string, error) {
	var setArgs []string

	if externalPostgresURL == "" {
		fmt.Println("==> Creating arma-postgres-creds (random password)...")
		password, err := randomPassword()
		if err != nil {
			return nil, err
		}
		if err := kubectlApplySecret(ctx, "generic", "arma-postgres-creds", []string{"--from-literal=POSTGRES_PASSWORD=" + password}); err != nil {
			return nil, err
		}
		setArgs = append(setArgs, "--set", "postgres.existingSecret=arma-postgres-creds")
	} else {
		setArgs = append(setArgs, "--set", "postgres.enabled=false", "--set", "postgres.externalUrl="+externalPostgresURL)
	}

	if ghcrUser != "" && ghcrToken != "" {
		fmt.Println("==> Creating ghcr-pull-secret...")
		if err := kubectlApplySecret(ctx, "docker-registry", "ghcr-pull-secret", []string{
			"--docker-server=ghcr.io",
			"--docker-username=" + ghcrUser,
			"--docker-password=" + ghcrToken,
		}); err != nil {
			return nil, err
		}
		setArgs = append(setArgs, "--set", "imagePullSecrets={ghcr-pull-secret}")
	} else {
		fmt.Println("==> No --ghcr-user/--ghcr-token given -- skipping pull secret (only needed for a private image registry)")
	}

	fmt.Println("==> Steam auth: no password-based bootstrap secret to create -- run 'magpiectl admin refresh-steam-auth' after install (QR-code login, no password ever touches the cluster), or --set syncDaemon.steamAuth.anonymous=true for anonymous-only access")

	return setArgs, nil
}

func kubectlApplySecret(ctx context.Context, kind, name string, extraArgs []string) error {
	args := append([]string{"create", "secret", kind, name, "-n", chartNamespace}, extraArgs...)
	args = append(args, "--dry-run=client", "-o", "yaml")
	return applyPiped(ctx, args)
}

// applyPiped mirrors `kubectl <createArgs> --dry-run=client -o yaml |
// kubectl apply -f -` -- idempotent (safe to re-run), same pattern
// deploy/k3s-bootstrap.sh uses for exactly the same reason.
func applyPiped(ctx context.Context, createArgs []string) error {
	create := exec.CommandContext(ctx, "kubectl", createArgs...)
	yaml, err := create.Output()
	if err != nil {
		return fmt.Errorf("kubectl %s: %w", strings.Join(createArgs, " "), err)
	}
	apply := exec.CommandContext(ctx, "kubectl", "apply", "-f", "-")
	apply.Stdin = strings.NewReader(string(yaml))
	apply.Stderr = os.Stderr
	if err := apply.Run(); err != nil {
		return fmt.Errorf("kubectl apply: %w", err)
	}
	return nil
}

// URL-safe, deliberately -- this password gets embedded unescaped into
// DATABASE_URL via Kubernetes' $(PGPASSWORD) dependent-env-var expansion
// (see the chart's magpie.postgresEnv helper), which does raw string
// substitution with no percent-encoding. base64.StdEncoding's alphabet
// includes "+", "/", and "=" -- a "/" in particular corrupts a
// postgres://user:pass@host:port/db URL's structure badly enough to
// surface as "invalid port number" from whatever's left after userinfo
// parsing goes wrong, confirmed live. RawURLEncoding's alphabet
// ([A-Za-z0-9-_], no padding) is entirely RFC 3986 unreserved, so it's
// always safe unescaped wherever this password ends up.
func randomPassword() (string, error) {
	buf := make([]byte, 24)
	if _, err := rand.Read(buf); err != nil {
		return "", fmt.Errorf("failed to generate a random password: %w", err)
	}
	return base64.RawURLEncoding.EncodeToString(buf), nil
}
