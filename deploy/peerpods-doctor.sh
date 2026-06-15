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
LIVE=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --node) ONLY_NODE="$2"; shift 2 ;;
        --live) LIVE=true; shift ;;
        -h|--help)
            echo "Usage: $0 [--node NODE] [--live]"
            echo "  Layered diagnostic for the kata-remote / Peer Pods chain."
            echo "  --node NODE   probe only this worker node (default: all workers)"
            echo "  --live        ALSO launch a throwaway kata-remote pod and assert it"
            echo "                creates + reaches Running (validates guest-pull end to"
            echo "                end). COSTS a billable EC2 pod VM per run; self-cleaning."
            echo "                Override the test image with DOCTOR_LIVE_IMAGE."
            echo ""
            echo "  Read-only by default (cluster + pod + per-node host layers). Only"
            echo "  --live mutates (creates/deletes a throwaway pod + reaps its pod VM)."
            exit 0
            ;;
        *) echo "Unknown option: $1"; echo "Usage: $0 [--node NODE] [--live]"; exit 1 ;;
    esac
done

step_banner "--" "Peer Pods (kata-remote) layered doctor" "ops-admin"

require_cmds kubectl jq aws
require_aws_auth
ensure_kubectl_context

CAA_NS="${CAA_NAMESPACE:-confidential-containers-system}"
CAA_DS="${CAA_DAEMONSET:-cloud-api-adaptor-daemonset}"
GP_LABEL="app.kubernetes.io/name=kata-remote-containerd-guestpull"
# The containerd runtime-handler that kata-remote pods must request via the
# io.containerd.cri.runtime-handler annotation == the RuntimeClass .handler.
RC_HANDLER="$(kubectl get runtimeclass kata-remote -o jsonpath='{.handler}' 2>/dev/null | tr -d '\r' || true)"
RC_HANDLER="${RC_HANDLER:-kata-remote}"

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
    # Hard-bound the exec: a node whose containerd is wedged (e.g. mid
    # CreateContainerError storm) makes `containerd config dump` / `ctr` block
    # forever, which used to hang the whole read-only doctor (and survive
    # Ctrl-C in Git-Bash). `timeout -k` force-kills the client so the layer
    # degrades to SKIP/FAIL instead of hanging. Tune via DOCTOR_EXEC_TIMEOUT_SECS.
    MSYS_NO_PATHCONV=1 timeout -k 5 "${DOCTOR_EXEC_TIMEOUT_SECS:-25}" \
        kubectl -n "${CAA_NS}" exec "${pod}" -- \
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

# runtimeclass: kata-remote EXISTS. This is a genuine prerequisite, so it stays
# early in the dependency order. The OVERHEAD-zero sub-check is a DOWNSTREAM
# scheduling concern (it is only zeroed by ./03 AFTER the CAA daemonset is Ready),
# so it is probed as a separate `runtimeclass-overhead` layer AFTER caa-daemonset
# below — otherwise a non-zero overhead (the normal state when ./03 aborted at the
# CAA gate) is mis-ranked as the earliest/root failing layer.
if kubectl get runtimeclass kata-remote >/dev/null 2>&1; then
    record "runtimeclass" PASS "kata-remote RuntimeClass exists"
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

# runtimeclass-overhead: the kata-remote RuntimeClass must carry ZERO pod
# overhead. The scheduler ADDS .overhead.podFixed to every kata-remote pod's
# effective requests, so a non-zero overhead makes (otherwise 0-cpu) peer-pod
# agents go Unschedulable "Insufficient cpu". This is checked AFTER caa-daemonset
# on purpose: ./03 only zeros the overhead once the CAA daemonset is Ready (it
# step_fails at the readiness gate before reaching the zeroing step), so a
# non-zero overhead is almost always a SYMPTOM of an unhealthy caa-daemonset, not
# the root cause — fix caa-daemonset first.
if kubectl get runtimeclass kata-remote >/dev/null 2>&1; then
    if kata_remote_overhead_is_zero; then
        record "runtimeclass-overhead" PASS "overhead zero (peer-pods schedules via kata.peerpods.io/vm)"
    elif [[ "${DS_DESIRED:-0}" -ge 1 && "${DS_READY:-0}" -ne "${DS_DESIRED:-0}" ]]; then
        record "runtimeclass-overhead" FAIL "non-zero overhead $(kata_remote_overhead) (scheduler adds it -> 'Insufficient cpu'). This is downstream of the failing caa-daemonset (./03 zeros it only AFTER CAA is Ready) — fix caa-daemonset first."
    else
        record "runtimeclass-overhead" FAIL "non-zero overhead $(kata_remote_overhead) (scheduler adds it -> 'Insufficient cpu'). Re-run ./03-coco-operator.sh, or patch it directly: kubectl patch runtimeclass kata-remote --type=merge -p '{\"overhead\":{\"podFixed\":{\"cpu\":\"0\",\"memory\":\"0\"}}}'"
    fi
else
    record "runtimeclass-overhead" SKIP "RuntimeClass kata-remote missing (see runtimeclass)"
fi

# agent-pull-secret: kata-remote agents pull the workload image INSIDE the pod VM
# (CoCo image_guest_pull), so the GUEST needs registry credentials — the worker
# node-role's ECR access is not used. ./03 seeds a dockerconfigjson secret in the
# agent namespace and links it to the default SA the agent pods use; without it
# every confidential agent fails CreateContainer with "[CDH] Image Pull error:
# ... Not authorized" (the fleet/caa-log layers see it reactively — this catches
# it proactively, before any agent is launched).
AGENT_NS="${K8S_NAMESPACE_AGENTS:-swarm-agents}"
PULL_SECRET="${AGENT_ECR_PULL_SECRET_NAME:-ecr-harness-pull}"
if kubectl get namespace "${AGENT_NS}" >/dev/null 2>&1; then
    SA_REFS="$(kubectl get serviceaccount default -n "${AGENT_NS}" \
        -o jsonpath='{range .imagePullSecrets[*]}{.name}{"\n"}{end}' 2>/dev/null | tr -d '\r')"
    if ! kubectl get secret "${PULL_SECRET}" -n "${AGENT_NS}" >/dev/null 2>&1; then
        record "agent-pull-secret" FAIL "pull secret ${PULL_SECRET} missing in ${AGENT_NS}; in-guest ECR pull will fail 'Not authorized'. Re-run ./03-coco-operator.sh (it seeds it + installs the refresher CronJob)."
    elif ! printf '%s\n' "${SA_REFS}" | grep -qxF "${PULL_SECRET}"; then
        record "agent-pull-secret" FAIL "secret ${PULL_SECRET} exists but the default SA in ${AGENT_NS} does not reference it (new agents won't get it). Re-run ./03-coco-operator.sh, or: kubectl patch serviceaccount default -n ${AGENT_NS} -p '{\"imagePullSecrets\":[{\"name\":\"${PULL_SECRET}\"}]}'"
    else
        record "agent-pull-secret" PASS "${PULL_SECRET} present in ${AGENT_NS} and linked to the default SA"
    fi
else
    record "agent-pull-secret" SKIP "namespace ${AGENT_NS} does not exist yet (deploy not past ./04)"
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

# vm-headroom: vm-capacity only proves nodes ADVERTISE slots; this proves a slot
# is actually FREE. A pool can advertise 10/node yet be 100% consumed — often by
# failed/stuck pods squatting slots they will never use — so new agents go
# Unschedulable "Insufficient kata.peerpods.io/vm". This is the false-green the
# plain capacity check missed.
HEADROOM_TXT="$(peerpods_vm_headroom node.kubernetes.io/worker)"; HEADROOM_RC=$?
case "${HEADROOM_RC}" in
    0) record "vm-headroom" PASS "$(printf '%s' "${HEADROOM_TXT}" | awk '/TOTAL/{sub(/^[[:space:]]+/,"");print;exit}')" ;;
    2) record "vm-headroom" SKIP "no workers advertise kata.peerpods.io/vm" ;;
    *) record "vm-headroom" FAIL "0 free kata.peerpods.io/vm slots — new agents cannot schedule. Reclaim failed/stuck pods (below) or grow the pool (TEE_NODE_DESIRED_COUNT / CAA_PEERPODS_LIMIT_PER_NODE)"
       printf '%s\n' "${HEADROOM_TXT}" | indent ;;
esac

# worker-labels: nodes that belong to a managed node group but never got the
# node.kubernetes.io/worker label (late-joiners after ./03's one-time labeling)
# get no CAA pod and advertise zero capacity — invisible dead weight the plain
# capacity check (which filters on that very label) cannot see.
UNLABELED_NODES="$(peerpods_unlabeled_workers || true)"
if [[ -z "${UNLABELED_NODES}" ]]; then
    record "worker-labels" PASS "all node-group nodes carry node.kubernetes.io/worker"
else
    record "worker-labels" FAIL "node(s) in a node group missing node.kubernetes.io/worker (no CAA -> 0 VM capacity). Re-run ./03-coco-operator.sh (or ./07 self-heals via ensure_worker_labels)"
    printf '%s\n' "${UNLABELED_NODES}" | sed 's/^/    /'
fi

#------------------------------------------------------------------------------
# Application layers (pod-level, cluster-wide)
#------------------------------------------------------------------------------
log_section "Application layers"

# One snapshot of every kata-remote pod (used by the next two checks).
ALL_PODS_JSON="$(kubectl get pods -A -o json 2>/dev/null || echo '{}')"

# runtime-handler-annotation: every kata-remote pod MUST carry
# io.containerd.cri.runtime-handler=<RC handler>. On containerd 1.7 this is what
# routes the workload image pull to the kata-remote nydus (guest-pull)
# snapshotter; without it the image unpacks to overlayfs and create fails with
# "content digest ...: not found". A pod missing it was built by a scheduler
# without the fix (stale) -> redeploy the scheduler.
KR_TOTAL="$(printf '%s' "${ALL_PODS_JSON}" | jq --arg h "${RC_HANDLER}" \
    '[.items[] | select(.spec.runtimeClassName=="kata-remote")] | length' 2>/dev/null || echo 0)"
KR_MISSING_LIST="$(printf '%s' "${ALL_PODS_JSON}" | jq -r --arg h "${RC_HANDLER}" \
    '[.items[] | select(.spec.runtimeClassName=="kata-remote")
       | select((.metadata.annotations["io.containerd.cri.runtime-handler"] // "") != $h)
       | "\(.metadata.namespace)/\(.metadata.name)"] | .[]' 2>/dev/null || true)"
KR_MISSING_N="$(printf '%s' "${KR_MISSING_LIST}" | grep -c . || true)"
if [[ "${KR_TOTAL:-0}" -eq 0 ]]; then
    record "runtime-handler-annotation" SKIP "no kata-remote pods present to inspect"
elif [[ "${KR_MISSING_N:-0}" -eq 0 ]]; then
    record "runtime-handler-annotation" PASS "${KR_TOTAL} kata-remote pod(s) carry io.containerd.cri.runtime-handler=${RC_HANDLER}"
else
    record "runtime-handler-annotation" FAIL "${KR_MISSING_N}/${KR_TOTAL} kata-remote pod(s) lack io.containerd.cri.runtime-handler=${RC_HANDLER} -> image unpacks to overlayfs, 'content digest not found'. Redeploy the scheduler (./07-deploy-r1.sh) so new pods carry it."
    printf '%s\n' "${KR_MISSING_LIST}" | head -10 | sed 's/^/    /'
fi

# fleet-failures: kata-remote pods stuck in a create/pull error, grouped by
# message, so the blast radius is visible at a glance.
FLEET_FAILS="$(printf '%s' "${ALL_PODS_JSON}" | jq -r '
    [.items[] | select(.spec.runtimeClassName=="kata-remote")
      | (.status.containerStatuses // [])[]?
      | .state.waiting // empty
      | select(.reason | test("CreateContainerError|RunContainerError|CreateContainerConfigError|ImagePullBackOff|ErrImagePull"))
      | (.reason + " :: " + ((.message // "") | gsub("[0-9a-f]{64}";"<sha>") | .[0:140]))]
    | group_by(.) | map({m: .[0], n: length}) | sort_by(-.n) | .[]
    | "\(.n)x  \(.m)"' 2>/dev/null || true)"
if [[ -z "${FLEET_FAILS}" ]]; then
    record "fleet-failures" PASS "no kata-remote pods in CreateContainer/ImagePull error"
else
    FLEET_N="$(printf '%s\n' "${FLEET_FAILS}" | awk -F'x ' '{s+=$1} END{print s+0}')"
    record "fleet-failures" FAIL "${FLEET_N} kata-remote pod(s) in create/pull error (grouped below)"
    printf '%s\n' "${FLEET_FAILS}" | sed 's/^/    /'
fi

#------------------------------------------------------------------------------
# Resolve the pinned harness ref (for the host-image content probe).
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

    # runtime-config: the runtimes.kata-remote block must be complete — a
    # runtime_type AND a (non-overlayfs) snapshotter — and the RuntimeClass
    # .handler must match what pods request. A bound snapshotter plugin is not
    # enough if kata-deploy wrote a half runtime block.
    if [[ -n "${DUMP}" ]]; then
        RTYPE="$(printf '%s' "${DUMP}" | awk -F= '
            /runtimes\.kata-remote\]/{r=1;next}
            r && /^[[:space:]]*\[/{r=0}
            r && /runtime_type[[:space:]]*=/{v=$2; gsub(/[" \t]/,"",v); print v; exit}')"
        if [[ -z "${RTYPE}" ]]; then
            record "${short}/runtime-config" FAIL "runtimes.kata-remote has no runtime_type in the effective containerd config; kata-deploy wiring is incomplete"
        elif [[ -z "${SNAP}" || "${SNAP}" == "overlayfs" ]]; then
            record "${short}/runtime-config" FAIL "runtimes.kata-remote snapshotter is '${SNAP:-<unset>}' (expected a nydus guest-pull snapshotter), runtime_type=${RTYPE}"
        elif [[ "${RC_HANDLER}" != "kata-remote" ]]; then
            record "${short}/runtime-config" FAIL "RuntimeClass kata-remote .handler='${RC_HANDLER}' does not match the runtime name 'kata-remote'; pods' runtime-handler annotation will not route to this runtime"
        else
            record "${short}/runtime-config" PASS "runtime_type=${RTYPE}, snapshotter=${SNAP}, RC handler=${RC_HANDLER}"
        fi
    else
        record "${short}/runtime-config" SKIP "guest-pull pod not Ready; cannot read containerd config"
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

    # host-image-content: the harness image, IF present on the host, must have
    # COMPLETE content. An image pulled while discard_unpacked_layers was still
    # true (EKS default, before the guest-pull DaemonSet flips it to false) has
    # its compressed layers discarded after the host unpack; the nydus snapshotter
    # then cannot unpack them and every kata-remote pod fails CreateContainer with
    # "content digest ...: not found" -- while the kubelet reports the image
    # "already present on machine" and never re-pulls. Mere PRESENCE is NOT a
    # failure (the kubelet legitimately registers the image); only INCOMPLETE
    # content is the bug. Flipping the flag cannot restore already-discarded
    # layers, and containerd auto-refetch (PR #10703) is not in 1.7.x.
    if [[ "${CAN_EXEC}" != "true" ]]; then
        record "${short}/incomplete-images" SKIP "guest-pull pod not Ready; cannot inspect host content store"
    else
        CHECK="$(host_exec "${GP_POD}" ctr -n k8s.io -a /run/containerd/containerd.sock images check || true)"
        # An incomplete image only BREAKS a kata-remote pod, because only those
        # are unpacked via the nydus snapshotter (which needs the layer content).
        # System images (CNI/coredns/efs-csi/...) run on the default overlayfs
        # snapshotter and are perfectly fine incomplete — they already have their
        # snapshots and never read the discarded blob again. So FAIL only on
        # incomplete WORKLOAD images (matched by the harness repo / resource
        # prefix); report the rest as a harmless informational count.
        WL_FILTER="${HARNESS_REPO:-${RESOURCE_PREFIX:-aura-swarm}}"
        INCOMPLETE_IMGS="$(printf '%s\n' "${CHECK}" | awk 'tolower($0) ~ /incomplete/{print $1}')"
        INCN="$(printf '%s' "${INCOMPLETE_IMGS}" | grep -c . || true)"
        RELEVANT="$(printf '%s\n' "${INCOMPLETE_IMGS}" | grep -E "${WL_FILTER}" || true)"
        RELN="$(printf '%s' "${RELEVANT}" | grep -c . || true)"
        if [[ "${RELN:-0}" -gt 0 ]]; then
            record "${short}/incomplete-images" FAIL "${RELN} kata-remote workload image(s) have INCOMPLETE content (discarded layers -> nydus unpack fails 'content digest not found'). Purge so the kubelet re-pulls them complete (flag is false now): ctr -n k8s.io images ls -q | grep ${WL_FILTER} | xargs -r ctr -n k8s.io images rm"
            printf '%s\n' "${RELEVANT}" | head -10 | sed 's/^/    /'
        elif [[ "${INCN:-0}" -gt 0 ]]; then
            record "${short}/incomplete-images" PASS "no incomplete kata-remote workload images (${INCN} non-workload image(s) incomplete — harmless; they run on overlayfs, not nydus)"
        else
            record "${short}/incomplete-images" PASS "no host images with incomplete content"
        fi
    fi

    # caa-log: scan the CAA pod on this node (+ kata-deploy) for known fatal
    # error signatures, so a problem that only shows in logs is surfaced here.
    CAA_POD="$(kubectl -n "${CAA_NS}" get pods --field-selector "spec.nodeName=${node}" -o json 2>/dev/null \
        | jq -r '[.items[] | select((.metadata.ownerReferences // [])[]?.name | test("cloud-api-adaptor")) | .metadata.name][0] // ""' 2>/dev/null | tr -d '\r' || true)"
    if [[ -z "${CAA_POD}" ]]; then
        record "${short}/caa-log" SKIP "no CAA pod found on node"
    else
        # Scan BOTH the current and the previous (pre-restart) logs: a CAA pod
        # that CrashLooped or was clean-restarted moves the real fatal line (e.g.
        # an InsufficientFreeAddressesInSubnet from a failed RunInstances, or the
        # in-guest CDH ECR auth failure forwarded by the proxy) into --previous,
        # where a current-only scan would miss it and falsely report "no
        # signatures". The signature set covers pod-VM launch (RunInstances /
        # capacity / address exhaustion), IAM/auth (UnauthorizedOperation /
        # AccessDenied), the in-guest image guest-pull (Image Pull error / Not
        # authorized / image_guest_pull), attestation/CDH (Get Resource failed),
        # and host wiring (port bind / missing socket).
        CAA_LOG="$(kubectl -n "${CAA_NS}" logs "${CAA_POD}" --tail=200 2>/dev/null; \
                   kubectl -n "${CAA_NS}" logs "${CAA_POD}" --previous --tail=200 2>/dev/null || true)"
        SIGS="$(printf '%s\n' "${CAA_LOG}" | grep -iE 'RunInstances|UnauthorizedOperation|AccessDenied|InsufficientInstanceCapacity|InsufficientFreeAddressesInSubnet|Image Pull error|Not authorized|image_guest_pull|Get Resource failed|attestation .*fail|failed to create|bind: address already in use|no such file or directory' | grep -ivE 'level=(debug|info)' | sort -u | tail -8 || true)"
        if [[ -z "${SIGS}" ]]; then
            record "${short}/caa-log" PASS "no known fatal signatures in the CAA log (current + previous, last 200 lines each)"
        else
            record "${short}/caa-log" FAIL "CAA log on ${short} shows error signature(s) (current + previous; lines below)"
            printf '%s\n' "${SIGS}" | sed 's/^/    /'
        fi
    fi
done

#------------------------------------------------------------------------------
# Live end-to-end (opt-in --live): launch a throwaway kata-remote pod and assert
# it CREATES + reaches Running. This validates webhook -> capacity -> pod-VM boot
# -> guest-pull -> container create end to end (the only thing the read-only
# layers cannot prove). It boots a BILLABLE pod VM; the pod (and a best-effort
# pod-VM reap) are cleaned up on exit. Attestation/CDH is out of scope here
# (05-peer-pods-smoke-test.sh covers that); this uses a plain public image.
#------------------------------------------------------------------------------
if [[ "${LIVE}" == "true" ]]; then
    log_section "Live end-to-end (--live)"
    LIVE_IMAGE="${DOCTOR_LIVE_IMAGE:-busybox:1.36}"
    LIVE_POD="peerpods-doctor-live-$(date +%s)"
    LIVE_NS="${K8S_NAMESPACE_SYSTEM}"
    LIVE_TIMEOUT="${DOCTOR_LIVE_TIMEOUT_SECS:-600}"
    PODVM_FILTER="podvm-${LIVE_POD}*"

    live_cleanup() {
        kubectl delete pod "${LIVE_POD}" -n "${LIVE_NS}" --ignore-not-found --wait=false >/dev/null 2>&1 || true
        # Best-effort: terminate any pod VM this run created (no leaked instance).
        local ids
        ids="$(aws ec2 describe-instances --region "${AWS_REGION}" \
            --filters "Name=tag:Name,Values=${PODVM_FILTER}" "Name=instance-state-name,Values=pending,running" \
            --query 'Reservations[].Instances[].InstanceId' --output text 2>/dev/null | tr '[:space:]' ' ' || true)"
        if [[ -n "${ids// /}" ]]; then
            log_detail "terminating throwaway pod VM(s): ${ids}"
            aws ec2 terminate-instances --region "${AWS_REGION}" --instance-ids ${ids} >/dev/null 2>&1 || true
        fi
    }
    trap live_cleanup EXIT

    LIVE_NODE_SELECTOR=""
    [[ -n "${ONLY_NODE}" ]] && LIVE_NODE_SELECTOR=$'\n  nodeSelector:\n    kubernetes.io/hostname: '"${ONLY_NODE}"

    log_info "Launching throwaway kata-remote pod ${LIVE_POD} (image ${LIVE_IMAGE}; boots a pod VM)..."
    kubectl apply -f - >/dev/null <<EOF || true
apiVersion: v1
kind: Pod
metadata:
  name: ${LIVE_POD}
  namespace: ${LIVE_NS}
  annotations:
    io.containerd.cri.runtime-handler: ${RC_HANDLER}
spec:
  restartPolicy: Never
  runtimeClassName: kata-remote${LIVE_NODE_SELECTOR}
  containers:
    - name: probe
      image: ${LIVE_IMAGE}
      command: ["sh", "-c", "echo doctor-live-ok; sleep 3600"]
      resources:
        requests: { cpu: 50m, memory: 64Mi }
        limits: { cpu: 250m, memory: 256Mi }
EOF

    LIVE_ELAPSED=0; LIVE_POLL=10; LIVE_RESULT=""; LIVE_NOTE=""
    while [[ ${LIVE_ELAPSED} -le ${LIVE_TIMEOUT} ]]; do
        LJSON="$(kubectl get pod "${LIVE_POD}" -n "${LIVE_NS}" -o json 2>/dev/null || echo '{}')"
        LPHASE="$(printf '%s' "${LJSON}" | jq -r '.status.phase // "Unknown"')"
        LWAIT="$(printf '%s' "${LJSON}" | jq -r '.status.containerStatuses[0].state.waiting.reason // ""')"
        LWMSG="$(printf '%s' "${LJSON}" | jq -r '.status.containerStatuses[0].state.waiting.message // ""')"
        LSCHED="$(printf '%s' "${LJSON}" | jq -r '([.status.conditions[]? | select(.type=="PodScheduled")][0]) as $c | (($c.reason // "")+": "+($c.message // ""))')"
        if [[ "${LPHASE}" == "Running" || "${LPHASE}" == "Succeeded" ]]; then
            LIVE_RESULT=PASS; LIVE_NOTE="pod reached ${LPHASE} (create -> guest-pull -> pod-VM boot all work)"; break
        fi
        case "${LWAIT}" in
            CreateContainerError|RunContainerError|CreateContainerConfigError|ImagePullBackOff|ErrImagePull)
                LIVE_RESULT=FAIL; LIVE_NOTE="${LWAIT}: ${LWMSG:0:160}"; break ;;
        esac
        [[ "${LSCHED}" == *Unschedulable* ]] && { LIVE_RESULT=FAIL; LIVE_NOTE="Unschedulable: ${LSCHED:0:140}"; break; }
        log_progress "[${LIVE_ELAPSED}s] live pod phase=${LPHASE}${LWAIT:+ waiting=${LWAIT}}"
        sleep "${LIVE_POLL}"; LIVE_ELAPSED=$((LIVE_ELAPSED + LIVE_POLL))
    done
    [[ -z "${LIVE_RESULT}" ]] && { LIVE_RESULT=FAIL; LIVE_NOTE="did not reach Running within ${LIVE_TIMEOUT}s (still ${LPHASE:-?})"; }
    record "live-create" "${LIVE_RESULT}" "${LIVE_NOTE}"
    if [[ "${LIVE_RESULT}" != "PASS" ]]; then
        log_detail "live pod events:"
        kubectl describe pod "${LIVE_POD}" -n "${LIVE_NS}" 2>/dev/null | sed -n '/Events:/,$p' | indent || true
    fi
    live_cleanup
    trap - EXIT
fi

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
