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
	c.Flags().StringVar(&f.namespace, "namespace", "magpie", "target namespace")
	c.Flags().StringVar(&f.release, "release", "arma", "helm release name")
	c.Flags().StringVar(&f.timeout, "timeout", "10m", "helm --wait timeout")
	c.Flags().BoolVar(&f.dryRun, "dry-run", false, "render + apply nothing")
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
	c := &cobra.Command{
		Use:   "install [version] [-- extra helm args]",
		Short: "Install the magpie stack for the first time (shortcut for `deploy --install`)",
		Long: "Shortcut for `deploy --install` -- there's no prior release to carry values over from, so " +
			"supply every value the chart needs yourself after `--`, e.g.:\n" +
			"  magpiectl install -- --set identity.baseUrl=http://identity.magpie.local \\\n" +
			"    --set postgres.existingSecret=arma-postgres-creds",
		RunE: func(cc *cobra.Command, args []string) error {
			return deploy.Run(cc.Context(), deploy.Options{
				Version:       versionArg(cc, args),
				ImageTag:      f.imageTag,
				Namespace:     f.namespace,
				Release:       f.release,
				Timeout:       f.timeout,
				Install:       true,
				DryRun:        f.dryRun,
				ExtraHelmArgs: extraArgsAfterDash(cc, args),
			})
		},
	}
	addDeployFlags(c, &f)
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
