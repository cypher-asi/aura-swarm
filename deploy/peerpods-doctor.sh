#!/bin/bash
# peerpods-doctor.sh - Read-only, layered diagnostic for the kata-remote / Peer
# Pods (Cloud API Adaptor) chain. It probes each micro-step IN DEPENDENCY ORDER
# and prints a PASS/FAIL/SKIP checklist, then names the EARLIEST failing layer
# (the likely root cause). Use it to pinpoint exactly where a kata-remote pod
# breaks WITHOUT re-running the mutating 03 step or burning a billable pod VM.
#
# It is strictly READ-ONLY: it never patches, restarts, or deletes anything, and
# it never launches a pod VM. Remediation commands are PRINTED, not run.
#
# Layers:
#   Cluster (one-shot):
#     platform        gateway/control/scheduler deployments Ready
#     runtimeclass    kata-remote RuntimeClass exists + overhead zero
#     caa-daemonset   cloud-api-adaptor-daemonset Ready N/N
#     webhook         peer-pods mutating webhook effective (caBundle + endpoints)
#     vm-capacity     workers advertise kata.peerpods.io/vm
#   Per node (via the privileged guest-pull DaemonSet pod's host namespaces):
#     hypervisor-socket  /run/peerpod/hypervisor.sock present
#     fuse               mount.fuse present (nydus-overlayfs)
#     guest-pull-flags   disable_snapshot_annotations=false + discard_unpacked_layers=false
#     nydus-bound        kata-remote snapshotter is a containerd plugin loaded "ok"
#     nydus-daemon       nydus guest-pull daemon socket present + process running
#     host-image-cache   harness workload image NOT host-cached (must guest-pull)
#
# Usage:
#   ./peerpods-doctor.sh                 # all worker nodes
#   ./peerpods-doctor.sh --node NODE     # one node (e.g. ip-10-0-3-31.us-east-2.compute.internal)
#   ./peerpods-doctor.sh --help

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

ONLY_NODE=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --node) ONLY_NODE="$2"; shift 2 ;;
        -h|--help)
            echo "Usage: $0 [--node NODE]"
            echo "  Read-only layered diagnostic for the kata-remote / Peer Pods chain."
            echo "  --node NODE   probe only this worker node (default: all workers)"
            exit 0
            ;;
        *) echo "Unknown option: $1"; echo "Usage: $0 [--node NODE]"; exit 1 ;;
    esac
done

step_banner "--" "Peer Pods (kata-remote) layered doctor" "ops-admin"

require_cmds kubectl jq aws
require_aws_auth
ensure_kubectl_context

CAA_NS="${CAA_NAMESPACE:-confidential-containers-system}"
CAA_DS="${CAA_DAEMONSET:-cloud-api-adaptor-daemonset}"
GP_LABEL="app.kubernetes.io/name=kata-remote-containerd-guestpull"

#------------------------------------------------------------------------------
# Checklist plumbing (same pattern as 08-r1-soak-check.sh).
#------------------------------------------------------------------------------
declare -a CHECK_NAMES=()
declare -a CHECK_RESULTS=()
declare -a CHECK_NOTES=()

record() { # record <name> <PASS|FAIL|SKIP> <note>
    CHECK_NAMES+=("$1")
    CHECK_RESULTS+=("$2")
    CHECK_NOTES+=("${3:-}")
    case "$2" in
        PASS) log_ok "$1 ${3:-}" ;;
        SKIP) log_warn "$1 skipped ${3:-}" ;;
        *)    log_err "$1 ${3:-}" ;;
    esac
}

#------------------------------------------------------------------------------
# Host command on NODE through its guest-pull DaemonSet pod's PID1 namespaces.
# Echoes stdout; returns 3 when the node has no Ready guest-pull pod to exec in
# (the caller then falls back to parsing that pod's logs).
#------------------------------------------------------------------------------
guestpull_pod_on_node() { # <node> -> pod name (Ready or not), empty if none
    kubectl -n "${CAA_NS}" get pod -l "${GP_LABEL}" \
        --field-selector "spec.nodeName=$1" \
        -o jsonpath='{.items[0].metadata.name}' 2>/dev/null | tr -d '\r'
}

pod_is_ready() { # <pod>
    local s
    s=$(kubectl -n "${CAA_NS}" get pod "$1" \
        -o jsonpath='{range .status.conditions[?(@.type=="Ready")]}{.status}{end}' 2>/dev/null | tr -d '\r')
    [[ "${s}" == "True" ]]
}

host_exec() { # <pod> <cmd...>
    local pod="$1"; shift
    MSYS_NO_PATHCONV=1 kubectl -n "${CAA_NS}" exec "${pod}" -- \
        nsenter -t 1 -m -u -i -n -p -- "$@" 2>/dev/null
}

#------------------------------------------------------------------------------
# Cluster layers
#------------------------------------------------------------------------------
log_section "Cluster layers"

# platform: control-plane deployments Ready.
PLAT_OK=true
PLAT_BAD=""
for d in aura-swarm-gateway aura-swarm-control aura-swarm-scheduler; do
    r=$(kubectl get deployment "${d}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo 0)
    [[ "${r:-0}" -ge 1 ]] || { PLAT_OK=false; PLAT_BAD+="${d} "; }
done
if [[ "${PLAT_OK}" == "true" ]]; then
    record "platform" PASS "gateway/control/scheduler Ready"
else
    record "platform" FAIL "not ready: ${PLAT_BAD}"
fi

# runtimeclass: kata-remote exists + overhead zero.
if kubectl get runtimeclass kata-remote >/dev/null 2>&1; then
    if kata_remote_overhead_is_zero; then
        record "runtimeclass" PASS "kata-remote exists, overhead zero"
    else
        record "runtimeclass" FAIL "non-zero overhead $(kata_remote_overhead) (scheduler adds it -> 'Insufficient cpu'); re-run ./03-coco-operator.sh"
    fi
else
    record "runtimeclass" FAIL "RuntimeClass kata-remote missing; run ./03-coco-operator.sh"
fi

# caa-daemonset: Ready N/N (one-shot read; no waiting).
DS_JSON=$(kubectl -n "${CAA_NS}" get ds "${CAA_DS}" -o json 2>/dev/null || echo '{}')
DS_DESIRED=$(printf '%s' "${DS_JSON}" | jq -r '.status.desiredNumberScheduled // 0')
DS_READY=$(printf '%s' "${DS_JSON}" | jq -r '.status.numberReady // 0')
if [[ "${DS_DESIRED:-0}" -ge 1 && "${DS_READY:-0}" -eq "${DS_DESIRED:-0}" ]]; then
    record "caa-daemonset" PASS "${DS_READY}/${DS_DESIRED} Ready"
else
    record "caa-daemonset" FAIL "${DS_READY:-0}/${DS_DESIRED:-0} Ready"
    dump_daemonset_diagnostics "${CAA_NS}" "${CAA_DS}"
fi

# webhook: peer-pods mutating webhook effective.
WH_DIAG=""; WH_RC=0
WH_DIAG=$(peerpods_webhook_effective) || WH_RC=$?
if [[ ${WH_RC} -eq 0 ]]; then
    record "webhook" PASS "mutating webhook effective"
elif [[ ${WH_RC} -eq 2 ]]; then
    record "webhook" FAIL "no peer-pods MutatingWebhookConfiguration registered; run ./03-coco-operator.sh"
else
    record "webhook" FAIL "registered but NOT effective (empty caBundle or no endpoints); fix cert-manager injection"
fi
[[ -n "${WH_DIAG}" ]] && printf '%s\n' "${WH_DIAG}" | indent

# vm-capacity: every worker advertises kata.peerpods.io/vm >= 1.
CAP_JSON=$(kubectl get nodes -l node.kubernetes.io/worker -o json 2>/dev/null || echo '{}')
CAP_TOTAL=$(printf '%s' "${CAP_JSON}" | jq '[.items[]] | length')
CAP_OK=$(printf '%s' "${CAP_JSON}" | jq '[.items[] | select(((.status.allocatable["kata.peerpods.io/vm"] // "0") | tonumber? // 0) >= 1)] | length')
if [[ "${CAP_TOTAL:-0}" -ge 1 && "${CAP_OK:-0}" -eq "${CAP_TOTAL:-0}" ]]; then
    record "vm-capacity" PASS "${CAP_OK}/${CAP_TOTAL} workers advertise kata.peerpods.io/vm"
else
    record "vm-capacity" FAIL "${CAP_OK:-0}/${CAP_TOTAL:-0} workers advertise kata.peerpods.io/vm; re-run ./03-coco-operator.sh"
    peerpods_node_capacity_summary
fi

#------------------------------------------------------------------------------
# Resolve the pinned harness ref (for the host-image-cache probe).
#------------------------------------------------------------------------------
HARNESS_REF="${AURA_HARNESS_IMAGE:-}"
if [[ -z "${HARNESS_REF}" && -f "${HARNESS_STATE_FILE}" ]]; then
    HARNESS_REF=$(grep '^AURA_HARNESS_IMAGE=' "${HARNESS_STATE_FILE}" 2>/dev/null \
        | head -1 | cut -d= -f2- | tr -d '\r' || true)
fi
HARNESS_DIGEST="${HARNESS_REF##*@}"   # sha256:... (or the whole ref if no @)
HARNESS_REPO="${HARNESS_REF%@*}"; HARNESS_REPO="${HARNESS_REPO##*/}"  # e.g. aura-swarm-dev-harness

#------------------------------------------------------------------------------
# Per-node host layers
#------------------------------------------------------------------------------
if [[ -n "${ONLY_NODE}" ]]; then
    NODES=("${ONLY_NODE}")
else
    mapfile -t NODES < <(kubectl get nodes -l node.kubernetes.io/worker \
        -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' 2>/dev/null | tr -d '\r' | sed '/^$/d')
fi

# Parse the snapshotter the kata-remote runtime uses out of a containerd dump.
snap_from_dump() { # reads dump on stdin
    awk -F= '
        /runtimes\.kata-remote\]/{r=1;next}
        r && /^[[:space:]]*\[/{r=0}
        r && /snapshotter[[:space:]]*=/{v=$2; gsub(/[" \t]/,"",v); print v; exit}'
}
# Parse the proxy_plugins socket address for a given snapshotter name.
sock_from_dump() { # <snap>; reads dump on stdin
    awk -F= -v n="$1" '
        $0 ~ /proxy_plugins/ && $0 ~ n && /\]/ {p=1;next}
        p && /^[[:space:]]*\[/{p=0}
        p && /address[[:space:]]*=/{v=$2; gsub(/[" \t]/,"",v); print v; exit}'
}

for node in "${NODES[@]}"; do
    [[ -z "${node}" ]] && continue
    short="${node%%.*}"
    log_section "Node ${short}"

    GP_POD="$(guestpull_pod_on_node "${node}")"
    CAN_EXEC=false
    if [[ -n "${GP_POD}" ]] && pod_is_ready "${GP_POD}"; then
        CAN_EXEC=true
    fi

    # When we cannot exec (no guest-pull pod, or it is CrashLooping because the
    # nydus-bind guard hit exit 1), parse the pod's own logs for the markers the
    # guest-pull configurer prints, so the layers still report a real finding.
    GP_LOGS=""
    if [[ "${CAN_EXEC}" == "false" && -n "${GP_POD}" ]]; then
        GP_LOGS="$(kubectl -n "${CAA_NS}" logs "${GP_POD}" --tail=200 2>/dev/null; \
                   kubectl -n "${CAA_NS}" logs "${GP_POD}" --previous --tail=200 2>/dev/null || true)"
    fi

    if [[ -z "${GP_POD}" ]]; then
        for L in hypervisor-socket fuse guest-pull-flags nydus-bound nydus-daemon host-image-cache; do
            record "${short}/${L}" SKIP "no guest-pull pod on node (is it a worker? is the DaemonSet applied?)"
        done
        continue
    fi

    DUMP=""
    [[ "${CAN_EXEC}" == "true" ]] && DUMP="$(host_exec "${GP_POD}" containerd config dump || true)"
    SNAP=""
    [[ -n "${DUMP}" ]] && SNAP="$(printf '%s' "${DUMP}" | snap_from_dump)"

    # hypervisor-socket
    if [[ "${CAN_EXEC}" == "true" ]]; then
        if host_exec "${GP_POD}" ls /run/peerpod/hypervisor.sock >/dev/null 2>&1; then
            record "${short}/hypervisor-socket" PASS "/run/peerpod/hypervisor.sock present"
        else
            record "${short}/hypervisor-socket" FAIL "/run/peerpod/hypervisor.sock missing; kata-remote sandbox creation will fail. Re-run ./03-coco-operator.sh (clean-restarts the CAA pod)"
        fi
    else
        record "${short}/hypervisor-socket" SKIP "guest-pull pod not Ready; cannot exec (see its logs)"
    fi

    # fuse
    if [[ "${CAN_EXEC}" == "true" ]]; then
        if host_exec "${GP_POD}" sh -c 'command -v mount.fuse' >/dev/null 2>&1; then
            record "${short}/fuse" PASS "mount.fuse present"
        else
            record "${short}/fuse" FAIL "mount.fuse missing; nydus-overlayfs mount fails. Re-run ./03-coco-operator.sh (fuse installer DaemonSet)"
        fi
    else
        record "${short}/fuse" SKIP "guest-pull pod not Ready; cannot exec"
    fi

    # guest-pull-flags
    if [[ -n "${DUMP}" ]]; then
        if printf '%s' "${DUMP}" | grep -qE 'disable_snapshot_annotations[[:space:]]*=[[:space:]]*false' \
           && printf '%s' "${DUMP}" | grep -qE 'discard_unpacked_layers[[:space:]]*=[[:space:]]*false'; then
            record "${short}/guest-pull-flags" PASS "disable_snapshot_annotations=false, discard_unpacked_layers=false"
        else
            record "${short}/guest-pull-flags" FAIL "guest-pull flags not both false; workload unpacks on host -> 'content digest not found'. Re-run ./03-coco-operator.sh"
        fi
    elif [[ -n "${GP_LOGS}" ]] && printf '%s' "${GP_LOGS}" | grep -q 'guest-pull flags now effective'; then
        record "${short}/guest-pull-flags" PASS "(from guest-pull pod logs) flags effective"
    elif [[ -n "${GP_LOGS}" ]] && printf '%s' "${GP_LOGS}" | grep -q 'guest-pull flags still not effective'; then
        record "${short}/guest-pull-flags" FAIL "(from logs) flags still not effective after restart"
    else
        record "${short}/guest-pull-flags" SKIP "guest-pull pod not Ready and no marker in logs"
    fi

    # nydus-bound: the kata-remote snapshotter must be a containerd plugin loaded "ok".
    if [[ "${CAN_EXEC}" == "true" && -n "${SNAP}" && "${SNAP}" != "overlayfs" ]]; then
        PLUGINS="$(host_exec "${GP_POD}" ctr -a /run/containerd/containerd.sock plugins ls || true)"
        if printf '%s' "${PLUGINS}" | awk -v n="${SNAP}" '$2==n && $NF=="ok"{f=1} END{exit f?0:1}'; then
            record "${short}/nydus-bound" PASS "snapshotter ${SNAP} loaded ok"
        else
            ST="$(printf '%s' "${PLUGINS}" | awk -v n="${SNAP}" '$2==n{print $NF}')"
            record "${short}/nydus-bound" FAIL "snapshotter ${SNAP} not bound (status=${ST:-absent}); image pulls fall back to host overlayfs. Restart containerd on ${short}: sudo systemctl restart containerd (or re-run ./03-coco-operator.sh to roll the guest-pull DS)"
        fi
    elif [[ "${CAN_EXEC}" == "true" && "${SNAP}" == "overlayfs" ]]; then
        record "${short}/nydus-bound" FAIL "kata-remote uses the default overlayfs snapshotter, not a nydus guest-pull snapshotter; kata-deploy did not wire it"
    elif [[ "${CAN_EXEC}" == "true" && -z "${SNAP}" ]]; then
        record "${short}/nydus-bound" SKIP "kata-remote runtime/snapshotter not in containerd config (kata-deploy not applied on this node?)"
    elif [[ -n "${GP_LOGS}" ]] && printf '%s' "${GP_LOGS}" | grep -qiE 'snapshotter .* (already )?bound|is bound \(ok\)'; then
        record "${short}/nydus-bound" PASS "(from logs) snapshotter bound"
    elif [[ -n "${GP_LOGS}" ]] && printf '%s' "${GP_LOGS}" | grep -qi 'still not bound after restart'; then
        record "${short}/nydus-bound" FAIL "(from logs) snapshotter still not bound after restart; nydus daemon socket likely missing"
    else
        record "${short}/nydus-bound" SKIP "guest-pull pod not Ready and no marker in logs"
    fi

    # nydus-daemon: proxy socket present + process running.
    if [[ "${CAN_EXEC}" == "true" && -n "${SNAP}" && "${SNAP}" != "overlayfs" ]]; then
        SOCK="$(printf '%s' "${DUMP}" | sock_from_dump "${SNAP}")"
        SOCK_OK=false; PROC_OK=false
        [[ -n "${SOCK}" ]] && host_exec "${GP_POD}" ls "${SOCK}" >/dev/null 2>&1 && SOCK_OK=true
        host_exec "${GP_POD}" sh -c 'pgrep -f nydus >/dev/null 2>&1 || pgrep -f containerd-nydus-grpc >/dev/null 2>&1' && PROC_OK=true
        if [[ "${SOCK_OK}" == "true" && "${PROC_OK}" == "true" ]]; then
            record "${short}/nydus-daemon" PASS "daemon running, socket ${SOCK:-?} present"
        else
            record "${short}/nydus-daemon" FAIL "nydus guest-pull daemon down (socket present=${SOCK_OK}, process running=${PROC_OK}); kata-deploy nydus snapshotter problem"
        fi
    else
        record "${short}/nydus-daemon" SKIP "guest-pull pod not Ready or no nydus snapshotter configured"
    fi

    # host-image-cache: the harness workload image must NOT be cached on the
    # worker (it should be guest-pulled inside the pod VM). A host-cached copy is
    # what makes containerd reuse an incomplete host snapshot -> 'content digest
    # not found' even after the snapshotter binds.
    if [[ "${CAN_EXEC}" != "true" ]]; then
        record "${short}/host-image-cache" SKIP "guest-pull pod not Ready; cannot inspect host content store"
    elif [[ -z "${HARNESS_REF}" ]]; then
        record "${short}/host-image-cache" SKIP "no pinned harness image resolved (AURA_HARNESS_IMAGE / .last-harness-image.env)"
    else
        IMAGES="$(host_exec "${GP_POD}" ctr -n k8s.io -a /run/containerd/containerd.sock images ls -q || true)"
        if printf '%s' "${IMAGES}" | grep -qF "${HARNESS_DIGEST}" \
           || { [[ -n "${HARNESS_REPO}" ]] && printf '%s' "${IMAGES}" | grep -qF "${HARNESS_REPO}"; }; then
            record "${short}/host-image-cache" FAIL "harness image is HOST-cached on ${short} (should guest-pull). Remove it so the next pod guest-pulls: nsenter -t 1 -- ctr -n k8s.io images rm \$(ctr -n k8s.io images ls -q | grep ${HARNESS_REPO:-harness})"
        else
            record "${short}/host-image-cache" PASS "harness image not host-cached (guest-pull path clear)"
        fi
    fi
done

#------------------------------------------------------------------------------
# Checklist + verdict
#------------------------------------------------------------------------------
log_section "Peer Pods Doctor Checklist"
FAILS=0
FIRST_FAIL=""
for i in "${!CHECK_NAMES[@]}"; do
    case "${CHECK_RESULTS[$i]}" in
        PASS) echo -e "  ${GREEN}PASS${NC}  ${CHECK_NAMES[$i]}  ${CHECK_NOTES[$i]}" ;;
        SKIP) echo -e "  ${YELLOW}SKIP${NC}  ${CHECK_NAMES[$i]}  ${CHECK_NOTES[$i]}" ;;
        *)    echo -e "  ${RED}FAIL${NC}  ${CHECK_NAMES[$i]}  ${CHECK_NOTES[$i]}"
              FAILS=$((FAILS + 1))
              [[ -z "${FIRST_FAIL}" ]] && FIRST_FAIL="${CHECK_NAMES[$i]}" ;;
    esac
done
echo ""

if [[ ${FAILS} -gt 0 ]]; then
    log_err "${FAILS} layer(s) failed"
    log_detail "Root cause (earliest failing layer): ${FIRST_FAIL}"
    log_detail "Later layers depend on earlier ones — fix ${FIRST_FAIL} first, then re-run this doctor."
    exit 1
fi
log_ok "All probed layers healthy"
