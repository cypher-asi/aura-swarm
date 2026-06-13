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

step_banner "05" "Peer Pods (kata-remote) attestation smoke test"

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

cleanup() {
    kubectl delete pod "${SMOKE_POD}" "${SMOKE_SETTER_POD}" "${SMOKE_RM_POD}" \
        -n "${K8S_NAMESPACE_SYSTEM}" --ignore-not-found --wait=false >/dev/null 2>&1 || true
    # Remove the scratch resource file from the KBS LocalFs repository so no
    # throwaway key material outlives the test (best-effort; mounts the RWX PVC).
    kubectl run "${SMOKE_RM_POD}" -n "${K8S_NAMESPACE_SYSTEM}" --restart=Never --quiet \
        --image="${SMOKE_WRITER_IMAGE}" \
        --overrides="{\"spec\":{\"volumes\":[{\"name\":\"repo\",\"persistentVolumeClaim\":{\"claimName\":\"${KBS_REPO_PVC}\"}}],\"containers\":[{\"name\":\"rm\",\"image\":\"${SMOKE_WRITER_IMAGE}\",\"command\":[\"rm\",\"-f\",\"${KBS_REPO_DIR}/${SMOKE_RESOURCE_PATH}\"],\"volumeMounts\":[{\"name\":\"repo\",\"mountPath\":\"${KBS_REPO_DIR}\"}]}]}}" \
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
                echo -e "${YELLOW}⚠${NC} Terminating leftover pod VM ${NEW_PODVM_ID} (cleanup safety net)..." >&2
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
echo "Pod VMs matching '${PODVM_NAME_FILTER}' before launch: ${BEFORE_COUNT}"

#------------------------------------------------------------------------------
# 1. Provision the scratch KBS resource
#------------------------------------------------------------------------------

SMOKE_VALUE=$(openssl rand -hex 32)

echo "Provisioning scratch KBS resource ${SMOKE_RESOURCE_PATH} (writing into the ${KBS_REPO_PVC} PVC)..."
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
        - "mkdir -p '${KBS_REPO_DIR}/${SMOKE_RESOURCE_DIR}' && printf '%s' '${SMOKE_VALUE}' > '${KBS_REPO_DIR}/${SMOKE_RESOURCE_PATH}' && echo wrote '${KBS_REPO_DIR}/${SMOKE_RESOURCE_PATH}' && ls -l '${KBS_REPO_DIR}/${SMOKE_RESOURCE_PATH}'"
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
echo "Waiting for the KBS resource writer pod (timeout ${SETTER_TIMEOUT}s)..."
while [[ ${SETTER_ELAPSED} -le ${SETTER_TIMEOUT} ]]; do
    SETTER_PHASE=$(kubectl get pod "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.phase}' 2>/dev/null || echo "Unknown")
    [[ "${SETTER_PHASE}" == "Succeeded" || "${SETTER_PHASE}" == "Failed" ]] && break
    SETTER_WAIT=$(kubectl get pod "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}' 2>/dev/null || true)
    echo "  [${SETTER_ELAPSED}s] setter pod: ${SETTER_PHASE:-Pending} ${SETTER_WAIT:+(${SETTER_WAIT})}"
    sleep "${SETTER_POLL}"
    SETTER_ELAPSED=$((SETTER_ELAPSED + SETTER_POLL))
done

if [[ "${SETTER_PHASE}" != "Succeeded" ]]; then
    echo -e "${YELLOW}--- KBS resource writer diagnostics ---${NC}"
    echo "  image: ${SMOKE_WRITER_IMAGE}"
    echo "  phase: $(kubectl get pod "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" -o jsonpath='{.status.phase}' 2>/dev/null)"
    echo "  container state:"
    kubectl get pod "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{range .status.containerStatuses[*]}    {.name}: ready={.ready} waiting={.state.waiting.reason} {.state.waiting.message} terminated={.state.terminated.reason}(exit {.state.terminated.exitCode}){"\n"}{end}' 2>/dev/null || true
    echo "  events:"
    kubectl describe pod "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null \
        | sed -n '/Events:/,$p' | sed 's/^/    /' || true
    echo "  logs (current):"
    kubectl logs "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>&1 | sed 's/^/    /' || true
    echo -e "${YELLOW}--- end diagnostics ---${NC}"
    step_fail "could not provision the scratch KBS resource (writer pod failed)"
fi
echo -e "${GREEN}✓${NC} Scratch resource provisioned in KBS"

#------------------------------------------------------------------------------
# 2. Throwaway kata-remote pod: CAA boots a pod VM that attests + fetches via CDH
#------------------------------------------------------------------------------

echo ""
echo "Launching throwaway pod on kata-remote (CAA pod-VM boot + attestation can take a few minutes)..."
kubectl apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: ${SMOKE_POD}
  namespace: ${K8S_NAMESPACE_SYSTEM}
  labels:
    app: peer-pods-smoke-test
spec:
  restartPolicy: Never
  runtimeClassName: kata-remote
  containers:
    - name: smoke
      image: curlimages/curl:8.10.1
      command:
        - sh
        - -c
        - |
          i=0
          while [ \$i -lt 60 ]; do
            v=\$(curl -fsS --max-time 10 "${CDH_RESOURCE_URL}/${SMOKE_RESOURCE_PATH}" 2>/dev/null || true)
            if [ -n "\$v" ]; then
              echo "CDH_VALUE:\$v"
              exit 0
            fi
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
while [[ ${ELAPSED} -le ${TIMEOUT} ]]; do
    PHASE=$(kubectl get pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.phase}' 2>/dev/null || echo "Unknown")
    # Capture the pod VM as soon as CAA launches it, while it is still alive.
    if [[ -z "${NEW_PODVM_ID}" ]]; then
        NEW_PODVM_ID="$(new_podvm_id || true)"
        [[ -n "${NEW_PODVM_ID}" ]] && echo "  detected pod VM: ${NEW_PODVM_ID}"
    fi
    if [[ "${PHASE}" == "Succeeded" || "${PHASE}" == "Failed" ]]; then
        break
    fi
    SCHED=$(kubectl get pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{range .status.conditions[?(@.type=="PodScheduled")]}{.reason}: {.message}{end}' 2>/dev/null || true)
    CWAIT=$(kubectl get pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}' 2>/dev/null || true)
    echo "  [${ELAPSED}s] pod phase: ${PHASE}${SCHED:+  sched=[${SCHED}]}${CWAIT:+  container=${CWAIT}}${NEW_PODVM_ID:+  podVM=${NEW_PODVM_ID}}"
    sleep "${POLL}"
    ELAPSED=$((ELAPSED + POLL))
done

# One last look in case the VM appeared right as the pod completed.
if [[ -z "${NEW_PODVM_ID}" ]]; then
    NEW_PODVM_ID="$(new_podvm_id || true)"
fi

if [[ "${PHASE}" != "Succeeded" ]]; then
    echo "Pod events:"
    kubectl describe pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null | tail -20 | sed 's/^/  /' || true
    echo "Pod logs:"
    kubectl logs "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null | sed 's/^/  /' || true
    step_fail "peer-pods smoke pod ended in phase '${PHASE}' — CAA pod-VM boot, attestation, or DEK release did not succeed"
fi

#------------------------------------------------------------------------------
# 3a. Assert a real EC2 pod VM was created during the run
#------------------------------------------------------------------------------

if [[ -z "${NEW_PODVM_ID}" ]]; then
    step_fail "pod succeeded but no new EC2 pod VM matching '${PODVM_NAME_FILTER}' was observed — CAA did not launch a pod VM (or the tag filter is wrong; see PODVM_NAME_FILTER TODO)"
fi
echo -e "${GREEN}✓${NC} CAA launched a pod VM during the run (${NEW_PODVM_ID})"

#------------------------------------------------------------------------------
# 3b. Verify the released value matches what we provisioned
#------------------------------------------------------------------------------

FETCHED=$(kubectl logs "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
    | grep '^CDH_VALUE:' | head -1 | cut -d: -f2-)

if [[ "${FETCHED}" != "${SMOKE_VALUE}" ]]; then
    step_fail "fetched DEK does not match the provisioned value (got '${FETCHED:0:16}...', wanted '${SMOKE_VALUE:0:16}...')"
fi
echo -e "${GREEN}✓${NC} Pod VM booted, attested, and fetched the scratch DEK via CDH"
echo -e "${GREEN}✓${NC} Value matches the provisioned resource — end-to-end peer-pod attestation works"

#------------------------------------------------------------------------------
# 3c. Headline assertion: deleting the pod TERMINATES the pod VM (no leak)
#------------------------------------------------------------------------------

echo ""
echo "Deleting the smoke pod and asserting CAA terminates pod VM ${NEW_PODVM_ID}..."
kubectl delete pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" --ignore-not-found --wait=true --timeout=120s >/dev/null 2>&1 || true

TERM_TIMEOUT="${PEER_PODS_TERMINATE_TIMEOUT_SECS:-300}"
TERM_POLL=10
TERM_ELAPSED=0
PODVM_STATE=""
while [[ ${TERM_ELAPSED} -le ${TERM_TIMEOUT} ]]; do
    PODVM_STATE="$(instance_state "${NEW_PODVM_ID}")"
    if [[ "${PODVM_STATE}" == "shutting-down" || "${PODVM_STATE}" == "terminated" ]]; then
        break
    fi
    echo "  [${TERM_ELAPSED}s] pod VM ${NEW_PODVM_ID}: ${PODVM_STATE}"
    sleep "${TERM_POLL}"
    TERM_ELAPSED=$((TERM_ELAPSED + TERM_POLL))
done

if [[ "${PODVM_STATE}" != "shutting-down" && "${PODVM_STATE}" != "terminated" ]]; then
    echo -e "${YELLOW}--- leaked pod VM diagnostics ---${NC}"
    aws ec2 describe-instances --region "${AWS_REGION}" --instance-ids "${NEW_PODVM_ID}" \
        --query 'Reservations[].Instances[].{id:InstanceId,state:State.Name,type:InstanceType,launched:LaunchTime}' \
        --output table 2>&1 | sed 's/^/  /' || true
    echo -e "${YELLOW}--- end diagnostics ---${NC}"
    step_fail "pod VM ${NEW_PODVM_ID} is still '${PODVM_STATE}' ${TERM_TIMEOUT}s after pod deletion — possible leaked instance (cost/quota); the cleanup trap will terminate it, but CAA should reap it automatically"
fi
echo -e "${GREEN}✓${NC} Pod VM ${NEW_PODVM_ID} is ${PODVM_STATE} after pod deletion — no leaked instances"
# Already terminating/terminated, so the cleanup safety net has nothing to do.
NEW_PODVM_ID=""

step_ok "06 (./06-build-harness.sh)"
