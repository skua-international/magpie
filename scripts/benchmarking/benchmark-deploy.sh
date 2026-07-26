#!/usr/bin/env bash
# Nukes the golden-content workshop tree, restarts sync-daemon, watches the resync.
set -euo pipefail

export KUBECONFIG="${KUBECONFIG:-/home/grim/.kube/config-205-209-101-194}"
NAMESPACE="skua-magpie"
DEPLOY="skua-infra-sync-daemon"
LABEL="app.kubernetes.io/component=sync-daemon"
PVC="skua-infra-golden-content"
DEBUG_POD="nuke-workshop-debug"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# The pre-nuke measurement below reflects whatever's actually on disk right
# now, which can be a stale/partial number if the last run got interrupted
# mid-resync -- override with the real expected full-workshop size (GiB) to
# get an accurate ETA/progress instead. Empty = measure live, as before.
TARGET_GIB="${TARGET_GIB:-}"
SKIP_AUTH="${SKIP_AUTH:-0}"    # 1 = skip the QR-code refresh, reuse whatever session is already stored
NO_RESTART="${NO_RESTART:-0}"  # 1 = nuke workshop on the RUNNING pod (ephemeral container, shares its
                                # volumes -- no RWO conflict) and trigger sync via `magpiectl mods sync`
                                # for every source, instead of scaling to 0/1. Skips the whole
                                # auth dance entirely: no restart means no fresh Steam login at all,
                                # just the already-established in-memory session doing the work.

if [ "$NO_RESTART" = "1" ]; then
    echo "1/4 (NO_RESTART=1) skipping auth refresh and scale dance -- reusing the running pod's session"

    POD=$(kubectl get pod -n "$NAMESPACE" -l "$LABEL" -o jsonpath='{.items[0].metadata.name}')
    if [ -z "$POD" ]; then
        echo "no running sync-daemon pod found -- NO_RESTART needs one already up" >&2
        exit 1
    fi
    echo "target pod: ${POD}"

    # Unique per run -- ephemeral containers can never be removed, only
    # appended to, so a stale name from a previous NO_RESTART run would
    # collide.
    DEBUG_CONTAINER="nuke-debug-$(date -u +%s)"
    echo "2/4 attaching ephemeral debug container ${DEBUG_CONTAINER} to ${POD} (shares its volumes, no PVC re-mount needed)"
    # --custom needs kubectl 1.30+; securityContext matches the pod's own
    # runAsNonRoot policy (busybox's default user is root, which that
    # policy rejects outright).
    CUSTOM_SPEC="$(mktemp)"
    cat > "$CUSTOM_SPEC" <<'YAML'
volumeMounts:
  - name: golden-content
    mountPath: /content
securityContext:
  runAsUser: 65532
  runAsNonRoot: true
YAML
    kubectl debug -n "$NAMESPACE" "$POD" --image=busybox:1.36 --container="$DEBUG_CONTAINER" \
        --custom="$CUSTOM_SPEC" -- sh -c "sleep 300" >/dev/null
    rm -f "$CUSTOM_SPEC"

    for _ in $(seq 1 30); do
        state=$(kubectl get pod -n "$NAMESPACE" "$POD" -o jsonpath="{.status.ephemeralContainerStatuses[?(@.name=='${DEBUG_CONTAINER}')].state.running}" 2>/dev/null)
        [ -n "$state" ] && break
        sleep 1
    done

    echo "3/4 measuring then deleting /content/workshop"
    if [ -n "$TARGET_GIB" ]; then
        TARGET_TOTAL_KIB=$(awk -v g="$TARGET_GIB" 'BEGIN { printf "%.0f", g * 1048576 }')
        echo "using overridden target size: ${TARGET_GIB} GiB (${TARGET_TOTAL_KIB} KiB)"
    else
        TARGET_TOTAL_KIB=$(kubectl exec -n "$NAMESPACE" "$POD" -c "$DEBUG_CONTAINER" -- du -sk /content/workshop 2>/dev/null | awk '{print $1}')
        echo "pre-nuke workshop size: ${TARGET_TOTAL_KIB} KiB"
    fi
    kubectl exec -n "$NAMESPACE" "$POD" -c "$DEBUG_CONTAINER" -- sh -c 'rm -rf /content/workshop/* /content/workshop/.[!.]* 2>/dev/null; ls -la /content/workshop'
    # Ephemeral containers can't be deleted, only left to exit on their own --
    # it's just sleeping busybox, harmless to leave until the pod itself goes.

    echo "4/4 triggering sync for every mod source, then starting watcher"
    while read -r id _; do
        if [ -n "$id" ]; then
            # RefreshSource's turso upsert isn't fully serialized against
            # concurrent calls yet (hit "concurrent use forbidden" firing
            # these back-to-back with zero delay) -- a real bug worth
            # fixing in magpie itself, but staggering + tolerating a single
            # failure here keeps the benchmark itself unblocked meanwhile.
            magpiectl mods sync "$id" || echo "  (sync trigger for ${id} failed, continuing)"
            sleep 0.5
        fi
    done < <(magpiectl mods list)

    AUTO_STOP=1 TARGET_TOTAL_KIB="$TARGET_TOTAL_KIB" "${SCRIPT_DIR}/watch-sync-daemon.sh"
    exit 0
fi

if [ "$SKIP_AUTH" = "1" ]; then
    echo "1/6 skipping Steam auth refresh (SKIP_AUTH=1) -- reusing stored session"
else
    echo "1/6 refreshing Steam auth (scan the QR code when prompted)"
    # RefreshSteamAuth is an RPC served BY sync-daemon itself -- must run
    # before scaling to 0, or there's no pod left to receive the call.
    magpiectl admin refresh-steam-auth
fi

echo "2/6 scaling ${DEPLOY} to 0"
kubectl scale deploy -n "$NAMESPACE" "$DEPLOY" --replicas=0
kubectl wait --for=delete pod -n "$NAMESPACE" -l "$LABEL" --timeout=120s || true

echo "3/6 mounting golden-content PVC in a debug pod"
kubectl delete pod -n "$NAMESPACE" "$DEBUG_POD" --ignore-not-found --wait=true
cat <<EOF | kubectl apply -f -
apiVersion: v1
kind: Pod
metadata:
  name: ${DEBUG_POD}
  namespace: ${NAMESPACE}
spec:
  restartPolicy: Never
  containers:
    - name: debug
      image: busybox:1.36
      command: ["sh", "-c", "sleep 3600"]
      volumeMounts:
        - mountPath: /content
          name: golden-content
  volumes:
    - name: golden-content
      persistentVolumeClaim:
        claimName: ${PVC}
EOF
kubectl wait --for=condition=Ready pod -n "$NAMESPACE" "$DEBUG_POD" --timeout=90s

echo "4/6 measuring then deleting /content/workshop"
if [ -n "$TARGET_GIB" ]; then
    TARGET_TOTAL_KIB=$(awk -v g="$TARGET_GIB" 'BEGIN { printf "%.0f", g * 1048576 }')
    echo "using overridden target size: ${TARGET_GIB} GiB (${TARGET_TOTAL_KIB} KiB) -- actual pre-nuke size may differ if the last run was interrupted"
else
    TARGET_TOTAL_KIB=$(kubectl exec -n "$NAMESPACE" "$DEBUG_POD" -- du -sk /content/workshop 2>/dev/null | awk '{print $1}')
    echo "pre-nuke workshop size: ${TARGET_TOTAL_KIB} KiB"
fi
kubectl exec -n "$NAMESPACE" "$DEBUG_POD" -- sh -c 'rm -rf /content/workshop/* /content/workshop/.[!.]* 2>/dev/null; ls -la /content/workshop'
kubectl delete pod -n "$NAMESPACE" "$DEBUG_POD" --ignore-not-found --wait=true

echo "5/6 scaling ${DEPLOY} back to 1"
kubectl scale deploy -n "$NAMESPACE" "$DEPLOY" --replicas=1
kubectl rollout status deploy -n "$NAMESPACE" "$DEPLOY" --timeout=120s

echo "6/6 starting watcher (auto-stops once blob growth goes idle)"
AUTO_STOP=1 TARGET_TOTAL_KIB="$TARGET_TOTAL_KIB" "${SCRIPT_DIR}/watch-sync-daemon.sh"
