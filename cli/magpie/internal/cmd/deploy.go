package cmd

import (
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
	c.Flags().StringVar(&f.namespace, "namespace", defaults.namespace, "target namespace (env MAGPIE_NAMESPACE, or 'magpiectl target')")
	c.Flags().StringVar(&f.release, "release", defaults.release, "helm release name (env MAGPIE_RELEASE, or 'magpiectl target')")
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
		"only with --bootstrap-k3s: k3s/kubelet's own storage root")
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
			if err := deploy.Run(cc.Context(), deploy.Options{
				Version:       versionArg(cc, args),
				ImageTag:      f.imageTag,
				Namespace:     f.namespace,
				Release:       f.release,
				Timeout:       f.timeout,
				Install:       install,
				DryRun:        f.dryRun,
				ExtraHelmArgs: extraArgsAfterDash(cc, args),
			}); err != nil {
				return err
			}
			if !f.dryRun {
				maybePromptEditBaselineConfigMap(cc.Context(), f.namespace, f.release)
			}
			return nil
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
	var nodeSetupOnly bool
	c := &cobra.Command{
		Use:   "install [version] [-- extra helm args]",
		Short: "Install the magpie stack for the first time (shortcut for `deploy --install`)",
		Long: "Shortcut for `deploy --install`, plus everything a from-scratch install needs that an " +
			"upgrade doesn't: optionally bootstrapping k3s itself (--bootstrap-k3s), and creating the " +
			"secrets/values a first install has no prior release to carry over. Without --bootstrap-k3s, " +
			"an existing kubeconfig/kubectl context pointed at a real cluster is required, same as " +
			"deploy/upgrade. Any --set/-f passed after `--` layers on top of everything this resolves.\n\n" +
			"--ssh keeps the remote host's own footprint to just what genuinely has to run there: it " +
			"detects the remote host's architecture and downloads the matching magpiectl release binary " +
			"directly onto it (never copies over whatever platform is running this controlling process " +
			"-- a Windows or macOS machine driving a Linux VM is the normal case here, not an edge case), " +
			"runs only k3s install there (a genuinely root-level operation on that specific machine), " +
			"then fetches the resulting kubeconfig back and runs everything else -- secrets, helm " +
			"pull/upgrade -- locally against it. helm/kubectl only need to be installed on this machine, " +
			"never the remote one. Assumes the target already has root or passwordless sudo.",
		RunE: func(cc *cobra.Command, args []string) error {
			opts := deploy.Options{
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
			}
			// --node-setup-only was being checked before --ssh here,
			// which meant `--ssh <host> --node-setup-only` silently
			// ignored --ssh entirely and bootstrapped k3s on the local
			// machine instead of the remote target -- confirmed live.
			// --ssh is checked first now; nodeSetupOnly on its own
			// (no --ssh) still means "run node-setup locally," used by
			// RunRemoteInstall's own remote invocation of this exact
			// binary on the target host, where --ssh has no meaning.
			if sshHost != "" && nodeSetupOnly {
				version, err := deploy.ResolveVersion(cc.Context(), opts.Version)
				if err != nil {
					return err
				}
				opts.Version = version
				return deploy.RunRemoteNodeSetupOnly(cc.Context(), sshHost, sshIdentity, opts)
			}
			if nodeSetupOnly {
				return deploy.RunNodeSetup(cc.Context(), opts.DataDir)
			}
			if sshHost != "" {
				version, err := deploy.ResolveVersion(cc.Context(), opts.Version)
				if err != nil {
					return err
				}
				opts.Version = version
				if err := deploy.RunRemoteInstall(cc.Context(), sshHost, sshIdentity, opts); err != nil {
					return err
				}
				if !opts.DryRun {
					maybePromptEditBaselineConfigMap(cc.Context(), opts.Namespace, opts.Release)
				}
				return nil
			}
			if err := deploy.Run(cc.Context(), opts); err != nil {
				return err
			}
			if !opts.DryRun {
				maybePromptEditBaselineConfigMap(cc.Context(), opts.Namespace, opts.Release)
			}
			return nil
		},
	}
	addDeployFlags(c, &f)
	addInstallFlags(c, &i)
	c.Flags().StringVar(&sshHost, "ssh", "", "run this install on a remote host over ssh instead of locally (e.g. user@host)")
	c.Flags().StringVar(&sshIdentity, "ssh-identity", "", "ssh identity file to use with --ssh")
	c.Flags().BoolVar(&nodeSetupOnly, "node-setup-only", false,
		"internal: used by --ssh on the remote side to run only k3s install, not the full install")
	_ = c.Flags().MarkHidden("node-setup-only")
	return c
}

func upgradeCmd() *cobra.Command {
	var f deployFlags
	c := &cobra.Command{
		Use:   "upgrade [version] [-- extra helm args]",
		Short: "Upgrade an existing magpie stack (alias for `deploy`)",
		RunE: func(cc *cobra.Command, args []string) error {
			if err := deploy.Run(cc.Context(), deploy.Options{
				Version:       versionArg(cc, args),
				ImageTag:      f.imageTag,
				Namespace:     f.namespace,
				Release:       f.release,
				Timeout:       f.timeout,
				Install:       false,
				DryRun:        f.dryRun,
				ExtraHelmArgs: extraArgsAfterDash(cc, args),
			}); err != nil {
				return err
			}
			if !f.dryRun {
				maybePromptEditBaselineConfigMap(cc.Context(), f.namespace, f.release)
			}
			return nil
		},
	}
	addDeployFlags(c, &f)
	return c
}
