#!/bin/bash
# 05-peer-pods-smoke-test.sh - End-to-end attestation proof for the Peer Pods
# (Cloud API Adaptor / kata-remote) runtime BEFORE any product traffic.
#
# The confidential VM here is an off-cluster AWS-managed SEV-SNP "pod VM" that
# CAA launches per pod. The pod's
# kata shim + agent-protocol-forwarder run on an ordinary worker; the workload,
# guest kernel and CDH run inside the pod VM.
#
# Flow:
#   1. provision a scratch KBS resource (random DEK) by writing it into the
#      kbs-repository PVC (same approach as the SNP test)
#   2. launch a throwaway pod with runtimeClassName=kata-remote (NO SNP
#      nodeSelector/toleration); CAA boots a pod VM that attests + fetches the
#      resource through the in-guest CDH (release is attestation-gated)
#   3. NEW assertions vs on-node:
#        a. a real EC2 pod VM is CREATED during the run
#        b. the in-guest CDH fetches the scratch DEK and the value matches
#        c. after the pod is deleted the pod VM is TERMINATED (no leaked
#           instances → no silent cost/quota leak) — the headline new check
#   4. self-cleaning: pod, scratch resource, and (as a last resort) the pod VM
#
# Usage: ./05-peer-pods-smoke-test.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "05" "Peer Pods (kata-remote) attestation smoke test" "ops-admin"

# When the run FAILS, the EXIT trap normally deletes the smoke pod + scratch
# resource, which destroys the pod's events/logs before you can inspect them.
# Pass --no-cleanup (or KEEP_POD_ON_FAILURE=true) to keep them in place on
# failure and print ready-to-paste post-mortem commands instead.
KEEP_POD_ON_FAILURE="${KEEP_POD_ON_FAILURE:-false}"
for arg in "$@"; do
    case "${arg}" in
        --no-cleanup|--keep|--keep-on-failure) KEEP_POD_ON_FAILURE=true ;;
        -h|--help)
            echo "Usage: $0 [--no-cleanup]"
            echo "  --no-cleanup   on failure, KEEP the smoke pod + scratch KBS resource"
            echo "                 (and any pod VM) for post-mortem and print the"
            echo "                 describe/logs/EC2 commands to inspect them."
            echo "                 Equivalent: KEEP_POD_ON_FAILURE=true ./$(basename "$0")"
            exit 0
            ;;
        *) step_fail "unknown argument: ${arg} (try --help)" ;;
    esac
done

require_cmds aws kubectl jq openssl
require_aws_auth
ensure_kubectl_context

[[ -f "${SECRETS_DIR}/kbs-admin.key" ]] \
    || step_fail "missing .secrets/kbs-admin.key — run ./04-trustee-kbs.sh first"

# Provision the scratch resource by writing directly into the KBS LocalFs
# repository (same rationale as the SNP test: kbs-client is an ORAS artifact and
# the KBS image is distroless). The writer/rm pods are plain busybox on ordinary
# workers — they do NOT use kata-remote.
SMOKE_WRITER_IMAGE="${SMOKE_WRITER_IMAGE:-busybox:1.36}"
KBS_REPO_PVC="kbs-repository"
KBS_REPO_DIR="/opt/confidential-containers/kbs"
# CoCo guest CDH RESTful API (resource release is attestation-gated)
CDH_RESOURCE_URL="${CDH_RESOURCE_URL:-http://127.0.0.1:8006/cdh/resource}"
SMOKE_RESOURCE_PATH="default/peer-pods-smoke-test/dek"
SMOKE_RESOURCE_DIR="$(dirname "${SMOKE_RESOURCE_PATH}")"
# KBS (Trustee >= v0.20) stores resources via the kvstorage LocalFs backend: the
# resource plugin uses the unified storage backend under the "repository"
# namespace, and EACH resource is a SINGLE flat file whose name is the resource
# path with every "/" replaced by the literal 4-char string "\x2F" (see
# deps/key-value-storage/src/local_fs/mod.rs: dir_path.join(key.replace('/',
# "\\x2F"))). So the on-disk file for default/peer-pods-smoke-test/dek is
#   <dir_path>/repository/default\x2Fpeer-pods-smoke-test\x2Fdek
# NOT a nested default/peer-pods-smoke-test/dek tree. Writing the nested tree (or
# omitting the repository namespace dir) makes KBS return "resource not found"
# even though attestation succeeds.
KBS_REPO_NS_DIR="${KBS_REPO_DIR}/repository"
SMOKE_RESOURCE_KEY_ESC="${SMOKE_RESOURCE_PATH//\//\\x2F}"
SMOKE_RESOURCE_FILE="${KBS_REPO_NS_DIR}/${SMOKE_RESOURCE_KEY_ESC}"
# The flat filename contains literal backslashes ("\x2F"). Embedding it directly
# in a YAML double-quoted scalar or a JSON string is a trap: YAML/JSON treat
# "\x2F" as an escape and turn it back into "/", recreating the WRONG nested path.
# So hand the writer/rm pods the base64 of the exact filename and decode it inside
# the container, which is immune to the heredoc + YAML + shell escaping layers.
SMOKE_RESOURCE_NAME_B64="$(printf '%s' "${SMOKE_RESOURCE_KEY_ESC}" | base64 | tr -d '\n')"

SMOKE_SETTER_POD="peer-pods-smoke-kbs-setter"
SMOKE_RM_POD="peer-pods-smoke-kbs-rm"
SMOKE_POD="peer-pods-smoke-test"

# CAA names each pod VM after its pod (provider default: podvm-<pod-name>-<id>).
# TODO(phase4): confirm the exact instance naming/tagging against the pinned CAA
# chart (some versions also set a peerpod/cloud-api-adaptor tag); adjust if needed.
PODVM_NAME_FILTER="${PODVM_NAME_FILTER:-podvm-${SMOKE_POD}*}"

# Set before the trap so cleanup can reference them under `set -u`.
BEFORE_IDS=""
NEW_PODVM_ID=""

# Pod-VM instance IDs matching this test's tag filter in the given comma-separated
# instance states (e.g. "pending,running").
podvm_instance_ids() {
    aws ec2 describe-instances --region "${AWS_REGION}" \
        --filters "Name=tag:Name,Values=${PODVM_NAME_FILTER}" \
                  "Name=instance-state-name,Values=$1" \
        --query 'Reservations[].Instances[].InstanceId' \
        --output text 2>/dev/null | tr '[:space:]' '\n' | sed '/^$/d' || true
}

# Current state of a single instance id (echoes "unknown" if it cannot be read).
instance_state() {
    local s
    s=$(aws ec2 describe-instances --region "${AWS_REGION}" --instance-ids "$1" \
        --query 'Reservations[].Instances[].State.Name' --output text 2>/dev/null || echo "")
    echo "${s:-unknown}"
}

# First pending/running pod VM matching the filter that was not present before
# launch (i.e. the one CAA created for this run). Echoes empty if none yet.
new_podvm_id() {
    local id
    while IFS= read -r id; do
        [[ -z "${id}" ]] && continue
        if ! printf '%s\n' "${BEFORE_IDS}" | grep -qxF "${id}"; then
            echo "${id}"
            return 0
        fi
    done < <(podvm_instance_ids "pending,running")
    return 0
}

# Live diagnostics for a kata-remote pod taking too long in ContainerCreating
# (CAA pod-VM boot / sandbox creation). This is the silent state the bare phase
# poll hides: a scheduled pod whose sandbox never finishes sets NO error
# waiting.reason, so without this the loop just reprints "ContainerCreating"
# until the launch timeout. Surfaces the three signals that actually explain it
# — the pod's sandbox-creation Events, the pod VM's EC2 state, and the CAA
# daemonset log tail. All read-only.
dump_containercreating_diagnostics() {
    log_info "--- kata-remote ContainerCreating diagnostics ---"
    log_detail "pod sandbox events (a repeated FailedCreatePodSandBox shows the real cause):"
    kubectl describe pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null \
        | sed -n '/Events:/,$p' | indent || true
    if [[ -n "${NEW_PODVM_ID}" ]]; then
        log_detail "pod VM ${NEW_PODVM_ID} (CAA launched it for this run):"
        aws ec2 describe-instances --region "${AWS_REGION}" --instance-ids "${NEW_PODVM_ID}" \
            --query 'Reservations[].Instances[].{id:InstanceId,state:State.Name,type:InstanceType,ami:ImageId,launched:LaunchTime,reason:StateReason.Message}' \
            --output table 2>&1 | indent || true
    else
        log_detail "pod VM: none matching '${PODVM_NAME_FILTER}' detected yet"
        log_detail "(CAA may not have called RunInstances — check the CAA log below for an AWS error)"
    fi
    log_detail "CAA daemonset log (tail):"
    kubectl logs -n "${CAA_NAMESPACE:-confidential-containers-system}" \
        ds/"${CAA_DAEMONSET:-cloud-api-adaptor-daemonset}" --tail=80 2>/dev/null \
        | indent || true
    log_info "--- end diagnostics ---"
}

# Most recent Event for the smoke pod as "reason: message" (empty if none). The
# events stream is where kata-remote sandbox-creation failures surface
# (FailedCreatePodSandBox ...), which the bare phase / containerStatuses hide.
pod_latest_event() {
    kubectl get events -n "${K8S_NAMESPACE_SYSTEM}" \
        --field-selector "involvedObject.name=${SMOKE_POD},involvedObject.kind=Pod" \
        --sort-by=.lastTimestamp \
        -o jsonpath='{range .items[-1:]}{.reason}: {.message}{end}' 2>/dev/null || true
}

# One-line EC2 summary for a pod VM id: "state type ami privIP launched".
podvm_describe_line() {
    aws ec2 describe-instances --region "${AWS_REGION}" --instance-ids "$1" \
        --query 'Reservations[].Instances[].[State.Name,InstanceType,ImageId,(PrivateIpAddress||`-`),LaunchTime]' \
        --output text 2>/dev/null | tr '\t' ' ' || true
}

# Print a post-mortem with the exact commands to inspect the (preserved) smoke
# pod, the CAA pod co-located on its node, and any pod VM it launched. Called
# from the EXIT trap when the run failed and KEEP_POD_ON_FAILURE=true.
print_failure_postmortem() {
    local ns="${K8S_NAMESPACE_SYSTEM}" pod="${SMOKE_POD}"
    local caa_ns="${CAA_NAMESPACE:-confidential-containers-system}"
    local node caa_pod
    node=$(kubectl get pod "${pod}" -n "${ns}" -o jsonpath='{.spec.nodeName}' 2>/dev/null || true)
    if [[ -n "${node}" ]]; then
        caa_pod=$(kubectl get pods -n "${caa_ns}" --field-selector "spec.nodeName=${node}" -o json 2>/dev/null \
            | jq -r '[.items[] | select((.metadata.ownerReferences // [])[]?.name | test("cloud-api-adaptor")) | .metadata.name][0] // ""' 2>/dev/null || true)
    fi
    {
        echo ""
        echo -e "${YELLOW}⚠ KEEP_POD_ON_FAILURE: leaving '${pod}' (ns ${ns}) and the scratch KBS resource in place for post-mortem.${NC}"
        echo "  pod node: ${node:-<unscheduled>}"
        [[ -n "${NEW_PODVM_ID}" ]] && echo "  detected pod VM: ${NEW_PODVM_ID}"
        echo ""
        echo "  Inspect (the pod and its events/logs are still there):"
        echo "    kubectl describe pod ${pod} -n ${ns} | sed -n '/Events:/,\$p'"
        echo "    kubectl get pod ${pod} -n ${ns} -o wide"
        echo "    kubectl logs ${pod} -n ${ns}"
        echo "    kubectl logs -n ${caa_ns} ${caa_pod:-<caa-pod-on-${node:-NODE}>} --tail=200"
        echo "    aws ec2 describe-instances --region ${AWS_REGION} \\"
        echo "      --filters \"Name=tag:Name,Values=${PODVM_NAME_FILTER}\" \\"
        echo "      --query 'Reservations[].Instances[].{id:InstanceId,state:State.Name,launched:LaunchTime}' --output table"
        echo ""
        echo "  When done, clean up manually:"
        echo "    kubectl delete pod ${pod} ${SMOKE_SETTER_POD} ${SMOKE_RM_POD} -n ${ns} --ignore-not-found"
        [[ -n "${NEW_PODVM_ID}" ]] && \
            echo "    aws ec2 terminate-instances --region ${AWS_REGION} --instance-ids ${NEW_PODVM_ID}"
    } >&2
}

cleanup() {
    local rc=$?
    # Preserve evidence for a post-mortem when the run FAILED and the operator
    # asked to keep it: skip the whole teardown (smoke pod, scratch resource,
    # and the pod VM safety net) so events/logs/EC2 survive for inspection.
    if [[ "${KEEP_POD_ON_FAILURE}" == "true" && ${rc} -ne 0 && "${CLEANUP_PRESERVED:-0}" == "0" ]]; then
        CLEANUP_PRESERVED=1
        print_failure_postmortem
        return
    fi
    kubectl delete pod "${SMOKE_POD}" "${SMOKE_SETTER_POD}" "${SMOKE_RM_POD}" \
        -n "${K8S_NAMESPACE_SYSTEM}" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    # Remove the scratch resource file from the KBS LocalFs repository so no
    # throwaway key material outlives the test (best-effort; mounts the RWX PVC).
    kubectl run "${SMOKE_RM_POD}" -n "${K8S_NAMESPACE_SYSTEM}" --restart=Never --quiet \
        --image="${SMOKE_WRITER_IMAGE}" \
        --overrides="{\"spec\":{\"volumes\":[{\"name\":\"repo\",\"persistentVolumeClaim\":{\"claimName\":\"${KBS_REPO_PVC}\"}}],\"containers\":[{\"name\":\"rm\",\"image\":\"${SMOKE_WRITER_IMAGE}\",\"command\":[\"sh\",\"-c\",\"f=\$(printf %s '${SMOKE_RESOURCE_NAME_B64}' | base64 -d); rm -f ${KBS_REPO_NS_DIR}/\$f\"],\"volumeMounts\":[{\"name\":\"repo\",\"mountPath\":\"${KBS_REPO_DIR}\"}]}]}}" \
        >/dev/null 2>&1 || true
    kubectl wait --for=jsonpath='{.status.phase}'=Succeeded "pod/${SMOKE_RM_POD}" \
        -n "${K8S_NAMESPACE_SYSTEM}" --timeout=60s >/dev/null 2>&1 || true
    kubectl delete pod "${SMOKE_RM_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        --ignore-not-found --wait=false >/dev/null 2>&1 || true
    # Last-resort: never leave a running pod VM behind (cost/quota leak). Only
    # touches the instance THIS test created (captured in NEW_PODVM_ID).
    if [[ -n "${NEW_PODVM_ID}" ]]; then
        case "$(instance_state "${NEW_PODVM_ID}")" in
            pending|running|stopping|stopped)
                log_warn "Terminating leftover pod VM ${NEW_PODVM_ID} (cleanup safety net)..." >&2
                aws ec2 terminate-instances --region "${AWS_REGION}" \
                    --instance-ids "${NEW_PODVM_ID}" >/dev/null 2>&1 || true
                ;;
        esac
    fi
}
trap cleanup EXIT

# Idempotent re-run: clear leftovers from a previous partial failure first.
cleanup
NEW_PODVM_ID=""

#------------------------------------------------------------------------------
# 0. Capture pod VMs present BEFORE launch (so we can prove one is CREATED).
#------------------------------------------------------------------------------

BEFORE_IDS="$(podvm_instance_ids "pending,running,shutting-down,stopping,stopped")"
BEFORE_COUNT=$(printf '%s\n' "${BEFORE_IDS}" | sed '/^$/d' | wc -l | tr -d ' ')
log_info "Pod VMs matching '${PODVM_NAME_FILTER}' before launch: ${BEFORE_COUNT}"

#------------------------------------------------------------------------------
# 1. Provision the scratch KBS resource
#------------------------------------------------------------------------------

SMOKE_VALUE=$(openssl rand -hex 32)

log_info "Provisioning scratch KBS resource ${SMOKE_RESOURCE_PATH} (writing into the ${KBS_REPO_PVC} PVC)..."
kubectl apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: ${SMOKE_SETTER_POD}
  namespace: ${K8S_NAMESPACE_SYSTEM}
spec:
  restartPolicy: Never
  containers:
    - name: writer
      image: ${SMOKE_WRITER_IMAGE}
      command:
        - sh
        - -c
        - |
          set -e
          mkdir -p '${KBS_REPO_NS_DIR}'
          f=\$(printf '%s' '${SMOKE_RESOURCE_NAME_B64}' | base64 -d)
          printf '%s' '${SMOKE_VALUE}' > "${KBS_REPO_NS_DIR}/\$f"
          echo "wrote ${KBS_REPO_NS_DIR}/\$f"
          ls -l "${KBS_REPO_NS_DIR}/\$f"
      volumeMounts:
        - { name: repo, mountPath: ${KBS_REPO_DIR} }
  volumes:
    - name: repo
      persistentVolumeClaim:
        claimName: ${KBS_REPO_PVC}
EOF

SETTER_TIMEOUT="${SETTER_TIMEOUT_SECS:-180}"
SETTER_POLL=10
SETTER_ELAPSED=0
SETTER_PHASE=""
log_info "Waiting for the KBS resource writer pod (timeout ${SETTER_TIMEOUT}s)..."
while [[ ${SETTER_ELAPSED} -le ${SETTER_TIMEOUT} ]]; do
    SETTER_PHASE=$(kubectl get pod "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.phase}' 2>/dev/null || echo "Unknown")
    [[ "${SETTER_PHASE}" == "Succeeded" || "${SETTER_PHASE}" == "Failed" ]] && break
    SETTER_WAIT=$(kubectl get pod "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}' 2>/dev/null || true)
    log_progress "[${SETTER_ELAPSED}s] setter pod: ${SETTER_PHASE:-Pending} ${SETTER_WAIT:+(${SETTER_WAIT})}"
    sleep "${SETTER_POLL}"
    SETTER_ELAPSED=$((SETTER_ELAPSED + SETTER_POLL))
done

if [[ "${SETTER_PHASE}" != "Succeeded" ]]; then
    log_info "--- KBS resource writer diagnostics ---"
    log_detail "image: ${SMOKE_WRITER_IMAGE}"
    log_detail "phase: $(kubectl get pod "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" -o jsonpath='{.status.phase}' 2>/dev/null)"
    log_detail "container state:"
    kubectl get pod "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{range .status.containerStatuses[*]}    {.name}: ready={.ready} waiting={.state.waiting.reason} {.state.waiting.message} terminated={.state.terminated.reason}(exit {.state.terminated.exitCode}){"\n"}{end}' 2>/dev/null || true
    log_detail "events:"
    kubectl describe pod "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null \
        | sed -n '/Events:/,$p' | indent || true
    log_detail "logs (current):"
    kubectl logs "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>&1 | indent || true
    log_info "--- end diagnostics ---"
    step_fail "could not provision the scratch KBS resource (writer pod failed)"
fi
log_ok "Scratch resource provisioned in KBS"

#------------------------------------------------------------------------------
# 2. Throwaway kata-remote pod: CAA boots a pod VM that attests + fetches via CDH
#------------------------------------------------------------------------------

# Pre-flight: the peer-pods mutating webhook MUST be effective (registered AND
# callable) or this pod will never schedule — it keeps its cpu/memory and goes
# Unschedulable "Insufficient cpu". Fail now with the precise cause instead of
# burning the launch timeout. Set PEER_PODS_REQUIRE_WEBHOOK=false to skip (e.g.
# on a cluster where you deliberately run kata-remote without the webhook).
if [[ "${PEER_PODS_REQUIRE_WEBHOOK:-true}" == "true" ]]; then
    WH_DIAG=""
    WH_RC=0
    WH_DIAG=$(peerpods_webhook_effective) || WH_RC=$?
    if [[ ${WH_RC} -ne 0 ]]; then
        log_info "--- peer-pods webhook diagnostics ---"
        printf '%s\n' "${WH_DIAG}" | indent
        log_info "--- end diagnostics ---"
        if [[ ${WH_RC} -eq 2 ]]; then
            step_fail "no peer-pods MutatingWebhookConfiguration is registered — kata-remote pods keep cpu/memory and never schedule. Run ./03-coco-operator.sh (CAA_ENABLE_WEBHOOK=true) first, or set PEER_PODS_REQUIRE_WEBHOOK=false to skip this guard."
        else
            step_fail "the peer-pods mutating webhook is registered but NOT effective (see diagnostics above): empty caBundle or no ready endpoints, so the API server cannot call it and (with failurePolicy=Ignore) would SILENTLY admit this pod unmutated. Fix cert-manager CA injection then re-run, or set PEER_PODS_REQUIRE_WEBHOOK=false to bypass."
        fi
    fi
    log_ok "Peer-pods mutating webhook is effective (kata-remote pods get rewritten to kata.peerpods.io/vm)"
fi

# Pre-flight: the kata-remote shim dials /run/peerpod/hypervisor.sock per node;
# a node whose CAA adaptor never bound it fails sandbox creation instantly
# ('dial unix /run/peerpod/hypervisor.sock: connect: no such file or directory')
# and the pod sits in ContainerCreating until the timeout. A Ready CAA daemonset
# does NOT prove this. We only need ONE healthy node: find one that serves the
# socket and PIN the smoke pod there (the pod otherwise has no node affinity and
# could land on a bare node). Set PEER_PODS_REQUIRE_HYPERVISOR_SOCKET=false to
# skip (healing CAA pods is ./03-coco-operator.sh's job, not this read-only test).
SMOKE_NODE=""
SMOKE_NODE_SELECTOR=""
if [[ "${PEER_PODS_REQUIRE_HYPERVISOR_SOCKET:-true}" == "true" ]]; then
    SMOKE_NODE="$(caa_node_with_hypervisor_socket \
        "${CAA_NAMESPACE:-confidential-containers-system}" \
        "${CAA_DAEMONSET:-cloud-api-adaptor-daemonset}")"
    if [[ -z "${SMOKE_NODE}" ]]; then
        log_info "--- CAA hypervisor socket diagnostics ---"
        log_detail "CAA pods NOT serving /run/peerpod/hypervisor.sock:"
        caa_pods_missing_hypervisor_socket \
            "${CAA_NAMESPACE:-confidential-containers-system}" \
            "${CAA_DAEMONSET:-cloud-api-adaptor-daemonset}" | indent
        log_info "--- end diagnostics ---"
        step_fail "no CAA node currently serves /run/peerpod/hypervisor.sock, so the kata-remote shim
  cannot reach the hypervisor and the smoke pod would fail sandbox creation (stuck
  ContainerCreating). Re-run ./03-coco-operator.sh (it verifies and clean-restarts a CAA pod until
  one serves the socket), or set PEER_PODS_REQUIRE_HYPERVISOR_SOCKET=false to skip this guard."
    fi
    log_ok "CAA node ${SMOKE_NODE} serves /run/peerpod/hypervisor.sock — pinning the smoke pod there"
    SMOKE_NODE_SELECTOR="  nodeSelector:
    kubernetes.io/hostname: ${SMOKE_NODE}"
fi

# Pre-flight: the kata-remote RuntimeClass must carry ZERO pod overhead. The
# scheduler ADDS .overhead.podFixed to every kata-remote pod's effective
# requests, so even a webhook-mutated pod (cpu=<none>, kata.peerpods.io/vm=1)
# gets that cpu added and goes Unschedulable "Insufficient cpu" on a busy node —
# which is exactly what an "Insufficient cpu" failure here means, NOT missing
# kata.peerpods.io/vm capacity. kata-deploy re-stamps a non-zero default on every
# CAA install (03-coco-operator.sh zeros it), so a stale overhead can creep back.
# Zero it in place (idempotent, conflict-free merge patch — the same one 03 uses)
# so the test is not blocked by a transient kata-deploy re-stamp. Set
# PEER_PODS_ZERO_RUNTIMECLASS_OVERHEAD=false to instead fail fast (pure-diagnosis
# mode; healing the RuntimeClass is then ./03-coco-operator.sh's job).
if ! kata_remote_overhead_is_zero; then
    KR_OVERHEAD="$(kata_remote_overhead)"
    if [[ "${PEER_PODS_ZERO_RUNTIMECLASS_OVERHEAD:-true}" == "true" ]]; then
        if kubectl patch runtimeclass kata-remote --type=merge \
            -p '{"overhead":{"podFixed":{"cpu":"0","memory":"0"}}}' >/dev/null 2>&1; then
            log_ok "Zeroed kata-remote RuntimeClass overhead (was ${KR_OVERHEAD}; the scheduler would otherwise add it and the pod would go Unschedulable 'Insufficient cpu')"
        else
            step_fail "kata-remote RuntimeClass has non-zero overhead (${KR_OVERHEAD}) and it could not be patched to zero.
  The scheduler ADDS this overhead to the (otherwise 0-cpu) mutated pod, so it goes Unschedulable 'Insufficient cpu'.
  Zero it manually, then re-run:
    kubectl patch runtimeclass kata-remote --type=merge -p '{\"overhead\":{\"podFixed\":{\"cpu\":\"0\",\"memory\":\"0\"}}}'"
        fi
    else
        step_fail "kata-remote RuntimeClass has non-zero overhead (${KR_OVERHEAD}) and PEER_PODS_ZERO_RUNTIMECLASS_OVERHEAD=false.
  The scheduler ADDS this overhead to the (otherwise 0-cpu) webhook-mutated pod, so it goes Unschedulable
  'Insufficient cpu' even though its container requests no cpu — this is NOT a kata.peerpods.io/vm capacity
  problem. Re-run ./03-coco-operator.sh (it zeros the overhead), or patch it directly, then re-run:
    kubectl patch runtimeclass kata-remote --type=merge -p '{\"overhead\":{\"podFixed\":{\"cpu\":\"0\",\"memory\":\"0\"}}}'"
    fi
else
    log_ok "kata-remote RuntimeClass overhead is zero (peer-pods schedules via kata.peerpods.io/vm)"
fi

#------------------------------------------------------------------------------
# CoCo initdata: tell the in-guest attestation-agent + CDH where the KBS is.
#
# The guest components (AA/CDH) run on the pod VM and egress over the pod VM's
# OWN VPC interface, NOT the cluster pod-network tunnel — so they CANNOT reach
# the KBS ClusterIP (kube-proxy only exists on cluster nodes). They must be given
# a VPC-routable KBS address. The KBS pod IP is a real VPC ENI address (AWS VPC
# CNI), reachable from the pod VM's agent subnet (the node SG admits TCP 8080 from
# the pod-VM SG), so discover it at run time and pass aa.toml/cdh.toml via the
# kata initdata annotation (gzip|base64 of the initdata TOML). Without this the
# CDH fetch fails fast with "[CDH] [ERROR]: Get Resource failed" and KBS never
# logs an attestation request.
KBS_POD_IP=$(kubectl get pod -n "${K8S_NAMESPACE_SYSTEM}" -l app=kbs \
    -o jsonpath='{.items[0].status.podIP}' 2>/dev/null || true)
[[ -n "${KBS_POD_IP}" ]] \
    || step_fail "could not determine the KBS pod IP (kubectl get pod -n ${K8S_NAMESPACE_SYSTEM} -l app=kbs); is the KBS running?"
KBS_GUEST_URL="http://${KBS_POD_IP}:8080"
log_info "In-guest attestation will target KBS at ${KBS_GUEST_URL} (pod VM reaches the KBS pod IP over the VPC)."

INITDATA_TOML=$(mktemp)
cat > "${INITDATA_TOML}" <<TOML
algorithm = "sha384"
version = "0.1.0"

[data]
"aa.toml" = '''
[token_configs]
[token_configs.coco_as]
url = '${KBS_GUEST_URL}'

[token_configs.kbs]
url = '${KBS_GUEST_URL}'
'''

"cdh.toml" = '''
socket = 'unix:///run/confidential-containers/cdh.sock'
credentials = []

[kbc]
name = 'cc_kbc'
url = '${KBS_GUEST_URL}'
'''
TOML
SMOKE_INITDATA_B64=$(gzip -c "${INITDATA_TOML}" | base64 -w0)
rm -f "${INITDATA_TOML}"

echo ""
log_info "Launching throwaway pod on kata-remote (CAA pod-VM boot + attestation can take a few minutes)..."
kubectl apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: ${SMOKE_POD}
  namespace: ${K8S_NAMESPACE_SYSTEM}
  labels:
    app: peer-pods-smoke-test
  annotations:
    io.katacontainers.config.hypervisor.cc_init_data: "${SMOKE_INITDATA_B64}"
    # containerd 1.7 only routes the image pull to the kata-remote runtime's
    # nydus (guest-pull) snapshotter when this annotation is present; without it
    # the image unpacks to the default overlayfs snapshotter and create fails
    # with "content digest ...: not found" (containerd #8674 / kata #8407).
    io.containerd.cri.runtime-handler: kata-remote
spec:
  restartPolicy: Never
  runtimeClassName: kata-remote
${SMOKE_NODE_SELECTOR}
  containers:
    - name: smoke
      image: curlimages/curl:8.10.1
      command:
        - sh
        - -c
        - |
          i=0
          while [ \$i -lt 60 ]; do
            v=\$(curl -fsS --max-time 10 "${CDH_RESOURCE_URL}/${SMOKE_RESOURCE_PATH}" 2>/tmp/err) || true
            if [ -n "\$v" ]; then
              echo "CDH_VALUE:\$v"
              exit 0
            fi
            echo "CDH_ATTEMPT \$i failed: \$(cat /tmp/err 2>/dev/null)"
            i=\$((i + 1))
            sleep 5
          done
          echo "CDH_FETCH_FAILED"
          exit 1
      resources:
        requests: { cpu: 250m, memory: 256Mi }
        limits: { cpu: 500m, memory: 512Mi }
EOF

TIMEOUT="${PEER_PODS_SMOKE_TIMEOUT_SECS:-900}"
POLL=10
ELAPSED=0
PHASE=""
# Fail fast if the pod stays Unschedulable. A kata-remote pod can fail here for
# two distinct reasons: the webhook did not mutate it (still requests cpu), or it
# was mutated but workers do not advertise/free kata.peerpods.io/vm capacity.
# Neither condition self-resolves during this smoke test.
UNSCHED_GRACE="${PEER_PODS_UNSCHED_GRACE_SECS:-60}"
UNSCHED_ELAPSED=0
# Also fail fast on a stuck container-create error (e.g. the workload image being
# unpacked on the host instead of guest-pulled -> "content digest not found").
# Allow a grace for the pod-VM boot + first CreateContainer attempt.
CWAIT_GRACE="${PEER_PODS_CONTAINER_ERROR_GRACE_SECS:-90}"
CWAIT_ELAPSED=0
# A scheduled kata-remote pod whose sandbox is still being created sits in a
# benign "ContainerCreating" with NO error waiting.reason while CAA boots the
# pod VM and the shim dials the in-guest agent. That can legitimately take a few
# minutes, but it must not be a black box: surface live diagnostics every
# CC_DIAG_INTERVAL, and fast-fail after CC_GRACE so a pod VM that boots but never
# becomes usable (unreachable agent, failed attestation, no RunInstances) ends
# with the cause printed instead of silently burning the full launch timeout.
CC_DIAG_INTERVAL="${PEER_PODS_CC_DIAG_INTERVAL_SECS:-120}"
CC_GRACE="${PEER_PODS_CONTAINERCREATING_GRACE_SECS:-420}"
CC_ELAPSED=0
CC_LAST_DIAG=0
# A pod that reaches Running has booted, attested, and started the container, so
# the in-guest CDH fetch should release the DEK within seconds and the pod flips
# to Succeeded. If it stays Running this long the fetch is failing every retry
# (attestation/DEK release broken) and the container will only confirm it after
# burning its full in-guest retry budget (~300s). Fast-fail with the container's
# own CDH_ATTEMPT log instead of waiting it out.
RUNNING_GRACE="${PEER_PODS_RUNNING_GRACE_SECS:-90}"
RUNNING_ELAPSED=0
# Previous values so we can log STATE TRANSITIONS (phase, pod-VM EC2 state, and
# the latest pod Event) the moment they change, instead of silently reprinting.
PREV_PHASE=""
PREV_PODVM_STATE=""
PREV_EVENT=""

# Echo the effective launch parameters so the run is self-describing: every knob
# that decides when/why it fails, plus the worker<->pod-VM contract to check.
log_kv "pod" "${SMOKE_POD} (ns ${K8S_NAMESPACE_SYSTEM}, runtimeClass kata-remote)"
log_kv "pod VM" "AMI ${PODVM_AMI_ID:-<unset>}, type ${PODVM_INSTANCE_TYPE:-<unset>}, name filter '${PODVM_NAME_FILTER}'"
log_kv "CDH resource" "${CDH_RESOURCE_URL}/${SMOKE_RESOURCE_PATH}"
log_kv "worker<->podVM" "forwarder :${CAA_FORWARDER_PORT:-15150}, vxlan :${CAA_VXLAN_PORT:-9000}"
log_kv "CAA" "ds ${CAA_DAEMONSET:-cloud-api-adaptor-daemonset} (ns ${CAA_NAMESPACE:-confidential-containers-system})"
log_kv "timeouts(s)" "launch=${TIMEOUT} unsched-grace=${UNSCHED_GRACE} container-err-grace=${CWAIT_GRACE} containercreating-grace=${CC_GRACE} running-grace=${RUNNING_GRACE} diag-interval=${CC_DIAG_INTERVAL} poll=${POLL}"
echo ""

while [[ ${ELAPSED} -le ${TIMEOUT} ]]; do
    # One JSON snapshot per poll; derive every reported field from it so the
    # status line is self-consistent and we make fewer API calls than one get
    # per field.
    POD_JSON=$(kubectl get pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" -o json 2>/dev/null || echo '{}')
    PHASE=$(printf '%s' "${POD_JSON}" | jq -r '.status.phase // "Unknown"')
    SCHED=$(printf '%s' "${POD_JSON}" | jq -r '([.status.conditions[]? | select(.type=="PodScheduled")][0]) as $c | (($c.reason // "") + ": " + ($c.message // ""))')
    CWAIT=$(printf '%s' "${POD_JSON}" | jq -r '.status.containerStatuses[0].state.waiting.reason // ""')
    READY=$(printf '%s' "${POD_JSON}" | jq -r '[.status.containerStatuses[]? | select(.ready)] | length')
    NCONT=$(printf '%s' "${POD_JSON}" | jq -r '(.spec.containers // []) | length')
    RESTARTS=$(printf '%s' "${POD_JSON}" | jq -r '[.status.containerStatuses[]?.restartCount] | add // 0')
    NODE=$(printf '%s' "${POD_JSON}" | jq -r '.spec.nodeName // ""')
    POD_IP=$(printf '%s' "${POD_JSON}" | jq -r '.status.podIP // ""')

    # Capture the pod VM as soon as CAA launches it, while it is still alive, and
    # log a one-line EC2 summary (state/type/ami/ip/launched) the first time.
    if [[ -z "${NEW_PODVM_ID}" ]]; then
        NEW_PODVM_ID="$(new_podvm_id || true)"
        if [[ -n "${NEW_PODVM_ID}" ]]; then
            log_detail "${ICON_ARROW} detected pod VM ${NEW_PODVM_ID}: $(podvm_describe_line "${NEW_PODVM_ID}")"
        fi
    fi
    # Pod-VM EC2 state, logged on every transition (pending -> running -> ...).
    PODVM_STATE=""
    if [[ -n "${NEW_PODVM_ID}" ]]; then
        PODVM_STATE="$(instance_state "${NEW_PODVM_ID}")"
        if [[ "${PODVM_STATE}" != "${PREV_PODVM_STATE}" ]]; then
            log_detail "${ICON_ARROW} pod VM ${NEW_PODVM_ID} state: ${PREV_PODVM_STATE:-<new>} -> ${PODVM_STATE}"
            PREV_PODVM_STATE="${PODVM_STATE}"
        fi
    fi
    # Log pod phase transitions explicitly (the per-poll line shows phase too).
    if [[ "${PHASE}" != "${PREV_PHASE}" ]]; then
        [[ -n "${PREV_PHASE}" ]] && log_detail "${ICON_ARROW} pod phase: ${PREV_PHASE} -> ${PHASE}"
        PREV_PHASE="${PHASE}"
    fi
    # Surface the latest pod Event (where FailedCreatePodSandBox etc. appear),
    # but only when it CHANGES so the same warning is not reprinted every poll.
    EVENT="$(pod_latest_event)"
    if [[ -n "${EVENT}" && "${EVENT}" != "${PREV_EVENT}" ]]; then
        log_detail "${ICON_ARROW} event: ${EVENT:0:200}"
        PREV_EVENT="${EVENT}"
    fi

    if [[ "${PHASE}" == "Succeeded" || "${PHASE}" == "Failed" ]]; then
        log_progress "[${ELAPSED}s] phase=${PHASE} ready=${READY}/${NCONT} restarts=${RESTARTS}${NODE:+ node=${NODE}}${POD_IP:+ ip=${POD_IP}}${NEW_PODVM_ID:+ podVM=${NEW_PODVM_ID}${PODVM_STATE:+(${PODVM_STATE})}}"
        break
    fi

    log_progress "[${ELAPSED}s] phase=${PHASE} ready=${READY}/${NCONT} restarts=${RESTARTS}${SCHED:+ sched=[${SCHED}]}${CWAIT:+ container=${CWAIT}}${NODE:+ node=${NODE}}${POD_IP:+ ip=${POD_IP}}${NEW_PODVM_ID:+ podVM=${NEW_PODVM_ID}${PODVM_STATE:+(${PODVM_STATE})}}"

    # Detect a stuck-Unschedulable pod and bail with a precise diagnosis.
    if [[ "${SCHED}" == Unschedulable:* || "${SCHED}" == *"Unschedulable"* ]]; then
        UNSCHED_ELAPSED=$((UNSCHED_ELAPSED + POLL))
        # Print the likely cause on the FIRST Unschedulable poll so a Ctrl-C
        # before the ${UNSCHED_GRACE}s full dump still shows what to look at.
        if [[ "${UNSCHED_HINTED:-0}" == "0" ]]; then
            UNSCHED_HINTED=1
            if [[ "${SCHED}" == *"Insufficient kata.peerpods.io/vm"* ]]; then
                log_detail "${ICON_ARROW} Unschedulable — the pod was likely mutated, but workers have no/free kata.peerpods.io/vm capacity. Full diagnostics in ${UNSCHED_GRACE}s."
            else
                log_detail "${ICON_ARROW} Unschedulable — if this is 'Insufficient cpu' either the webhook did NOT rewrite this pod (a mutated pod has NO cpu request and asks for kata.peerpods.io/vm:1) OR the kata-remote RuntimeClass still has non-zero overhead the scheduler adds. Full diagnostics in ${UNSCHED_GRACE}s."
            fi
        fi
        if [[ ${UNSCHED_ELAPSED} -ge ${UNSCHED_GRACE} ]]; then
            CPU_REQ=$(kubectl get pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
                -o jsonpath='{.spec.containers[0].resources.requests.cpu}' 2>/dev/null || true)
            VM_RES=$(kubectl get pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
                -o jsonpath='{.spec.containers[0].resources.requests.kata\.peerpods\.io/vm}' 2>/dev/null || true)
            log_info "--- unschedulable diagnostics ---"
            log_detail "pod still requests cpu=${CPU_REQ:-<none>}, kata.peerpods.io/vm=${VM_RES:-<none>}"
            log_detail "(a webhook-mutated kata-remote pod has NO cpu request and kata.peerpods.io/vm=1)"
            log_detail "peer-pods webhook effectiveness (caBundle/failurePolicy/endpoints):"
            peerpods_webhook_effective 2>/dev/null | indent || true
            log_detail "peer-pods-cm PEERPODS_LIMIT_PER_NODE:"
            kubectl -n "${CAA_NAMESPACE:-confidential-containers-system}" get configmap peer-pods-cm \
                -o jsonpath='    {.data.PEERPODS_LIMIT_PER_NODE}{"\n"}' 2>/dev/null || true
            log_detail "node capacity/allocatable kata.peerpods.io/vm:"
            peerpods_node_capacity_summary
            KR_OVERHEAD="$(kata_remote_overhead)"
            log_detail "kata-remote RuntimeClass overhead (podFixed): ${KR_OVERHEAD:-<none>}"
            log_detail "(the scheduler ADDS this to the pod; non-zero cpu here => 'Insufficient cpu' even though the pod requests 0 cpu)"
            log_info "--- end diagnostics ---"
            if [[ -n "${CPU_REQ:-}" ]]; then
                step_fail "kata-remote smoke pod stayed Unschedulable for ${UNSCHED_ELAPSED}s (${SCHED}).
  It still carries a cpu request, so the peer-pods mutating webhook did NOT rewrite it
  (mutated pods drop cpu/memory and request kata.peerpods.io/vm:1). The webhook is
  registered but not effective for this namespace — check, in order:
    1. the webhook pod is Ready and serving (cert-manager issued its cert):
         kubectl get pods -n ${CAA_NAMESPACE:-confidential-containers-system} | grep -i webhook
         kubectl get certificate,secret -n ${CAA_NAMESPACE:-confidential-containers-system} | grep -i webhook
    2. its namespaceSelector includes ${K8S_NAMESPACE_SYSTEM} (it may exclude non-agent namespaces)
    3. failurePolicy=Ignore silently admits unmutated pods when the webhook call fails
  If the webhook only targets ${K8S_NAMESPACE_AGENTS}, run the smoke test there
  (PEER_PODS_SMOKE_NAMESPACE) or label ${K8S_NAMESPACE_SYSTEM} for the webhook."
            elif [[ -n "${VM_RES:-}" && "${SCHED}" == *"Insufficient cpu"* \
                    && "${SCHED}" != *"Insufficient kata.peerpods.io/vm"* ]]; then
                step_fail "kata-remote smoke pod stayed Unschedulable for ${UNSCHED_ELAPSED}s (${SCHED}).
  The peer-pods webhook DID rewrite it (cpu=<none>, kata.peerpods.io/vm=${VM_RES}), so this is NOT a
  kata.peerpods.io/vm capacity problem. The scheduler reports 'Insufficient cpu' because the kata-remote
  RuntimeClass carries non-zero overhead (${KR_OVERHEAD:-<unknown>}) that it ADDS to the pod's effective
  requests — so the (otherwise 0-cpu) pod cannot fit on a busy node. kata-deploy re-stamps this overhead on
  every CAA install. Zero it (or re-run ./03-coco-operator.sh, which does), then re-run:
    kubectl patch runtimeclass kata-remote --type=merge -p '{\"overhead\":{\"podFixed\":{\"cpu\":\"0\",\"memory\":\"0\"}}}'"
            elif [[ -n "${VM_RES:-}" ]]; then
                step_fail "kata-remote smoke pod stayed Unschedulable for ${UNSCHED_ELAPSED}s (${SCHED}).
  The peer-pods webhook DID rewrite it (cpu=<none>, kata.peerpods.io/vm=${VM_RES}),
  but the scheduler cannot find worker capacity for kata.peerpods.io/vm. Re-run
  ./03-coco-operator.sh so CAA applies PEERPODS_LIMIT_PER_NODE and patches
  nodes/status; if it still fails, inspect peer-pods-cm, CAA logs, and whether
  the cloud-api-adaptor service account can patch nodes/status."
            else
                step_fail "kata-remote smoke pod stayed Unschedulable for ${UNSCHED_ELAPSED}s (${SCHED}).
  The pod has neither a cpu request nor kata.peerpods.io/vm request in the first
  container, so the peer-pods mutation result is unexpected. Inspect the pod spec
  and peer-pods webhook logs."
            fi
        fi
    else
        UNSCHED_ELAPSED=0
    fi

    # Fast-fail on a stuck container-create error: a kata-remote workload image
    # being unpacked on the worker (instead of guest-pulled), an in-guest image
    # pull failure, etc. These do not self-resolve, so don't burn the timeout.
    case "${CWAIT}" in
        CreateContainerError|RunContainerError|CreateContainerConfigError|ImagePullBackOff|ErrImagePull)
            CWAIT_ELAPSED=$((CWAIT_ELAPSED + POLL))
            if [[ ${CWAIT_ELAPSED} -ge ${CWAIT_GRACE} ]]; then
                CMSG=$(kubectl get pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
                    -o jsonpath='{.status.containerStatuses[0].state.waiting.message}' 2>/dev/null || true)
                log_info "--- container-create diagnostics (${CWAIT}) ---"
                log_detail "container message: ${CMSG:-<none>}"
                log_detail "pod events:"
                kubectl describe pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null \
                    | sed -n '/Events:/,$p' | indent || true
                log_detail "CAA daemonset log (tail):"
                kubectl logs -n "${CAA_NAMESPACE:-confidential-containers-system}" \
                    ds/"${CAA_DAEMONSET:-cloud-api-adaptor-daemonset}" --tail=80 2>/dev/null \
                    | indent || true
                log_info "--- end diagnostics ---"
                step_fail "kata-remote smoke pod stuck in ${CWAIT} for ${CWAIT_ELAPSED}s:
  '${CMSG:0:200}'
  An 'error unpacking image ... content digest not found' here means the WORKLOAD image is being
  unpacked on the worker instead of pulled inside the guest pod VM — the containerd guest-pull flags
  (disable_snapshot_annotations / discard_unpacked_layers) are not effective on this node. Ensure
  ./03-coco-operator.sh applied the guest-pull DaemonSet and it is Ready:
    kubectl -n ${CAA_NAMESPACE:-confidential-containers-system} get ds kata-remote-containerd-guestpull
    kubectl -n ${CAA_NAMESPACE:-confidential-containers-system} logs ds/kata-remote-containerd-guestpull"
            fi
            ;;
        *)
            CWAIT_ELAPSED=0
            # Benign-looking ContainerCreating: the pod is scheduled and CAA is
            # booting the pod VM / creating the sandbox. No error reason is set,
            # so make it observable and bounded instead of silent.
            if [[ "${PHASE}" == "Pending" \
                  && ( "${CWAIT}" == "ContainerCreating" || -z "${CWAIT}" ) \
                  && "${SCHED}" != *Unschedulable* ]]; then
                CC_ELAPSED=$((CC_ELAPSED + POLL))
                if [[ $((CC_ELAPSED - CC_LAST_DIAG)) -ge ${CC_DIAG_INTERVAL} ]]; then
                    CC_LAST_DIAG=${CC_ELAPSED}
                    log_detail "${ICON_ARROW} still ContainerCreating after ${CC_ELAPSED}s (CAA pod-VM boot/sandbox) — live diagnostics:"
                    dump_containercreating_diagnostics
                fi
                if [[ ${CC_ELAPSED} -ge ${CC_GRACE} ]]; then
                    dump_containercreating_diagnostics
                    step_fail "kata-remote smoke pod stuck in ContainerCreating for ${CC_ELAPSED}s (CAA pod-VM boot/sandbox never completed).
  The pod scheduled (webhook + capacity are fine) but its sandbox never came up. Most likely, in order:
    1. the pod VM booted but the worker shim cannot reach the in-guest agent (forwarder :${CAA_FORWARDER_PORT:-15150} / vxlan :${CAA_VXLAN_PORT:-9000}, or the guest never finished booting/attesting),
    2. CAA never launched a pod VM (RunInstances denied / bad AMI / instance-type SEV-SNP availability), or
    3. the in-guest CDH cannot reach KBS/Trustee to attest and release the resource.
  See the diagnostics above (sandbox Events, pod-VM EC2 state, CAA daemonset log).
  Raise PEER_PODS_CONTAINERCREATING_GRACE_SECS if your pod VMs legitimately need longer to boot."
                fi
            else
                CC_ELAPSED=0
                CC_LAST_DIAG=0
            fi
            ;;
    esac

    # Fast-fail a pod that is Running but never Succeeds. The container started,
    # so sandbox/boot/attestation-of-the-VM are fine; the only thing left is the
    # in-guest CDH fetch, which on a healthy guest releases the DEK within
    # seconds. Staying Running past the grace means every fetch is failing and
    # the container would only surface CDH_FETCH_FAILED after its full ~300s
    # retry budget. Bail now with its CDH_ATTEMPT log (the per-retry curl error).
    if [[ "${PHASE}" == "Running" ]]; then
        RUNNING_ELAPSED=$((RUNNING_ELAPSED + POLL))
        if [[ ${RUNNING_ELAPSED} -ge ${RUNNING_GRACE} ]]; then
            log_info "--- running-but-not-succeeded diagnostics ---"
            log_detail "container has been up ${RUNNING_ELAPSED}s without fetching the DEK (a healthy"
            log_detail "in-guest CDH fetch succeeds within seconds), so attestation/DEK release is failing."
            log_detail "smoke container log (CDH_ATTEMPT lines carry the per-retry curl error):"
            kubectl logs "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" --tail=40 2>/dev/null \
                | indent || true
            log_info "--- end diagnostics ---"
            step_fail "kata-remote smoke pod stayed Running for ${RUNNING_ELAPSED}s without succeeding.
  The pod VM booted and the smoke container started, so the only remaining step — the in-guest CDH
  fetching the attestation-gated DEK — is failing on every retry. See the CDH_ATTEMPT errors above; the
  usual causes, in order:
    1. the pod VM's SG cannot reach the KBS pod IP on 8080 over the VPC (node SG must admit TCP 8080 from the pod-VM SG),
    2. KBS/Trustee rejects the attestation (policy / SEV-SNP evidence), so the resource is never released, or
    3. the scratch resource is not at the \\x2F-escaped LocalFs path KBS reads (attestation OK but resource 404s).
  Raise PEER_PODS_RUNNING_GRACE_SECS if your guest legitimately needs longer to attest+fetch."
        fi
    else
        RUNNING_ELAPSED=0
    fi

    sleep "${POLL}"
    ELAPSED=$((ELAPSED + POLL))
done

# One last look in case the VM appeared right as the pod completed.
if [[ -z "${NEW_PODVM_ID}" ]]; then
    NEW_PODVM_ID="$(new_podvm_id || true)"
fi

if [[ "${PHASE}" != "Succeeded" ]]; then
    log_info "--- smoke pod post-mortem (ended phase ${PHASE}) ---"
    log_detail "pod events:"
    kubectl describe pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null \
        | sed -n '/Events:/,$p' | indent || true
    log_detail "pod logs (CDH_VALUE: on success, CDH_FETCH_FAILED if the in-guest fetch timed out):"
    kubectl logs "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null | indent || true
    if [[ -n "${NEW_PODVM_ID}" ]]; then
        log_detail "pod VM ${NEW_PODVM_ID}:"
        aws ec2 describe-instances --region "${AWS_REGION}" --instance-ids "${NEW_PODVM_ID}" \
            --query 'Reservations[].Instances[].{id:InstanceId,state:State.Name,type:InstanceType,ami:ImageId,launched:LaunchTime,reason:StateReason.Message}' \
            --output table 2>&1 | indent || true
    fi
    log_detail "CAA daemonset log (tail):"
    kubectl logs -n "${CAA_NAMESPACE:-confidential-containers-system}" \
        ds/"${CAA_DAEMONSET:-cloud-api-adaptor-daemonset}" --tail=80 2>/dev/null \
        | indent || true
    log_info "--- end post-mortem ---"
    step_fail "peer-pods smoke pod ended in phase '${PHASE}' — CAA pod-VM boot, attestation, or DEK release did not succeed"
fi

#------------------------------------------------------------------------------
# 3a. Assert a real EC2 pod VM was created during the run
#------------------------------------------------------------------------------

if [[ -z "${NEW_PODVM_ID}" ]]; then
    step_fail "pod succeeded but no new EC2 pod VM matching '${PODVM_NAME_FILTER}' was observed — CAA did not launch a pod VM (or the tag filter is wrong; see PODVM_NAME_FILTER TODO)"
fi
log_ok "CAA launched a pod VM during the run (${NEW_PODVM_ID})"

#------------------------------------------------------------------------------
# 3b. Verify the released value matches what we provisioned
#------------------------------------------------------------------------------

# The container prints the released DEK as a single `CDH_VALUE:<hex>` line; the
# `^CDH_VALUE:` anchor deliberately ignores the per-retry `CDH_ATTEMPT ...`
# diagnostics. Keep the `|| true` so a missing line falls through to the explicit
# mismatch check below (a clear step_fail) instead of tripping `set -e`/pipefail
# with no message.
FETCHED=$(kubectl logs "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null \
    | grep '^CDH_VALUE:' | head -1 | cut -d: -f2- || true)

if [[ "${FETCHED}" != "${SMOKE_VALUE}" ]]; then
    step_fail "fetched DEK does not match the provisioned value (got '${FETCHED:0:16}...', wanted '${SMOKE_VALUE:0:16}...')"
fi
log_ok "Pod VM booted, attested, and fetched the scratch DEK via CDH"
log_ok "Value matches the provisioned resource — end-to-end peer-pod attestation works"

#------------------------------------------------------------------------------
# 3c. Headline assertion: the pod VM does NOT leak. CAA reaps a peer pod's VM
# when the pod goes away — in practice it starts as soon as the container exits
# (a Succeeded pod's VM is often already shutting-down/terminated before the
# delete below). We still delete explicitly and then REQUIRE the VM to reach
# shutting-down/terminated, so a leaked instance (silent cost/quota drain) fails.
#------------------------------------------------------------------------------

echo ""
log_info "Deleting the smoke pod and asserting pod VM ${NEW_PODVM_ID} is reaped (CAA may already have started on container exit)..."
kubectl delete pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" --ignore-not-found --wait=true --timeout=120s >/dev/null 2>&1 || true

TERM_TIMEOUT="${PEER_PODS_TERMINATE_TIMEOUT_SECS:-300}"
TERM_POLL=10
TERM_ELAPSED=0
PODVM_STATE=""
PREV_TERM_STATE=""
while [[ ${TERM_ELAPSED} -le ${TERM_TIMEOUT} ]]; do
    PODVM_STATE="$(instance_state "${NEW_PODVM_ID}")"
    if [[ "${PODVM_STATE}" != "${PREV_TERM_STATE}" ]]; then
        log_detail "${ICON_ARROW} pod VM ${NEW_PODVM_ID} state: ${PREV_TERM_STATE:-<new>} -> ${PODVM_STATE}"
        PREV_TERM_STATE="${PODVM_STATE}"
    fi
    if [[ "${PODVM_STATE}" == "shutting-down" || "${PODVM_STATE}" == "terminated" ]]; then
        break
    fi
    log_progress "[${TERM_ELAPSED}s] pod VM ${NEW_PODVM_ID}: ${PODVM_STATE}"
    sleep "${TERM_POLL}"
    TERM_ELAPSED=$((TERM_ELAPSED + TERM_POLL))
done

if [[ "${PODVM_STATE}" != "shutting-down" && "${PODVM_STATE}" != "terminated" ]]; then
    log_info "--- leaked pod VM diagnostics ---"
    aws ec2 describe-instances --region "${AWS_REGION}" --instance-ids "${NEW_PODVM_ID}" \
        --query 'Reservations[].Instances[].{id:InstanceId,state:State.Name,type:InstanceType,launched:LaunchTime}' \
        --output table 2>&1 | indent || true
    log_info "--- end diagnostics ---"
    step_fail "pod VM ${NEW_PODVM_ID} is still '${PODVM_STATE}' ${TERM_TIMEOUT}s after pod deletion — possible leaked instance (cost/quota); the cleanup trap will terminate it, but CAA should reap it automatically"
fi
log_ok "Pod VM ${NEW_PODVM_ID} is ${PODVM_STATE} after pod deletion — no leaked instances"
# Already terminating/terminated, so the cleanup safety net has nothing to do.
NEW_PODVM_ID=""

step_ok "06 (./06-build-harness.sh)"
