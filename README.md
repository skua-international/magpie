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

All inter-service and client-facing APIs are [ConnectRPC](https://connectrpc.com/) (`proto/`). Cluster state lives in one shared Postgres instance (mod sources, missions, ACL grants, identities) plus `hostPath` volumes sized for this project's single-node k3s target — see `charts/magpie/values.yaml`'s `hostPaths` comment for why that's a deliberate choice, not just the easy one.

## Deploying

```bash
./deploy/k3s-bootstrap.sh   # installs k3s on a fresh single-node host, prints the helm install command
```

The script walks through the required secrets (Steam credentials, the shared Postgres password, optional OAuth2 app credentials) and installs `charts/magpie`. See the chart's `values.yaml` for every knob — most have sane defaults.

## Repo layout

```
crates/            shared library crates (steam-sync, registry-db, authn, protocol, crd, ...)
services/          the six binaries described above
proto/             ConnectRPC service definitions
charts/magpie/      the Helm chart
deploy/            k3s-bootstrap.sh
configs/           example main.cfg/basic.cfg to seed a server's operator-provided config directory
```

## Development

```bash
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
```

Each service has its own `Dockerfile` (via `cargo-chef`, built from the repo root as build context — every service needs the full workspace's `Cargo.lock`/manifests to resolve). Images are built and pushed to `ghcr.io/skua-international/magpie/*` by `.github/workflows/build-images.yml` on push to `main` and on version tags.
