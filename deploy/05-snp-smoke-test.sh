#!/bin/bash
# 05-snp-smoke-test.sh - End-to-end attestation proof BEFORE any product
# traffic: a throwaway pod on kata-qemu-snp boots a confidential VM, attests
# against the Trustee KBS, and fetches a scratch DEK through the in-guest CDH.
#
# Flow:
#   1. provision a scratch KBS resource (random value) via kbs-client, using
#      the admin key from .secrets/kbs-admin.key
#   2. run a throwaway SNP pod that fetches the resource through the CDH
#      (release only succeeds after SNP attestation passes)
#   3. compare values, then clean up the pod AND the scratch resource
#
# Usage: ./05-snp-smoke-test.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "05" "SNP attestation smoke test"

# This test exercises the on-node kata-qemu-snp path. Under peer_pods the
# confidential VM is an off-cluster AWS pod VM (kata-remote) with different
# assertions (pod-VM create/terminate), so redirect to the peer-pods variant
# and exit 0 so the numbered sequence still flows to step 06.
CONFIDENTIAL_RUNTIME="${CONFIDENTIAL_RUNTIME:-snp_local}"
if [[ "${CONFIDENTIAL_RUNTIME}" == "peer_pods" ]]; then
    echo -e "${YELLOW}ℹ${NC} CONFIDENTIAL_RUNTIME=peer_pods — this on-node SNP test does not apply."
    echo "  Run the Peer Pods smoke test instead:"
    echo ""
    echo "    ./05-peer-pods-smoke-test.sh"
    echo ""
    step_ok "06 (./06-deploy-r1.sh)"
    exit 0
fi

require_cmds aws kubectl jq openssl
require_aws_auth
ensure_kubectl_context

[[ -f "${SECRETS_DIR}/kbs-admin.key" ]] \
    || step_fail "missing .secrets/kbs-admin.key — run ./04-trustee-kbs.sh first"

# kbs-client is distributed as an ORAS artifact (a binary), not a runnable
# container image, and the KBS image is distroless (no shell to exec into).
# So we provision the scratch resource by writing it directly into the KBS
# LocalFs repository (dir_path in kbs-config) via a throwaway pod that mounts
# the same RWX (EFS) PVC the KBS reads from.
SMOKE_WRITER_IMAGE="${SMOKE_WRITER_IMAGE:-busybox:1.36}"
KBS_REPO_PVC="kbs-repository"
KBS_REPO_DIR="/opt/confidential-containers/kbs"
# CoCo guest CDH RESTful API (resource release is attestation-gated)
CDH_RESOURCE_URL="${CDH_RESOURCE_URL:-http://127.0.0.1:8006/cdh/resource}"
SMOKE_RESOURCE_PATH="default/snp-smoke-test/dek"
SMOKE_RESOURCE_DIR="$(dirname "${SMOKE_RESOURCE_PATH}")"

SMOKE_SETTER_POD="snp-smoke-kbs-setter"
SMOKE_RM_POD="snp-smoke-kbs-rm"
SMOKE_POD="snp-smoke-test"

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
}
trap cleanup EXIT

# Idempotent re-run: clear leftovers from a previous partial failure first.
cleanup

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
echo "Waiting for the kbs-client setter pod (timeout ${SETTER_TIMEOUT}s)..."
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
    echo "  logs (previous):"
    kubectl logs "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" --previous 2>&1 | sed 's/^/    /' || true
    echo -e "${YELLOW}--- end diagnostics ---${NC}"
    step_fail "could not provision the scratch KBS resource (kbs-client setter pod failed)"
fi
echo -e "${GREEN}✓${NC} Scratch resource provisioned in KBS"

#------------------------------------------------------------------------------
# 2. Throwaway SNP pod: attest + fetch the resource via CDH
#------------------------------------------------------------------------------

echo ""
echo "Launching throwaway pod on kata-qemu-snp (CoCo VM boot + attestation can take a few minutes)..."
kubectl apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: ${SMOKE_POD}
  namespace: ${K8S_NAMESPACE_SYSTEM}
  labels:
    app: snp-smoke-test
spec:
  restartPolicy: Never
  runtimeClassName: kata-qemu-snp
  tolerations:
    - key: swarm.io/confidential-node
      operator: Equal
      value: "true"
      effect: NoSchedule
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

TIMEOUT="${SNP_SMOKE_TIMEOUT_SECS:-900}"
POLL=10
ELAPSED=0
PHASE=""
while [[ ${ELAPSED} -le ${TIMEOUT} ]]; do
    PHASE=$(kubectl get pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.phase}' 2>/dev/null || echo "Unknown")
    if [[ "${PHASE}" == "Succeeded" || "${PHASE}" == "Failed" ]]; then
        break
    fi
    SCHED=$(kubectl get pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{range .status.conditions[?(@.type=="PodScheduled")]}{.reason}: {.message}{end}' 2>/dev/null || true)
    CWAIT=$(kubectl get pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}' 2>/dev/null || true)
    echo "  [${ELAPSED}s] pod phase: ${PHASE}${SCHED:+  sched=[${SCHED}]}${CWAIT:+  container=${CWAIT}}"
    sleep "${POLL}"
    ELAPSED=$((ELAPSED + POLL))
done

if [[ "${PHASE}" != "Succeeded" ]]; then
    echo "Pod events:"
    kubectl describe pod "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null | tail -20 | sed 's/^/  /' || true
    echo "Pod logs:"
    kubectl logs "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null | sed 's/^/  /' || true
    step_fail "SNP smoke pod ended in phase '${PHASE}' — attestation or DEK release did not succeed"
fi

#------------------------------------------------------------------------------
# 3. Verify the released value matches what we provisioned
#------------------------------------------------------------------------------

FETCHED=$(kubectl logs "${SMOKE_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
    | grep '^CDH_VALUE:' | head -1 | cut -d: -f2-)

if [[ "${FETCHED}" != "${SMOKE_VALUE}" ]]; then
    step_fail "fetched DEK does not match the provisioned value (got '${FETCHED:0:16}...', wanted '${SMOKE_VALUE:0:16}...')"
fi
echo -e "${GREEN}✓${NC} Confidential VM booted, attested, and fetched the scratch DEK via CDH"
echo -e "${GREEN}✓${NC} Value matches the provisioned resource — end-to-end attestation works"

step_ok "06 (./06-deploy-r1.sh)"
