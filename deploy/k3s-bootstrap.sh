#!/usr/bin/env bash
# Self-contained single-node k3s install for a host that doesn't already
# have Kubernetes. Installs k3s, waits for the node to go Ready, checks
# whether the hostPath directory backing sync-daemon's content/claims
# volumes supports reflink (only a warning if not -- it degrades claim()
# to a real copy rather than breaking anything), and prints the
# `helm install` command to run next.
#
# Everything this chart deploys (sync-daemon's content/claims hostPath
# volumes, the controller's per-server hostPath server-roots, launcher
# Pods with hostNetwork: true) assumes a single node -- this script does
# not attempt to set up a multi-node cluster.
set -euo pipefail

# Must match the chart's values.yaml hostPaths.contentPath/claimsPath
# parent -- both need to be on the same real filesystem for reflink to
# engage at all, so this checks the shared parent, not either path
# individually. Override if you changed those values.
ARMA_DATA_DIR="${ARMA_DATA_DIR:-/var/lib/magpie}"
COPY_KUBECONFIG="${COPY_KUBECONFIG:-true}"

log() { echo "[k3s-bootstrap] $*"; }

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

# Reflink CoW claims (steam_sync::claim) need this directory's filesystem
# to support it -- btrfs, or XFS formatted with reflink=1. Anything else
# still works, just falls back to a real recursive copy for every claim,
# which is slower and uses more disk. Warn, don't fail. Created here (not
# just left to Kubernetes' own DirectoryOrCreate) so there's something to
# stat even before the chart is installed.
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
    k3s kubectl config view --raw > "$HOME/.kube/config"
    chmod 600 "$HOME/.kube/config"
    log "kubeconfig copied to $HOME/.kube/config (export KUBECONFIG=$HOME/.kube/config, or it's picked up by default)"
else
    log "kubeconfig available via 'k3s kubectl', or at /etc/rancher/k3s/k3s.yaml"
fi

cat <<EOF

k3s is up. Next steps:

1. Create the Steam credentials secret (skip if using --set syncDaemon.steamAuth.anonymous=true):
     kubectl create secret generic arma-steam-creds \\
       --from-literal=STEAM_USER=<user> \\
       --from-literal=STEAM_PASSWORD=<password>

2. Create the shared Postgres password secret (skip if pointing at an
   external Postgres via --set postgres.enabled=false,postgres.externalUrl=...):
     kubectl create secret generic arma-postgres-creds \\
       --from-literal=POSTGRES_PASSWORD="\$(openssl rand -base64 24)"

3. (Optional) Create an OAuth2 app credentials secret for the identity
   service -- Discord/GitHub/Google login, one key pair per provider you
   want, any subset. Steam login needs no credentials at all (OpenID 2.0)
   and works with none of this set:
     kubectl create secret generic arma-oauth-creds \\
       --from-literal=DISCORD_CLIENT_ID=<id> \\
       --from-literal=DISCORD_CLIENT_SECRET=<secret>

4. Install the chart. identity.baseUrl must be this service's real,
   externally-reachable URL (whoever's browser completes a login needs to
   reach it -- an in-cluster DNS name won't do). jwt.jwksUrl/issuer/
   audience default to this chart's own services/identity, so you
   normally don't need to touch them:
     helm install arma charts/magpie \\
       --set image.repository=<your-registry>/magpie \\
       --set image.tag=<tag> \\
       --set launcherImage=<your-registry>/arma3-launcher:<tag> \\
       --set syncDaemon.steamAuth.existingSecret=arma-steam-creds \\
       --set postgres.existingSecret=arma-postgres-creds \\
       --set identity.baseUrl=<https://id.your-domain> \\
       --set identity.oauthSecret=arma-oauth-creds

5. Sign in once, at <identity.baseUrl>/auth/steam/start (or
   /auth/discord/start, etc. -- whichever providers you configured). The
   very first person to ever sign in is automatically granted every scope
   (there's no admin UI to hand out permissions any other way yet), so do
   this yourself before anyone else gets a chance to.

6. Confirm everything came up:
     kubectl get pods
     kubectl get crd armaservers.arma.skua.io

EOF
