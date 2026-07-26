#!/usr/bin/env bash
# Polls sync-daemon's CPU/mem via `kubectl top` and appends CSV rows until killed.
set -euo pipefail

export KUBECONFIG="${KUBECONFIG:-/home/grim/.kube/config-205-209-101-194}"
NAMESPACE="skua-magpie"
LABEL="app.kubernetes.io/component=sync-daemon"
CSI_LABEL="app.kubernetes.io/component=magpie-csi-node"
BLOB_PATH="/var/lib/magpie-csi/blob/content.img"
INTERVAL="${INTERVAL:-10}"
RUN_TS="$(date -u +%Y%m%dT%H%M%SZ)"
OUT="${OUT:-$(pwd)/sync-daemon-usage-${RUN_TS}.csv}"
AUTO_STOP="${AUTO_STOP:-0}"      # 1 = exit once blob growth goes idle after having grown
IDLE_POLLS="${IDLE_POLLS:-6}"    # consecutive zero-growth polls (after growth seen) before auto-stop
NET_IFACE="${NET_IFACE:-enp38s0}"  # elephant's real uplink, not a veth/cni interface
NETPROBE_POD="netprobe-watch"
TARGET_TOTAL_KIB="${TARGET_TOTAL_KIB:-}"   # pre-nuke workshop size in KiB; enables progress/ETA
TARGET_MIB_S="${TARGET_MIB_S:-105}"        # assumed throughput (MiB/s) for the initial ETA guess

fmt_duration() {
    awk -v s="$1" 'BEGIN {
        neg=""; if (s<0) { neg="-"; s=-s }
        s=int(s+0.5)
        h=int(s/3600); m=int((s%3600)/60); sec=s%60
        if (h>0) printf "%s%dh%dm%ds", neg, h, m, sec
        else if (m>0) printf "%s%dm%ds", neg, m, sec
        else printf "%s%ds", neg, sec
    }'
}

eta_calc() {
    # args: downloaded_kib elapsed_s target_kib target_mib_s initial_eta_s
    awk -v dl="$1" -v el="$2" -v tgt="$3" -v tmibs="$4" -v init_eta="$5" '
    function fmt(s,    neg,h,m,sec) {
        neg=""; if (s<0) { neg="-"; s=-s }
        s=int(s+0.5)
        h=int(s/3600); m=int((s%3600)/60); sec=s%60
        if (h>0) return sprintf("%s%dh%dm%ds", neg, h, m, sec)
        else if (m>0) return sprintf("%s%dm%ds", neg, m, sec)
        else return sprintf("%s%ds", neg, sec)
    }
    BEGIN {
        if (tgt <= 0) { print "-,-,-,-,-"; exit }
        pct = (dl/tgt)*100
        if (pct > 100) pct = 100
        if (pct < 0) pct = 0
        cur_mibs = tmibs
        if (el > 0 && dl > 0) cur_mibs = (dl/1024)/el
        remaining_kib = tgt - dl
        if (remaining_kib < 0) remaining_kib = 0
        eta_remaining = (cur_mibs > 0) ? (remaining_kib/1024)/cur_mibs : -1
        projected_total = el + eta_remaining
        diff = projected_total - init_eta
        diff_pct = (init_eta > 0) ? (diff/init_eta)*100 : 0
        printf "%.1f,%s,%s,%s,%.1f", pct, fmt(eta_remaining), fmt(projected_total), fmt(diff), diff_pct
    }'
}

INITIAL_ETA_S=0
if [ -n "$TARGET_TOTAL_KIB" ]; then
    INITIAL_ETA_S=$(awk -v k="$TARGET_TOTAL_KIB" -v m="$TARGET_MIB_S" 'BEGIN { printf "%.0f", (m>0) ? (k/1024)/m : 0 }')
    target_gib=$(awk -v k="$TARGET_TOTAL_KIB" 'BEGIN { printf "%.2f", k/1048576 }')
    echo "target: ${target_gib} GiB at assumed ${TARGET_MIB_S} MiB/s -> initial ETA ~$(fmt_duration "$INITIAL_ETA_S")"
fi

kubectl delete pod -n "$NAMESPACE" "$NETPROBE_POD" --ignore-not-found --wait=true >/dev/null 2>&1 || true
cat <<EOF | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata:
  name: ${NETPROBE_POD}
  namespace: ${NAMESPACE}
spec:
  hostNetwork: true
  restartPolicy: Never
  containers:
    - name: debug
      image: busybox:1.36
      command: ["sh", "-c", "sleep 86400"]
EOF
kubectl wait --for=condition=Ready pod -n "$NAMESPACE" "$NETPROBE_POD" --timeout=60s >/dev/null
cleanup() { kubectl delete pod -n "$NAMESPACE" "$NETPROBE_POD" --wait=false >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

echo "timestamp,elapsed_human,pod,cpu_millicores,mem_mib,restarts,blob_actual_gib,throughput_human,node_cpu_millicores,node_mem_mib,net_rx_human,net_tx_human,progress_pct,eta_remaining,projected_total,eta_diff,eta_diff_pct" | tee "$OUT"

prev_ts_epoch=""
prev_blob_kib=""
saw_growth=0
idle_count=0
run_start_epoch=$(date -u +%s)
first_blob_kib=""
prev_net_ts_epoch=""
prev_rx_bytes=""
prev_tx_bytes=""

while true; do
    ts=$(date -u +%Y-%m-%dT%H:%M:%SZ)
    ts_epoch=$(date -u +%s)

    top_line=$(kubectl top pod -n "$NAMESPACE" -l "$LABEL" --no-headers) || true
    restarts=$(kubectl get pod -n "$NAMESPACE" -l "$LABEL" -o jsonpath='{.items[0].status.containerStatuses[0].restartCount}' 2>/dev/null) || true

    node_line=$(kubectl top node --no-headers 2>/dev/null | awk '{cpu=$2; mem=$4; sub(/m/,"",cpu); sub(/Mi/,"",mem); print cpu","mem; exit}') || true
    node_cpu=${node_line%%,*}
    node_mem=${node_line##*,}

    csi_pod=$(kubectl get pod -n "$NAMESPACE" -l "$CSI_LABEL" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null) || true
    blob_kib=""
    if [ -n "$csi_pod" ]; then
        blob_kib=$(kubectl exec -n "$NAMESPACE" "$csi_pod" -c magpie-csi -- du -k "$BLOB_PATH" 2>/dev/null | awk '{print $1}') || true
    fi

    blob_gib=""
    throughput_human="-"
    dk=0
    if [ -n "$blob_kib" ]; then
        [ -z "$first_blob_kib" ] && first_blob_kib="$blob_kib"
        blob_gib=$(awk -v k="$blob_kib" 'BEGIN { printf "%.2f", k / 1048576 }')
        if [ -n "$prev_blob_kib" ] && [ -n "$prev_ts_epoch" ]; then
            dt=$((ts_epoch - prev_ts_epoch))
            dk=$((blob_kib - prev_blob_kib))
            if [ "$dt" -gt 0 ] && [ "$dk" -ge 0 ]; then
                throughput_human=$(awk -v dk="$dk" -v dt="$dt" 'BEGIN {
                    mib_s = (dk / 1024) / dt
                    if (mib_s >= 1024) printf "%.2f GiB/s (%.0f Mbps)", mib_s / 1024, mib_s * 8.388608
                    else printf "%.1f MiB/s (%.0f Mbps)", mib_s, mib_s * 8.388608
                }')
            fi
        fi
        prev_blob_kib="$blob_kib"
        prev_ts_epoch="$ts_epoch"
    fi

    net_line=$(kubectl exec -n "$NAMESPACE" "$NETPROBE_POD" -- sh -c "cat /proc/net/dev | grep '${NET_IFACE}:'" 2>/dev/null) || true
    net_rx_human="-"
    net_tx_human="-"
    if [ -n "$net_line" ]; then
        rx_bytes=$(echo "$net_line" | awk -F: '{print $2}' | awk '{print $1}')
        tx_bytes=$(echo "$net_line" | awk -F: '{print $2}' | awk '{print $9}')
        if [ -n "$prev_rx_bytes" ] && [ -n "$prev_net_ts_epoch" ]; then
            net_dt=$((ts_epoch - prev_net_ts_epoch))
            drx=$((rx_bytes - prev_rx_bytes))
            dtx=$((tx_bytes - prev_tx_bytes))
            if [ "$net_dt" -gt 0 ]; then
                net_rx_human=$(awk -v d="$drx" -v s="$net_dt" 'BEGIN {
                    mib_s = (d / 1048576) / s
                    if (mib_s >= 1024) printf "%.2f GiB/s (%.0f Mbps)", mib_s / 1024, mib_s * 8.388608
                    else printf "%.1f MiB/s (%.0f Mbps)", mib_s, mib_s * 8.388608
                }')
                net_tx_human=$(awk -v d="$dtx" -v s="$net_dt" 'BEGIN {
                    mib_s = (d / 1048576) / s
                    if (mib_s >= 1024) printf "%.2f GiB/s (%.0f Mbps)", mib_s / 1024, mib_s * 8.388608
                    else printf "%.1f MiB/s (%.0f Mbps)", mib_s, mib_s * 8.388608
                }')
            fi
        fi
        prev_rx_bytes="$rx_bytes"
        prev_tx_bytes="$tx_bytes"
        prev_net_ts_epoch="$ts_epoch"
    fi

    elapsed_s=$((ts_epoch - run_start_epoch))
    elapsed_human=$(fmt_duration "$elapsed_s")

    progress_pct="-"; eta_remaining="-"; projected_total="-"; eta_diff="-"; eta_diff_pct="-"
    if [ -n "$TARGET_TOTAL_KIB" ] && [ -n "$blob_kib" ] && [ -n "$first_blob_kib" ]; then
        downloaded_kib=$((blob_kib - first_blob_kib))
        [ "$downloaded_kib" -lt 0 ] && downloaded_kib=0
        IFS=',' read -r progress_pct eta_remaining projected_total eta_diff eta_diff_pct <<< \
            "$(eta_calc "$downloaded_kib" "$elapsed_s" "$TARGET_TOTAL_KIB" "$TARGET_MIB_S" "$INITIAL_ETA_S")"
    fi

    if [ -n "$top_line" ]; then
        while read -r pod cpu mem; do
            cpu_num=${cpu%m}
            mem_num=${mem%Mi}
            echo "${ts},${elapsed_human},${pod},${cpu_num},${mem_num},${restarts},${blob_gib},${throughput_human},${node_cpu},${node_mem},${net_rx_human},${net_tx_human},${progress_pct},${eta_remaining},${projected_total},${eta_diff},${eta_diff_pct}" | tee -a "$OUT"
        done <<< "$top_line"
    else
        echo "${ts},${elapsed_human},,,,${restarts},${blob_gib},${throughput_human},${node_cpu},${node_mem},${net_rx_human},${net_tx_human},${progress_pct},${eta_remaining},${projected_total},${eta_diff},${eta_diff_pct}" | tee -a "$OUT"
    fi

    if [ "$AUTO_STOP" = "1" ]; then
        if [ "$dk" -gt 0 ]; then
            saw_growth=1
            idle_count=0
        elif [ "$saw_growth" = "1" ]; then
            idle_count=$((idle_count + 1))
        fi
        if [ "$saw_growth" = "1" ] && [ "$idle_count" -ge "$IDLE_POLLS" ]; then
            run_end_epoch=$(date -u +%s)
            elapsed=$((run_end_epoch - run_start_epoch))
            total_kib=$((blob_kib - first_blob_kib))
            avg_human=$(awk -v k="$total_kib" -v s="$elapsed" 'BEGIN {
                if (s <= 0) { print "n/a"; exit }
                mib_s = (k / 1024) / s
                if (mib_s >= 1024) printf "%.2f GiB/s (%.0f Mbps) avg", mib_s / 1024, mib_s * 8.388608
                else printf "%.1f MiB/s (%.0f Mbps) avg", mib_s, mib_s * 8.388608
            }')
            summary="idle for $((IDLE_POLLS * INTERVAL))s, stopping. elapsed=$(fmt_duration "$elapsed") ${avg_human}"
            if [ -n "$TARGET_TOTAL_KIB" ]; then
                diff_s=$((elapsed - INITIAL_ETA_S))
                diff_pct=$(awk -v d="$diff_s" -v i="$INITIAL_ETA_S" 'BEGIN { print (i>0) ? sprintf("%.1f", (d/i)*100) : "n/a" }')
                summary="${summary} vs target ETA $(fmt_duration "$INITIAL_ETA_S"): diff=$(fmt_duration "$diff_s") (${diff_pct}%)"
            fi
            echo "$summary" | tee -a "$OUT"
            exit 0
        fi
    fi

    sleep "$INTERVAL"
done
