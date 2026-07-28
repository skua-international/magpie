package cmd

import (
	"fmt"
	"os"

	"github.com/skua-international/magpie/cli/internal/client"
	"github.com/skua-international/magpie/cli/internal/config"
)

// resolvedDefaults are the flag defaults every --api-url/--namespace/
// --release flag in this package registers with -- computed once, before
// cobra parses anything, from (in order) an env var, a saved
// `magpiectl target`, then the original hardcoded fallback. An
// explicitly passed flag always wins regardless of where the default
// came from; this only changes what's used when one isn't passed.
type resolvedDefaults struct {
	apiURL    string
	namespace string
	release   string
}

// defaults is computed once at package-init time (before Root() builds
// the command tree, since Go initializes package-level vars before any
// function runs), so every subcommand's own local --namespace/--release
// flag (servers.go, armaconfig.go, deploy.go) picks it up automatically
// just by using defaults.namespace/defaults.release as its registered
// default, with no shared flag or runtime plumbing required.
var defaults = resolveDefaults()

func resolveDefaults() resolvedDefaults {
	target, err := config.LoadTarget()
	if err != nil {
		// A corrupt/unreadable target.json shouldn't break every other
		// invocation -- warn and fall back as if none were ever saved.
		fmt.Fprintf(os.Stderr, "warning: failed to load saved target (%v), ignoring it\n", err)
		target = &config.Target{}
	}
	// A target saved before the single-entrypoint change names hostnames
	// that no longer exist, and it outranks the built-in default, so say
	// so plainly here rather than letting it fail later as a connection
	// error against a host the user never typed.
	if target.LegacyOnly() && os.Getenv("MAGPIE_API_URL") == "" {
		fmt.Fprintf(os.Stderr,
			"warning: your saved target predates magpie's single public entrypoint and lists\n"+
				"  per-service hostnames that no longer exist. Re-run `magpiectl target` to pick it\n"+
				"  again, or pass --api-url. Falling back to %s.\n", client.DefaultAPIURL)
	}
	return resolvedDefaults{
		apiURL:    firstNonEmpty(os.Getenv("MAGPIE_API_URL"), target.APIURL, client.DefaultAPIURL),
		namespace: firstNonEmpty(os.Getenv("MAGPIE_NAMESPACE"), target.Namespace, "magpie"),
		release:   firstNonEmpty(os.Getenv("MAGPIE_RELEASE"), target.Release, "arma"),
	}
}

func firstNonEmpty(vals ...string) string {
	for _, v := range vals {
		if v != "" {
			return v
		}
	}
	return ""
}
