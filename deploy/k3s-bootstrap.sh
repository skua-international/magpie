#!/usr/bin/env bash
# Guided setup for a magpie cluster: installs k3s on a fresh single-node
# host (or targets a k3s/k8s cluster you already have, via --kubeconfig),
# checks whether the hostPath directory backing sync-daemon's
# content/claims volumes supports reflink (only a warning if not -- it
# degrades claim() to a real copy rather than breaking anything), resolves
# the secrets the chart needs (auto-generating what can be auto-generated,
# prompting for what can't), and finishes by actually running
# scripts/deploy.sh --install -- the one place the real `helm` invocation
# lives, so this script never duplicates or drifts out of sync with it.
#
# Everything this chart deploys (sync-daemon's content/claims hostPath
# volumes, the controller's per-server hostPath server-roots, launcher
# Pods with hostNetwork: true) assumes a single node -- this script does
# not attempt to set up a multi-node cluster.
#
# Usage:
#   ./deploy/k3s-bootstrap.sh                                 # install k3s on this host, then deploy
#   ./deploy/k3s-bootstrap.sh --ssh user@host                 # install k3s on a remote host instead
#   ./deploy/k3s-bootstrap.sh --ssh user@host --ssh-identity ~/.ssh/id_ed25519
#   ./deploy/k3s-bootstrap.sh --kubeconfig ~/.kube/some-cluster.yaml  # skip k3s install, deploy onto an existing cluster
#
# --ssh runs the k3s-install half of this script on the remote host
# instead of locally. Without --ssh-identity, its contents are piped
# straight into `ssh ... bash -s` (one round trip, always exactly this
# version). With --ssh-identity, it's scp'd over to a temp path and
# executed from there instead -- a real file on the remote host is more
# robust than occupying the remote shell's stdin with the script body
# itself -- then removed; the identity is only ever used for the
# transfer/connection. Either way assumes the ssh target already has root
# or passwordless sudo (same assumption this environment's existing
# elephant_root-style host aliases already make) -- there's no TTY
# available for an interactive sudo password prompt over a non-
# interactive ssh command. ARMA_DATA_DIR/COPY_KUBECONFIG env var
# overrides are forwarded to the remote run if set locally. Once the
# remote run finishes, its kubeconfig is fetched back and saved locally
# (server URL rewritten from 127.0.0.1 to the real host), so the deploy
# step below (and kubectl/helm afterwards) can target the new cluster
# directly without an SSH session for every command.
#
# --kubeconfig skips k3s installation entirely (and the reflink
# filesystem check, since this host may not even be the cluster's node)
# and goes straight to secret resolution + deploy against whatever
# cluster that kubeconfig points at. Mutually exclusive with --ssh.
#
# Secrets:
#   - arma-postgres-creds: always auto-generated (a random password --
#     nothing to prompt for) unless --external-postgres-url is given.
#   - Steam auth: no password-based bootstrap Secret at all -- a Steam
#     password should never reach a deployed service, not even
#     transiently. Run `magpiectl admin refresh-steam-auth` after install
#     instead (QR-code login: scan it with the Steam mobile app, no
#     password typed anywhere), or --set syncDaemon.steamAuth.anonymous=true
#     for anonymous-only (public workshop content only) access.
#   - ghcr-pull-secret: only created if --ghcr-user/--ghcr-token (or
#     GHCR_USER/GHCR_TOKEN) are given -- optional, only needed if the
#     images live in a private registry (the default,
#     ghcr.io/skua-international/magpie/*, does).
#   - arma-oauth-creds (Discord/GitHub/Google login): not automated here,
#     shapes vary per-provider -- create it yourself if you want it, see
#     the printed instructions at the end. Steam login needs no app
#     credentials at all (OpenID 2.0) and works with none of this set.
#
# identity.baseUrl/ingress.baseDomain default to
# http://identity.magpie.local / magpie.local -- override with
# --identity-base-url/--ingress-base-domain if this host has a real
# domain, or if you're not using /etc/hosts on whatever machine needs to
# reach it.
set -euo pipefail

ARMA_DATA_DIR="${ARMA_DATA_DIR:-/var/lib/magpie}"
COPY_KUBECONFIG="${COPY_KUBECONFIG:-true}"
SSH_HOST=""
SSH_IDENTITY=""
EXISTING_KUBECONFIG=""
GHCR_USER="${GHCR_USER:-}"
GHCR_TOKEN="${GHCR_TOKEN:-}"
IDENTITY_BASE_URL="${IDENTITY_BASE_URL:-}"
INGRESS_BASE_DOMAIN="${INGRESS_BASE_DOMAIN:-magpie.local}"
EXTERNAL_POSTGRES_URL=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --ssh) SSH_HOST="$2"; shift 2 ;;
        --ssh-identity) SSH_IDENTITY="$2"; shift 2 ;;
        --kubeconfig) EXISTING_KUBECONFIG="$2"; shift 2 ;;
        --ghcr-user) GHCR_USER="$2"; shift 2 ;;
        --ghcr-token) GHCR_TOKEN="$2"; shift 2 ;;
        --identity-base-url) IDENTITY_BASE_URL="$2"; shift 2 ;;
        --ingress-base-domain) INGRESS_BASE_DOMAIN="$2"; shift 2 ;;
        --external-postgres-url) EXTERNAL_POSTGRES_URL="$2"; shift 2 ;;
        -h|--help)
            if [[ -f "${BASH_SOURCE[0]:-}" ]]; then
                grep '^#' "${BASH_SOURCE[0]:-}" | cut -c3-
            else
                echo "usage: see deploy/k3s-bootstrap.sh's own header at github.com/skua-international/magpie"
            fi
            exit 0
            ;;
        *)
            echo "error: unrecognized argument '$1' (see --help)" >&2
            exit 1
            ;;
    esac
done

if [[ -n "$SSH_HOST" && -n "$EXISTING_KUBECONFIG" ]]; then
    echo "error: --ssh and --kubeconfig are mutually exclusive" >&2
    exit 1
fi

log() { echo "[k3s-bootstrap] $*"; }

TEMP_FILES=()
cleanup() {
    [[ ${#TEMP_FILES[@]} -gt 0 ]] && rm -f "${TEMP_FILES[@]}"
}
trap cleanup EXIT

GITHUB_REPO="skua-international/magpie"

# This repo is private -- api.github.com/raw.githubusercontent.com both
# 404 (not 401/403, GitHub doesn't distinguish "private" from "doesn't
# exist" to an unauthenticated caller) without a token. `gh auth token`
# reuses whatever session `gh` already has rather than needing a
# separately exported GITHUB_TOKEN just to run this script.
GH_AUTH_HEADER=()
if command -v gh >/dev/null 2>&1; then
    gh_token="$(gh auth token 2>/dev/null || true)"
    [[ -n "$gh_token" ]] && GH_AUTH_HEADER=(-H "Authorization: token $gh_token")
fi

# Empty (not an error) when curl-piped -- ${BASH_SOURCE[0]:-} isn't a real
# path in that case. Every local-file lookup below is guarded on this
# being non-empty; scripts/deploy.sh is fetched fresh from GitHub instead
# when it is.
REPO_ROOT=""
if [[ -f "${BASH_SOURCE[0]:-}" ]]; then
    REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-}")/.." && pwd)"
fi

# The real path to *this script's own source* -- needed by --ssh below
# (piping/scp-ing it to the remote host). When curl-piped, ${BASH_SOURCE[0]:-}
# has already been fully consumed as bash's stdin and can't be re-read, so
# there's nothing to fall back to except downloading a fresh copy.
SELF_SCRIPT="${BASH_SOURCE[0]:-}"
if [[ ! -f "$SELF_SCRIPT" ]]; then
    SELF_SCRIPT="$(mktemp)"
    TEMP_FILES+=("$SELF_SCRIPT")
    curl -sSf "${GH_AUTH_HEADER[@]}" "https://raw.githubusercontent.com/${GITHUB_REPO}/main/deploy/k3s-bootstrap.sh" -o "$SELF_SCRIPT"
fi

# Set below to whichever kubeconfig this run ends up with -- so secret
# resolution and the deploy step further down work identically whether
# k3s was just installed locally, remotely via --ssh, or --kubeconfig
# pointed at a cluster that already existed.
KUBECONFIG_PATH=""

if [[ -n "$EXISTING_KUBECONFIG" ]]; then
    if [[ ! -f "$EXISTING_KUBECONFIG" ]]; then
        echo "error: --kubeconfig $EXISTING_KUBECONFIG does not exist" >&2
        exit 1
    fi
    log "using existing cluster via $EXISTING_KUBECONFIG -- skipping k3s install"
    KUBECONFIG_PATH="$EXISTING_KUBECONFIG"
elif [[ -n "$SSH_HOST" ]]; then
    ssh_args=(-o BatchMode=yes)
    [[ -n "$SSH_IDENTITY" ]] && ssh_args+=(-i "$SSH_IDENTITY")
    remote_env="ARMA_DATA_DIR=${ARMA_DATA_DIR@Q} COPY_KUBECONFIG=${COPY_KUBECONFIG@Q}"

    if [[ -n "$SSH_IDENTITY" ]]; then
        remote_script="/tmp/k3s-bootstrap.$$.sh"
        log "copying this script to $SSH_HOST:$remote_script..."
        scp "${ssh_args[@]}" "$SELF_SCRIPT" "$SSH_HOST:$remote_script"
        log "running it on $SSH_HOST..."
        ssh "${ssh_args[@]}" "$SSH_HOST" "$remote_env bash $remote_script; rm -f $remote_script"
    else
        log "running this script on $SSH_HOST over ssh..."
        ssh "${ssh_args[@]}" "$SSH_HOST" "$remote_env bash -s" < "$SELF_SCRIPT"
    fi

    if [[ "$COPY_KUBECONFIG" == "true" ]]; then
        log "fetching kubeconfig from $SSH_HOST..."
        remote_host="${SSH_HOST#*@}"
        mkdir -p "$HOME/.kube"
        KUBECONFIG_PATH="$HOME/.kube/config-${remote_host//[:.]/-}"
        ssh "${ssh_args[@]}" "$SSH_HOST" 'cat "$HOME/.kube/config"' \
            | sed -E "s#https://(127\.0\.0\.1|localhost):#https://${remote_host}:#" \
            > "$KUBECONFIG_PATH"
        chmod 600 "$KUBECONFIG_PATH"
        log "kubeconfig saved to $KUBECONFIG_PATH"
    fi
else
    if command -v k3s >/dev/null 2>&1; then
        log "k3s is already installed ($(k3s --version | head -n1)); skipping install"
    else
        log "installing k3s..."
        curl -sfL https://get.k3s.io | sh -
    fi

    log "waiting for the node to report Ready..."
    for _ in $(seq 1 60); do
        if k3s kubectl get nodes 2>/dev/null | grep -q ' Ready '; then
            break
        fi
        sleep 2
    done
    if ! k3s kubectl get nodes 2>/dev/null | grep -q ' Ready '; then
        log "node did not become Ready in time -- check 'systemctl status k3s' and 'journalctl -u k3s'"
        exit 1
    fi
    log "node is Ready"

    # Reflink CoW claims (steam_sync::claim) need this directory's
    # filesystem to support it -- btrfs, or XFS formatted with
    # reflink=1. Anything else still works, just falls back to a real
    # recursive copy for every claim, which is slower and uses more
    # disk. Warn, don't fail. Created here (not just left to Kubernetes'
    # own DirectoryOrCreate) so there's something to stat even before
    # the chart is installed.
    mkdir -p "$ARMA_DATA_DIR"
    fs_type=$(findmnt -no FSTYPE --target "$ARMA_DATA_DIR")
    case "$fs_type" in
        btrfs)
            log "$ARMA_DATA_DIR filesystem: btrfs (reflink supported)"
            ;;
        xfs)
            if xfs_info "$ARMA_DATA_DIR" 2>/dev/null | grep -q "reflink=1"; then
                log "$ARMA_DATA_DIR filesystem: xfs with reflink=1 (reflink supported)"
            else
                log "WARNING: $ARMA_DATA_DIR is xfs but wasn't formatted with reflink=1 -- claims will fall back to real copies. Reformat with 'mkfs.xfs -m reflink=1' if you want fast CoW claims."
            fi
            ;;
        *)
            log "WARNING: $ARMA_DATA_DIR's filesystem is '$fs_type', which doesn't support reflink -- claims will fall back to real copies (slower, more disk). btrfs or reflink-enabled xfs is recommended -- mount one at $ARMA_DATA_DIR before installing the chart if you want fast CoW claims."
            ;;
    esac

    if [ "$COPY_KUBECONFIG" = "true" ]; then
        mkdir -p "$HOME/.kube"
        KUBECONFIG_PATH="$HOME/.kube/config"
        k3s kubectl config view --raw > "$KUBECONFIG_PATH"
        chmod 600 "$KUBECONFIG_PATH"
        log "kubeconfig copied to $KUBECONFIG_PATH"
    else
        log "kubeconfig available via 'k3s kubectl', or at /etc/rancher/k3s/k3s.yaml -- deploy step below will be skipped since there's no local kubeconfig path to use"
    fi
fi

# --- Secret resolution --------------------------------------------------
# Everything below only runs if we actually have a kubeconfig to act
# against (always true for --kubeconfig/--ssh; only false for a local
# install with COPY_KUBECONFIG=false).
DEPLOY_SET_ARGS=()

# Must match the chart's own values.yaml `namespace` default -- every
# resource it creates lands here regardless of what namespace `helm
# install` itself was invoked with (see _helpers.tpl's magpie.namespace),
# so secrets created ahead of the chart need to target this same fixed
# name explicitly, not whatever the kubeconfig's current context defaults
# to.
CHART_NAMESPACE="magpie"

if [[ -n "$KUBECONFIG_PATH" ]]; then
    kctl() { KUBECONFIG="$KUBECONFIG_PATH" kubectl "$@"; }

    log "ensuring namespace $CHART_NAMESPACE exists..."
    kctl create namespace "$CHART_NAMESPACE" --dry-run=client -o yaml | kctl apply -f - >/dev/null

    if [[ -z "$EXTERNAL_POSTGRES_URL" ]]; then
        log "creating arma-postgres-creds (random password)..."
        # hex, not base64 -- this password gets embedded unescaped into
        # DATABASE_URL via $(PGPASSWORD) k8s dependent-env-var expansion
        # (see the chart's magpie.postgresEnv helper), which does raw
        # string substitution with no percent-encoding. base64's alphabet
        # includes "+"/"/"/"=" -- a "/" in particular corrupts a
        # postgres://user:pass@host:port/db URL badly enough to surface
        # as "invalid port number", confirmed live (see cli/magpie's own
        # randomPassword(), fixed the same way for the same reason).
        kctl create secret generic arma-postgres-creds -n "$CHART_NAMESPACE" \
            --from-literal=POSTGRES_PASSWORD="$(openssl rand -hex 24)" \
            --dry-run=client -o yaml | kctl apply -f - >/dev/null
        DEPLOY_SET_ARGS+=(--set postgres.existingSecret=arma-postgres-creds)
    else
        DEPLOY_SET_ARGS+=(--set "postgres.enabled=false" --set "postgres.externalUrl=$EXTERNAL_POSTGRES_URL")
    fi

    log "Steam auth: no password-based bootstrap Secret to create -- run 'magpiectl admin refresh-steam-auth' after install (QR-code login, no password ever touches the cluster), or --set syncDaemon.steamAuth.anonymous=true for anonymous-only access"

    if [[ -n "$GHCR_USER" && -n "$GHCR_TOKEN" ]]; then
        log "creating ghcr-pull-secret..."
        kctl create secret docker-registry ghcr-pull-secret -n "$CHART_NAMESPACE" \
            --docker-server=ghcr.io --docker-username="$GHCR_USER" --docker-password="$GHCR_TOKEN" \
            --dry-run=client -o yaml | kctl apply -f - >/dev/null
        DEPLOY_SET_ARGS+=(--set "imagePullSecrets={ghcr-pull-secret}")
    else
        log "no --ghcr-user/--ghcr-token given -- skipping pull secret (only needed for a private image registry)"
    fi

    DEPLOY_SET_ARGS+=(--set "ingress.baseDomain=$INGRESS_BASE_DOMAIN")
    DEPLOY_SET_ARGS+=(--set "identity.baseUrl=${IDENTITY_BASE_URL:-http://identity.$INGRESS_BASE_DOMAIN}")

    deploy_script="$REPO_ROOT/scripts/deploy.sh"
    if [[ -z "$REPO_ROOT" || ! -f "$deploy_script" ]]; then
        log "no local checkout -- fetching scripts/deploy.sh from GitHub..."
        deploy_script="$(mktemp)"
        TEMP_FILES+=("$deploy_script")
        curl -sSf "${GH_AUTH_HEADER[@]}" "https://raw.githubusercontent.com/${GITHUB_REPO}/main/scripts/deploy.sh" -o "$deploy_script"
        chmod +x "$deploy_script"
    fi
    log "running deploy.sh --install..."
    KUBECONFIG="$KUBECONFIG_PATH" bash "$deploy_script" --install -- "${DEPLOY_SET_ARGS[@]}"
else
    log "no kubeconfig available -- skipping secret creation and deploy. Set COPY_KUBECONFIG=true, or pass --kubeconfig, and re-run."
fi

cat <<EOF

Remaining manual steps, if you want them:

- (Optional) OAuth2 app credentials for identity (Discord/GitHub/Google
  login -- Steam needs none of this, OpenID 2.0):
    kubectl create secret generic arma-oauth-creds \\
      --from-literal=DISCORD_CLIENT_ID=<id> \\
      --from-literal=DISCORD_CLIENT_SECRET=<secret>
  then re-run deploy with --set identity.oauthSecret=arma-oauth-creds

- Sign in once, at <identity.baseUrl>/auth/steam/start (or
  /auth/discord/start, etc. -- whichever providers you configured), or
  via the magpiectl CLI/TUI (\`magpiectl login\`, or bare \`magpiectl\` -- see
  this repo's README for install instructions). The very first person to ever
  sign in is automatically granted every scope (there's no admin UI to
  hand out permissions any other way yet), so do this yourself before
  anyone else gets a chance to.

- Confirm everything came up:
    kubectl get pods -n magpie
    kubectl get crd armaservers.arma.skua.io modsources.arma.skua.io
    kubectl get ingress -n magpie

EOF
