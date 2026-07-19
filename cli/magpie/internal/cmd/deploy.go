package cmd

import (
	"os"

	"github.com/spf13/cobra"

	"github.com/skua-international/magpie/cli/internal/deploy"
)

// deployFlags is shared by deploy/upgrade/install so their --help output
// (and behavior) stays identical apart from what --install defaults to.
type deployFlags struct {
	imageTag  string
	namespace string
	release   string
	timeout   string
	dryRun    bool
}

func addDeployFlags(c *cobra.Command, f *deployFlags) {
	c.Flags().StringVar(&f.imageTag, "image-tag", "", "override just the image tag (chart stays at the resolved version)")
	c.Flags().StringVar(&f.namespace, "namespace", "magpie", "target namespace")
	c.Flags().StringVar(&f.release, "release", "arma", "helm release name")
	c.Flags().StringVar(&f.timeout, "timeout", "10m", "helm --wait timeout")
	c.Flags().BoolVar(&f.dryRun, "dry-run", false, "render + apply nothing")
}

// installFlags covers everything only a first install ever needs --
// bootstrapping k3s itself, and the secrets/values a from-scratch chart
// install has no prior release to carry over. See internal/deploy's
// BootstrapK3s/EnsureInstallSecrets for what each actually does.
type installFlags struct {
	bootstrapK3s        bool
	dataDir             string
	externalPostgresURL string
	ghcrUser            string
	ghcrToken           string
	identityBaseURL     string
	ingressBaseDomain   string
}

func addInstallFlags(c *cobra.Command, f *installFlags) {
	c.Flags().BoolVar(&f.bootstrapK3s, "bootstrap-k3s", false,
		"install k3s on this host first (opt-in) -- without this, an existing kubeconfig/kubectl context is required, same as deploy/upgrade")
	c.Flags().StringVar(&f.dataDir, "data-dir", "/var/lib/magpie",
		"only with --bootstrap-k3s: directory checked for reflink (CoW claim) filesystem support")
	c.Flags().StringVar(&f.externalPostgresURL, "external-postgres-url", "",
		"use an existing Postgres instead of the chart-managed one (postgres.enabled=false)")
	c.Flags().StringVar(&f.ghcrUser, "ghcr-user", "", "GHCR username -- creates ghcr-pull-secret if this and --ghcr-token are both set")
	c.Flags().StringVar(&f.ghcrToken, "ghcr-token", "", "GHCR token (needs read:packages) -- creates ghcr-pull-secret if this and --ghcr-user are both set")
	c.Flags().StringVar(&f.identityBaseURL, "identity-base-url", "",
		"identity's externally-reachable base URL (default: http://identity.<ingress-base-domain>)")
	c.Flags().StringVar(&f.ingressBaseDomain, "ingress-base-domain", "magpie.local", "base domain every service's Ingress hostname is built from")
}

// extraArgsAfterDash returns whatever was passed after a literal `--` on
// the command line -- forwarded straight through to `helm upgrade` as
// extra --set/-f overrides, exactly like scripts/deploy.sh's own `--`
// handling.
func extraArgsAfterDash(c *cobra.Command, args []string) []string {
	dash := c.ArgsLenAtDash()
	if dash < 0 {
		return nil
	}
	return args[dash:]
}

func versionArg(c *cobra.Command, args []string) string {
	dash := c.ArgsLenAtDash()
	n := len(args)
	if dash >= 0 {
		n = dash
	}
	if n > 0 {
		return args[0]
	}
	return ""
}

func deployCmd() *cobra.Command {
	var f deployFlags
	var install bool
	c := &cobra.Command{
		Use:   "deploy [version] [-- extra helm args]",
		Short: "Deploy a published version of the magpie stack to the current kubectl context",
		Long: "Deploy a published version of the magpie stack -- pulls the chart from its OCI publish " +
			"(never a local checkout, so it's always exactly what CI published), applies its CRDs, and " +
			"runs helm upgrade. With no version, deploys the latest GitHub release. Go equivalent of " +
			"scripts/deploy.sh, shelling out to the same helm/kubectl binaries.",
		RunE: func(cc *cobra.Command, args []string) error {
			return deploy.Run(cc.Context(), deploy.Options{
				Version:       versionArg(cc, args),
				ImageTag:      f.imageTag,
				Namespace:     f.namespace,
				Release:       f.release,
				Timeout:       f.timeout,
				Install:       install,
				DryRun:        f.dryRun,
				ExtraHelmArgs: extraArgsAfterDash(cc, args),
			})
		},
	}
	addDeployFlags(c, &f)
	c.Flags().BoolVar(&install, "install", false, "allow a first install (helm upgrade --install) instead of requiring an existing release")
	return c
}

func installCmd() *cobra.Command {
	var f deployFlags
	var i installFlags
	var sshHost, sshIdentity string
	c := &cobra.Command{
		Use:   "install [version] [-- extra helm args]",
		Short: "Install the magpie stack for the first time (shortcut for `deploy --install`)",
		Long: "Shortcut for `deploy --install`, plus everything a from-scratch install needs that an " +
			"upgrade doesn't: optionally bootstrapping k3s itself (--bootstrap-k3s), and creating the " +
			"secrets/values a first install has no prior release to carry over. Without --bootstrap-k3s, " +
			"an existing kubeconfig/kubectl context pointed at a real cluster is required, same as " +
			"deploy/upgrade. Any --set/-f passed after `--` layers on top of everything this resolves.\n\n" +
			"--ssh runs this identical command on a remote host instead of locally: k3s only targets " +
			"Linux, so this detects the remote host's architecture and downloads the matching " +
			"magpiectl release binary directly onto it (never copies over whatever platform is running " +
			"this controlling process -- a Windows or macOS machine driving a Linux VM is the normal " +
			"case here, not an edge case), runs it there, then fetches the resulting kubeconfig back -- " +
			"same contract deploy/k3s-bootstrap.sh's own --ssh already has. Assumes the target already " +
			"has root or passwordless sudo.",
		RunE: func(cc *cobra.Command, args []string) error {
			if sshHost != "" {
				version, err := deploy.ResolveVersion(cc.Context(), versionArg(cc, args))
				if err != nil {
					return err
				}
				return deploy.RunRemoteInstall(cc.Context(), sshHost, sshIdentity, version, os.Args[1:])
			}
			return deploy.Run(cc.Context(), deploy.Options{
				Version:             versionArg(cc, args),
				ImageTag:            f.imageTag,
				Namespace:           f.namespace,
				Release:             f.release,
				Timeout:             f.timeout,
				Install:             true,
				DryRun:              f.dryRun,
				ExtraHelmArgs:       extraArgsAfterDash(cc, args),
				BootstrapK3s:        i.bootstrapK3s,
				DataDir:             i.dataDir,
				ExternalPostgresURL: i.externalPostgresURL,
				GHCRUser:            i.ghcrUser,
				GHCRToken:           i.ghcrToken,
				IdentityBaseURL:     i.identityBaseURL,
				IngressBaseDomain:   i.ingressBaseDomain,
			})
		},
	}
	addDeployFlags(c, &f)
	addInstallFlags(c, &i)
	c.Flags().StringVar(&sshHost, "ssh", "", "run this install on a remote host over ssh instead of locally (e.g. user@host)")
	c.Flags().StringVar(&sshIdentity, "ssh-identity", "", "ssh identity file to use with --ssh")
	return c
}

func upgradeCmd() *cobra.Command {
	var f deployFlags
	c := &cobra.Command{
		Use:   "upgrade [version] [-- extra helm args]",
		Short: "Upgrade an existing magpie stack (alias for `deploy`)",
		RunE: func(cc *cobra.Command, args []string) error {
			return deploy.Run(cc.Context(), deploy.Options{
				Version:       versionArg(cc, args),
				ImageTag:      f.imageTag,
				Namespace:     f.namespace,
				Release:       f.release,
				Timeout:       f.timeout,
				Install:       false,
				DryRun:        f.dryRun,
				ExtraHelmArgs: extraArgsAfterDash(cc, args),
			})
		},
	}
	addDeployFlags(c, &f)
	return c
}
