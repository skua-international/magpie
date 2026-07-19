# MAGPIE

**M**anaged **A**rma **G**eneral **P**ile of **I**ntricate **E**ngineering.

Kubernetes-native orchestration for Arma 3 dedicated servers: mod/collection syncing with copy-on-write claims, a CRD-driven reconciler, JWT-authenticated provisioning APIs, and its own OAuth2/Steam OpenID identity broker. Built as a Cargo workspace of small, single-purpose services deployed together via one Helm chart.

## Architecture

```
                         ┌──────────────┐
   Discord bot, etc. ───▶│  server-api  │───▶ ArmaServer (CRD)
                         └──────────────┘           │
                                                      ▼
                         ┌──────────────┐     ┌──────────────┐
                    ───▶│   registry   │     │  controller  │
                         └──────┬───────┘     │ (reconciler) │
                                │              └──────┬───────┘
                                ▼                     ▼
                         ┌──────────────┐     launcher Deployment
                    ───▶│  sync-daemon │◀───────────┘
                         └──────────────┘
                                │
                    Steam CM session, reflink claims

                         ┌──────────────┐
                    ───▶│   identity   │──▶ issues the JWTs every
                         └──────────────┘    JWT-gated service above verifies
```

- **`launcher`** — launches the game server (and headless clients) from an already-synced claim plus an operator-provided server config. No Steam logic of its own.
- **`sync-daemon`** — owns all Steam depot/workshop mechanics: authenticated CM session, mod/collection resolution (including private/unlisted content), and reflink (copy-on-write) claims of one shared golden content tree, so every server gets its own cheap, isolated snapshot instead of sharing live state.
- **`controller`** — a `kube-runtime` reconciler that turns `ArmaServer` custom resources into launcher `Deployment`s. No external listener, no auth surface, no Kubernetes RBAC beyond `ArmaServer` + `Deployment`.
- **`server-api`** — JWT-authenticated `ArmaServer` CRUD plus deployment-like lifecycle (`Create`/`Start`/`Stop`/`Update`). Deliberately carries the least Kubernetes RBAC of anything here (no `Deployment` access at all) — it's the one process a stolen bearer token can reach directly.
- **`registry`** — JWT-authenticated mod source registry (a Steam mod, a collection, a preset export, or a locally-uploaded zip mod) and mission (`.pbo`) storage. Never touches the Kubernetes API.
- **`identity`** — OAuth2 login (Discord/GitHub/Google) and Steam login (OpenID 2.0 — Steam never adopted OIDC), account linking, and JWT issuance for every JWT-gated service above to verify. The first person to ever sign in is automatically granted every permission.

All inter-service and client-facing APIs are [ConnectRPC](https://connectrpc.com/) (`proto/`). Mod sources (`ModSource`) and servers (`ArmaServer`) are Kubernetes CRDs, not database rows — `kubectl get modsources`/`kubectl get armaservers` shows live state directly, and `sync-daemon`/`controller` reconcile them the same way any other operator would. Postgres holds what's left: missions, ACL grants, and identities. Everything sits on `hostPath` volumes sized for this project's single-node k3s target — see `charts/magpie/values.yaml`'s `hostPaths` comment for why that's a deliberate choice, not just the easy one.

## Deploying

This repo is private, so every command below that talks to GitHub (not GHCR/OCI, which uses its own pull secret) picks up auth from `gh auth token` automatically if the [GitHub CLI](https://cli.github.com/) is installed and logged in (`gh auth login`) -- no separate token to export.

First install, on a fresh host with no Kubernetes yet -- either from a checkout:

```bash
./deploy/k3s-bootstrap.sh   # installs k3s, checks the data dir's filesystem, walks through required secrets
```

or with no checkout at all, piped straight from GitHub like most install.sh-style tools do it:

```bash
curl -sSf -H "Authorization: token $(gh auth token)" \
  https://raw.githubusercontent.com/skua-international/magpie/main/deploy/k3s-bootstrap.sh | bash
```

Both are the same script -- piping it in just skips needing a clone first, and it fetches `scripts/deploy.sh` (and, for `--ssh`, itself) fresh from GitHub the same way when it can't find a local copy. It ends by actually running `scripts/deploy.sh --install` for you, with every `--set` it resolved along the way.

Upgrades (including the very first install, via `--install`) all go through one script, same pattern:

```bash
./scripts/deploy.sh                       # deploy this checkout's own chart version, same image tag
./scripts/deploy.sh 1.5.0                 # deploy a specific published version
./scripts/deploy.sh --image-tag c5130c2   # keep the chart version, swap just the images (e.g. testing an unreleased commit)
./scripts/deploy.sh --dry-run             # render + preflight without applying anything

# or, with no checkout:
curl -sSf -H "Authorization: token $(gh auth token)" \
  https://raw.githubusercontent.com/skua-international/magpie/main/scripts/deploy.sh | bash -s -- 1.5.0
```

It pulls the chart from its OCI publish (`oci://ghcr.io/skua-international/magpie/charts/magpie`) rather than the local checkout, so it always deploys the exact artifact CI published for that version — never a locally-edited template that hasn't gone through CI. With no VERSION and no local checkout to default from, it resolves the latest GitHub release instead. See `scripts/deploy.sh --help` for the rest (namespace/release name overrides, preflighting that the target image tags actually exist in GHCR before touching anything, etc). See the chart's `values.yaml` for every knob — most have sane defaults.

### Steam authentication

There's no password-based bootstrap Secret at all -- a Steam password should never reach a deployed service, not even transiently, and it should never reach *this* CLI either. Run `magpiectl admin refresh-steam-auth` instead: it prints a QR code (and the equivalent `https://s.team/q/1/...` URL, for a device that already has the Steam app installed) and does the login entirely client-side, on your own machine. Scan it with the Steam mobile app on whichever account you want the cluster to use. Only the resulting refresh token ever reaches the cluster (written to the `arma-steam-session` Secret via the `RefreshSteamAuth` RPC, which only ever accepts a refresh token, never a password), and that's what sync-daemon uses as the cluster's Steam identity for every workshop/depot operation from then on.

This isn't elevated access — visibility into private/unlisted mods and collections is scoped to whatever that specific Steam account can actually see (its own subscriptions, friends-only shares, etc.), same as browsing the Workshop as that account normally would. Use an account that's actually subscribed to / has visibility into whatever private content the servers need.

## Installing the CLI

`magpiectl` is both a direct CLI (`magpiectl servers list`, `magpiectl deploy`, ...) and, run with no subcommand, an interactive TUI — everything the TUI can do is also a direct invocation, and vice versa (see `cli/magpie/internal/actions`, the one implementation both surfaces call into). It can also drive the cluster's own lifecycle directly (`magpiectl deploy`/`upgrade`/`install`, shelling out to `helm`/`kubectl` the same way `scripts/deploy.sh` does) and establish the cluster's Steam session (`magpiectl admin refresh-steam-auth`, see "Steam authentication" above) without needing a repo checkout at all.

Download a prebuilt binary from [the latest release](https://github.com/skua-international/magpie/releases/latest). This repo is private, so the plain `.../releases/latest/download/...` URL 404s without auth -- resolve the actual asset via the API instead (needs `gh auth login` once, and `jq`):

```bash
# Linux/macOS -- picks the right archive for your OS/arch automatically
os="$(uname -s | tr '[:upper:]' '[:lower:]')"
arch="$(uname -m | sed -e 's/x86_64/amd64/' -e 's/aarch64/arm64/')"
asset="magpiectl_${os}_${arch}.tar.gz"
token="$(gh auth token)"
url="$(curl -sSf -H "Authorization: token $token" https://api.github.com/repos/skua-international/magpie/releases/latest \
  | jq -r --arg name "$asset" '.assets[] | select(.name == $name) | .url')"
curl -sSf -H "Authorization: token $token" -H 'Accept: application/octet-stream' -L "$url" \
  | tar xz -C /usr/local/bin magpiectl
```

Windows: same API dance (asset name `magpiectl_windows_amd64.zip`), or just download it from the releases page directly if you're already logged in to GitHub in your browser.

Then point it at your cluster and log in:

```bash
magpiectl --identity-url http://identity.magpie.local \
          --server-api-url http://server-api.magpie.local \
          --registry-url http://registry.magpie.local \
          auth login
magpiectl completion install   # optional -- bash/zsh/fish/powershell autocompletion
```

(Those base URLs are this chart's own `ingress.baseDomain` default, `magpie.local` — override to match whatever you actually set at install time.)

Building from source instead (e.g. to run an unreleased commit): `cd cli/magpie && go install ./cmd/magpiectl` -- only works from within a clone of this monorepo, since `cli/magpie`'s `go.mod` points at `generated/go` via a relative `replace` directive rather than a published module.

## Repo layout

```
crates/            shared library crates (steam-sync, registry-db, authn, protocol, crd, ...)
services/          the six binaries described above
cli/magpie/        the magpie CLI/TUI (Go)
proto/             ConnectRPC service definitions
charts/magpie/      the Helm chart
deploy/            k3s-bootstrap.sh
scripts/           deploy.sh, bump-version.sh
configs/           example main.cfg/basic.cfg to seed a server's operator-provided config directory
```

## Development

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

Each service has its own `Dockerfile` (via `cargo-chef`, built from the repo root as build context — every service needs the full workspace's `Cargo.lock`/manifests to resolve). Images are built and pushed to `ghcr.io/skua-international/magpie/*` by `.github/workflows/build-images.yml` on push to `main` and on version tags.
