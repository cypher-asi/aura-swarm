#!/bin/bash
# 04-snp-smoke-test.sh - End-to-end attestation proof BEFORE any product
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
# Usage: ./04-snp-smoke-test.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "04" "SNP attestation smoke test"

require_cmds aws kubectl jq openssl
require_aws_auth
ensure_kubectl_context

[[ -f "${SECRETS_DIR}/kbs-admin.key" ]] \
    || step_fail "missing .secrets/kbs-admin.key — run ./03-trustee-kbs.sh first"

KBS_CLIENT_IMAGE="${KBS_CLIENT_IMAGE:-ghcr.io/confidential-containers/staged-images/kbs-client:latest}"
KBS_URL="http://kbs.${K8S_NAMESPACE_SYSTEM}.svc.cluster.local:8080"
# CoCo guest CDH RESTful API (resource release is attestation-gated)
CDH_RESOURCE_URL="${CDH_RESOURCE_URL:-http://127.0.0.1:8006/cdh/resource}"
SMOKE_RESOURCE_PATH="default/snp-smoke-test/dek"

SMOKE_SECRET="snp-smoke-admin"
SMOKE_SETTER_POD="snp-smoke-kbs-setter"
SMOKE_POD="snp-smoke-test"

cleanup() {
    kubectl delete pod "${SMOKE_POD}" "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" \
        --ignore-not-found --wait=false >/dev/null 2>&1 || true
    kubectl delete secret "${SMOKE_SECRET}" -n "${K8S_NAMESPACE_SYSTEM}" \
        --ignore-not-found >/dev/null 2>&1 || true
    # Delete the scratch resource from the KBS LocalFs repository so no
    # throwaway key material outlives the test.
    kubectl exec -n "${K8S_NAMESPACE_SYSTEM}" deploy/kbs -- \
        rm -f "/opt/confidential-containers/kbs/${SMOKE_RESOURCE_PATH}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# Idempotent re-run: clear leftovers from a previous partial failure first.
cleanup

#------------------------------------------------------------------------------
# 1. Provision the scratch KBS resource
#------------------------------------------------------------------------------

SMOKE_VALUE=$(openssl rand -hex 32)

echo "Provisioning scratch KBS resource ${SMOKE_RESOURCE_PATH}..."
kubectl create secret generic "${SMOKE_SECRET}" -n "${K8S_NAMESPACE_SYSTEM}" \
    --from-file=kbs-admin.key="${SECRETS_DIR}/kbs-admin.key" \
    --from-literal=value="${SMOKE_VALUE}" >/dev/null

kubectl apply -f - >/dev/null <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: ${SMOKE_SETTER_POD}
  namespace: ${K8S_NAMESPACE_SYSTEM}
spec:
  restartPolicy: Never
  containers:
    - name: kbs-client
      image: ${KBS_CLIENT_IMAGE}
      command:
        - kbs-client
        - --url
        - ${KBS_URL}
        - config
        - --auth-private-key
        - /admin/kbs-admin.key
        - set-resource
        - --path
        - ${SMOKE_RESOURCE_PATH}
        - --resource-file
        - /admin/value
      volumeMounts:
        - { name: admin, mountPath: /admin, readOnly: true }
  volumes:
    - name: admin
      secret:
        secretName: ${SMOKE_SECRET}
EOF

if ! kubectl wait --for=jsonpath='{.status.phase}'=Succeeded "pod/${SMOKE_SETTER_POD}" \
    -n "${K8S_NAMESPACE_SYSTEM}" --timeout=180s >/dev/null 2>&1; then
    echo "kbs-client output:"
    kubectl logs "${SMOKE_SETTER_POD}" -n "${K8S_NAMESPACE_SYSTEM}" 2>/dev/null | sed 's/^/  /' || true
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
    echo "  [${ELAPSED}s] pod phase: ${PHASE}"
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

step_ok "05 (./05-deploy-r1.sh)"
