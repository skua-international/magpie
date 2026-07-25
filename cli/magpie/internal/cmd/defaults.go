package cmd

import (
	"fmt"
	"os"

	"github.com/skua-international/magpie/cli/internal/config"
)

// resolvedDefaults are the flag defaults every --identity-url/
// --server-api-url/--registry-url/--namespace/--release flag in this
// package registers with -- computed once, before cobra parses anything,
// from (in order) an env var, a saved `magpiectl target`, then the
// original hardcoded fallback. An explicitly passed flag always wins
// regardless of where the default came from; this only changes what's
// used when one isn't passed.
type resolvedDefaults struct {
	identityURL  string
	serverAPIURL string
	registryURL  string
	namespace    string
	release      string
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
	return resolvedDefaults{
		identityURL:  firstNonEmpty(os.Getenv("MAGPIE_IDENTITY_URL"), target.IdentityURL, "http://identity.magpie.local"),
		serverAPIURL: firstNonEmpty(os.Getenv("MAGPIE_SERVER_API_URL"), target.ServerAPIURL, "http://server-api.magpie.local"),
		registryURL:  firstNonEmpty(os.Getenv("MAGPIE_REGISTRY_URL"), target.RegistryURL, "http://registry.magpie.local"),
		namespace:    firstNonEmpty(os.Getenv("MAGPIE_NAMESPACE"), target.Namespace, "magpie"),
		release:      firstNonEmpty(os.Getenv("MAGPIE_RELEASE"), target.Release, "arma"),
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
