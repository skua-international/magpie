#!/usr/bin/env bash
# Deploys a specific *published* version of the magpie stack to whatever
# k8s/k3s cluster the current kubectl context points at. Pulls the chart
# from its OCI publish (ghcr.io/.../charts/magpie) rather than the local
# checkout -- that's what makes this reproducible regardless of what
# branch/commit happens to be checked out on this host: it always
# deploys the exact artifact CI published for VERSION, never a locally-
# edited template that hasn't gone through CI.
#
# Images default to that same VERSION too, with no extra flag needed --
# charts/magpie's own image.tag default falls back to .Chart.AppVersion
# (see templates/_helpers.tpl's magpie.image), and build-images.yml tags
# every image with the exact version out of Chart.yaml on every push
# (see that workflow's "Resolve version" step). Use --image-tag to
# override just the images while keeping a given chart version, e.g. to
# roll out an unreleased commit's short-sha build for testing.
#
# Usage:
#   scripts/deploy.sh [VERSION] [options] [-- extra helm args]
#
#   VERSION defaults to whatever's in this checkout's own
#   charts/magpie/Chart.yaml if run from one -- i.e. with no arguments,
#   deploys the published version matching this checkout. Also runnable
#   with no checkout at all (curl -sSf .../scripts/deploy.sh | bash -s --
#   ..., see README.md), in which case it defaults to the latest GitHub
#   release instead.
#
# Options:
#   --image-tag TAG    Override just the image tag (chart stays at VERSION)
#   --namespace NS     Target namespace (default: magpie)
#   --release NAME     Helm release name (default: arma)
#   --timeout DURATION Helm --wait timeout (default: 10m)
#   --install          Allow a first install (helm upgrade --install) instead
#                       of requiring an existing release -- you'll need to
#                       supply every required value yourself (see below),
#                       since there's no prior release to carry values over
#                       from. Pass them after `--`, e.g.:
#                         scripts/deploy.sh --install -- \
#                           --set identity.baseUrl=http://identity.magpie.local \
#                           --set postgres.existingSecret=arma-postgres-creds \
#                           --set imagePullSecrets='{ghcr-pull-secret}'
#                       (No Steam auth flag needed at install time at all --
#                       there's no password-based bootstrap Secret anymore.
#                       Run `magpiectl admin refresh-steam-auth` after install
#                       instead (QR-code login, no password ever touches the
#                       cluster), or --set syncDaemon.steamAuth.anonymous=true
#                       for anonymous-only access. ingress.baseDomain defaults
#                       to magpie.local; override it too if your host uses a
#                       different one.)
#   --dry-run          Render and diff without actually applying anything
#   --debug            Pass Helm's verbose/debug output through for chart
#                      pull, values fetch, and upgrade
#
# Anything after a literal `--` is passed straight through to `helm
# upgrade` as extra arguments (more --set/-f overrides, etc.) -- applied
# on top of the previous release's own values (unless --install), so this
# is for one-off overrides, not a substitute for values actually worth
# keeping (put those in charts/magpie/values.yaml instead).

set -euo pipefail

# Empty (not an error) when curl-piped -- ${BASH_SOURCE[0]:-} isn't a real
# path in that case (typically "bash" or "/dev/stdin"), so there's no
# local checkout to find a default VERSION or anything else in. Every
# local-file lookup below is guarded on this being non-empty.
REPO_ROOT=""
if [[ -f "${BASH_SOURCE[0]:-}" ]]; then
    REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]:-}")/.." && pwd)"
fi
CHART_REF="oci://ghcr.io/skua-international/magpie/charts/magpie"
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

NAMESPACE="magpie"
RELEASE="arma"
WAIT_TIMEOUT="10m"
VERSION=""
IMAGE_TAG=""
INSTALL=""
HELM_DRY_RUN=""
HELM_DEBUG_ARGS=()
KUBECTL_DRY_RUN=""
EXTRA_ARGS=()

ghcr_token() {
  local token="${GHCR_TOKEN:-${GITHUB_TOKEN:-}}"

  if [[ -z "$token" ]] && command -v gh >/dev/null 2>&1; then
    token="$(gh auth token 2>/dev/null || true)"
  fi

  printf '%s' "$token"
}

ghcr_refresh_auth() {
  if ! command -v gh >/dev/null 2>&1; then
    echo "error: gh is required to refresh GitHub auth for GHCR access" >&2
    exit 1
  fi

  echo "==> Refreshing GitHub auth for read:packages (your browser may open)"
  gh auth refresh --hostname github.com --scopes read:packages
}

ghcr_manifest_status() {
  local repository="$1"
  local tag="$2"
  local token="$3"

  curl -sS -o /dev/null -w '%{http_code}' \
    --connect-timeout 10 \
    --max-time 30 \
    -H "Authorization: Bearer $token" \
    -H 'Accept: application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json' \
    "https://ghcr.io/v2/${repository}/manifests/${tag}"
}

ghcr_manifest_exists() {
  local repository="$1"
  local tag="$2"
  local token
  local http_code

  token="$(ghcr_token)"
  if [[ -z "$token" ]]; then
    ghcr_refresh_auth
    token="$(ghcr_token)"
  fi

  http_code="$(ghcr_manifest_status "$repository" "$tag" "$token")" || {
    echo "error: unable to query ghcr.io for ${repository}:${tag}" >&2
    exit 1
  }

  if [[ "$http_code" == "401" || "$http_code" == "403" ]]; then
    ghcr_refresh_auth
    token="$(ghcr_token)"
    http_code="$(ghcr_manifest_status "$repository" "$tag" "$token")" || {
      echo "error: unable to query ghcr.io for ${repository}:${tag} after auth refresh" >&2
      exit 1
    }
  fi

  if [[ "$http_code" != "200" ]]; then
    echo "error: missing ghcr.io/${repository}:${tag} (registry returned HTTP ${http_code})" >&2
    exit 1
  fi
}

check_required_images() {
  local tag="$1"

  echo "==> Preflighting GHCR image tags for $tag"
  ghcr_manifest_exists "skua-international/magpie/controller" "$tag"
  ghcr_manifest_exists "skua-international/magpie/identity" "$tag"
  ghcr_manifest_exists "skua-international/magpie/registry" "$tag"
  ghcr_manifest_exists "skua-international/magpie/server-api" "$tag"
  ghcr_manifest_exists "skua-international/magpie/sync-daemon" "$tag"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --image-tag) IMAGE_TAG="$2"; shift 2 ;;
    --namespace) NAMESPACE="$2"; shift 2 ;;
    --release) RELEASE="$2"; shift 2 ;;
    --timeout) WAIT_TIMEOUT="$2"; shift 2 ;;
    --install) INSTALL="1"; shift ;;
    --dry-run) HELM_DRY_RUN="--dry-run"; KUBECTL_DRY_RUN="--dry-run=client"; shift ;;
    --debug) HELM_DEBUG_ARGS=(--debug); shift ;;
    -h|--help)
      if [[ -f "${BASH_SOURCE[0]:-}" ]]; then
        grep '^#' "${BASH_SOURCE[0]:-}" | cut -c3-
      else
        echo "usage: see scripts/deploy.sh's own header at github.com/skua-international/magpie"
      fi
      exit 0
      ;;
    --)
      shift
      EXTRA_ARGS=("$@")
      break
      ;;
    *)
      if [[ -n "$VERSION" ]]; then
        echo "error: unexpected extra argument '$1' (did you mean to put it after --?)" >&2
        exit 1
      fi
      VERSION="$1"
      shift
      ;;
  esac
done

if [[ -z "$VERSION" ]]; then
  if [[ -n "$REPO_ROOT" && -f "$REPO_ROOT/charts/magpie/Chart.yaml" ]]; then
    VERSION="$(grep '^version:' "$REPO_ROOT/charts/magpie/Chart.yaml" | awk '{print $2}')"
    echo "==> No version given, using this checkout's own chart version: $VERSION"
  else
    VERSION="$(curl -sSf "${GH_AUTH_HEADER[@]}" "https://api.github.com/repos/${GITHUB_REPO}/releases/latest" | grep '"tag_name"' | sed -E 's/.*"v?([^"]+)".*/\1/')"
    if [[ -z "$VERSION" ]]; then
      echo "error: no VERSION given and couldn't resolve the latest release from GitHub -- pass one explicitly" >&2
      exit 1
    fi
    echo "==> No version given and no local checkout, using the latest release: $VERSION"
  fi
fi

IMAGE_CHECK_TAG="$VERSION"
if [[ -n "$IMAGE_TAG" ]]; then
  IMAGE_CHECK_TAG="$IMAGE_TAG"
fi

check_required_images "$IMAGE_CHECK_TAG"

if [[ -z "$INSTALL" ]] && ! helm status "$RELEASE" -n "$NAMESPACE" >/dev/null 2>&1; then
  echo "error: no existing '$RELEASE' release in namespace '$NAMESPACE' to upgrade." >&2
  echo "Pass --install for a first install (see --help for the values you'll need to supply)." >&2
  exit 1
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT

echo "==> Pulling $CHART_REF version $VERSION"
helm pull "$CHART_REF" --version "$VERSION" --untar --destination "$WORKDIR" \
  "${HELM_DEBUG_ARGS[@]}"
CHART_DIR="$WORKDIR/magpie"

HELM_ARGS=(--namespace "$NAMESPACE" --wait --timeout "$WAIT_TIMEOUT")

if [[ -z "$INSTALL" ]]; then
  echo "==> Carrying over $RELEASE's existing user-supplied values"
  VALUES_FILE="$WORKDIR/current-values.yaml"
  helm get values "$RELEASE" -n "$NAMESPACE" -o yaml \
    "${HELM_DEBUG_ARGS[@]}" > "$VALUES_FILE"
  HELM_ARGS+=(-f "$VALUES_FILE")
else
  echo "==> --install: no prior release, only using --set/-f values passed after --"
  HELM_ARGS+=(--create-namespace)
fi

if [[ -n "$IMAGE_TAG" ]]; then
  echo "==> Overriding image tag: $IMAGE_TAG"
  HELM_ARGS+=(--set "image.tag=$IMAGE_TAG")
fi

# Helm's crds/ convention is install-time only -- `helm upgrade` never
# touches it even if the chart's CRDs changed (a deliberate Helm safety
# choice, not a bug: https://helm.sh/docs/chart_best_practices/custom_resource_definitions/).
# Applying explicitly keeps CRDs (e.g. new ModSource printer columns/
# fields) in sync on every deploy of an *existing* release. Only for
# upgrades, deliberately -- `helm install` (what --install triggers when
# there's no prior release) already installs crds/ itself, automatically,
# as part of a first install; doing this unconditionally used to also run
# it before a first install too, which then made helm's own CRD install
# fail outright (two different apply mechanisms -- kubectl's own field
# manager vs. helm's internal one -- fighting over the same object,
# confirmed live against magpiectl's own Go port of this same logic).
if [[ -z "$INSTALL" ]]; then
  echo "==> Applying CRDs"
  kubectl apply -f "$CHART_DIR/crds/" ${KUBECTL_DRY_RUN:+"$KUBECTL_DRY_RUN"}
fi

VERB="upgrade"
[[ -n "$INSTALL" ]] && VERB="upgrade --install"

echo "==> helm $VERB $RELEASE -> $VERSION"
# shellcheck disable=SC2086 # $VERB is a deliberate two-word verb, not a single token
helm $VERB "$RELEASE" "$CHART_DIR" \
  "${HELM_ARGS[@]}" \
  "${EXTRA_ARGS[@]}" \
  "${HELM_DEBUG_ARGS[@]}" \
  ${HELM_DRY_RUN:+"$HELM_DRY_RUN"}

if [[ -z "$HELM_DRY_RUN" ]]; then
  echo "==> Deployed. Pods:"
  kubectl get pods -n "$NAMESPACE" -o wide
fi
