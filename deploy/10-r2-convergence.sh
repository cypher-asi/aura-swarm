#!/bin/bash
# 10-r2-convergence.sh - R2 convergence GATE. Refuses to pass unless the
# fleet has fully converged on the new architecture:
#   - gateway reports store schema_version=2
#   - every agent record carries a tier + sealed storage (billing sku source)
#   - every running swarm-agent pod is on kata-qemu-snp
#   - zero pods on kata-fc
#   - spot-check: N agent pods report the sealed-state env contract
#
# Safe to run repeatedly while stragglers (hibernating agents) wake/migrate.
#
# Usage:
#   ./10-r2-convergence.sh
#   R2_SPOT_CHECK_COUNT=5 ./10-r2-convergence.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "10" "R2 convergence gate"

require_cmds aws kubectl jq curl
require_aws_auth
ensure_kubectl_context
trap gw_stop_port_forward EXIT

FAILURES=0
gate() { # gate <ok:0|1> <pass-msg> <fail-msg>
    if [[ "$1" -eq 0 ]]; then
        echo -e "${GREEN}✓${NC} $2"
    else
        echo -e "${RED}✗${NC} $3"
        FAILURES=$((FAILURES + 1))
    fi
}

#------------------------------------------------------------------------------
# 1. Schema version
#------------------------------------------------------------------------------

gw_start_port_forward || step_fail "gateway port-forward failed"
SCHEMA=$(gw_schema_version || echo "null")
ALL_AGENTS=$(gw_internal_get "/internal/agents/all" || echo "[]")
gw_stop_port_forward

[[ "${SCHEMA}" == "2" ]] && RES=0 || RES=1
gate "${RES}" \
    "Store schema_version=2" \
    "schema_version=${SCHEMA} (expected 2) — the v1->v2 migration has not completed"

#------------------------------------------------------------------------------
# 2. Every record tiered + sealed (this is what drives sku-based billing
#    payloads: the reporter derives the sku from the agent's tier)
#------------------------------------------------------------------------------

TOTAL=$(echo "${ALL_AGENTS}" | jq 'length')
UNTIERED=$(echo "${ALL_AGENTS}" | jq '[.[] | select(.spec.tier == null)] | length')
UNSEALED=$(echo "${ALL_AGENTS}" | jq '[.[] | select(.spec.storage_encryption == null)] | length')

[[ "${UNTIERED}" == "0" ]] && RES=0 || RES=1
gate "${RES}" \
    "All ${TOTAL} agent record(s) carry a tier (billing skus derivable for every agent)" \
    "${UNTIERED}/${TOTAL} agent record(s) have no tier"
if [[ "${UNTIERED}" != "0" ]]; then
    echo "${ALL_AGENTS}" | jq -r '.[] | select(.spec.tier == null) | "    \(.agent_id)  \(.name)  status=\(.status)"'
fi

[[ "${UNSEALED}" == "0" ]] && RES=0 || RES=1
gate "${RES}" \
    "All agent records report sealed storage encryption" \
    "${UNSEALED}/${TOTAL} agent record(s) lack storage_encryption=sealed"

#------------------------------------------------------------------------------
# 3. Runtime classes of every running pod
#------------------------------------------------------------------------------

PODS_JSON=$(mktemp)
snapshot_agent_pods "${PODS_JSON}"
# Running pods only: a terminal/Pending kata-fc pod is not a workload running
# under the wrong isolation, and must not falsely fail (or pass) the gate.
POD_TOTAL=$(jq '[.[] | select(.phase == "Running")] | length' "${PODS_JSON}")
KATA_FC=$(jq '[.[] | select(.runtime_class == "kata-fc" and .phase == "Running")] | length' "${PODS_JSON}")
NON_SNP=$(jq '[.[] | select(.phase == "Running" and .runtime_class != "kata-qemu-snp")] | length' "${PODS_JSON}")

[[ "${KATA_FC}" == "0" ]] && RES=0 || RES=1
gate "${RES}" \
    "Zero pods on kata-fc" \
    "${KATA_FC} pod(s) still on kata-fc:"
if [[ "${KATA_FC}" != "0" ]]; then
    jq -r '.[] | select(.runtime_class == "kata-fc" and .phase == "Running") | "    \(.agent_id)  \(.pod_name)"' "${PODS_JSON}"
fi

[[ "${NON_SNP}" == "0" ]] && RES=0 || RES=1
gate "${RES}" \
    "All ${POD_TOTAL} running agent pod(s) on kata-qemu-snp" \
    "${NON_SNP}/${POD_TOTAL} pod(s) not on kata-qemu-snp"

#------------------------------------------------------------------------------
# 4. Spot-check N pods for the sealed-state env contract
#------------------------------------------------------------------------------

SPOT_N="${R2_SPOT_CHECK_COUNT:-3}"
SPOT_FAILS=0
SPOT_CHECKED=0
while IFS= read -r pod; do
    [[ -z "${pod}" ]] && continue
    SPOT_CHECKED=$((SPOT_CHECKED + 1))
    pod_json=$(kubectl get pod "${pod}" -n "${K8S_NAMESPACE_AGENTS}" -o json 2>/dev/null || echo "{}")
    sealed=$(echo "${pod_json}" | jq -r '[.spec.containers[0].env[]? | select(.name=="AURA_STATE_ENCRYPTION") | .value] | first // ""')
    key_id=$(echo "${pod_json}" | jq -r '[.spec.containers[0].env[]? | select(.name=="AURA_STATE_KEY_ID") | .value] | first // ""')
    if [[ "${sealed}" == "sealed" && -n "${key_id}" ]]; then
        echo "    ${pod}: sealed, key_id=${key_id}"
    else
        echo "    ${pod}: AURA_STATE_ENCRYPTION='${sealed}' AURA_STATE_KEY_ID='${key_id}'"
        SPOT_FAILS=$((SPOT_FAILS + 1))
    fi
done < <(jq -r --argjson n "${SPOT_N}" '.[0:$n][].pod_name' "${PODS_JSON}")
rm -f "${PODS_JSON}"

if [[ "${SPOT_CHECKED}" -eq 0 ]]; then
    echo -e "${YELLOW}⚠${NC} No running agent pods to spot-check (empty fleet or everything hibernating)"
else
    [[ "${SPOT_FAILS}" == "0" ]] && RES=0 || RES=1
    gate "${RES}" \
        "Spot-checked ${SPOT_CHECKED} pod(s): sealed state contract present" \
        "${SPOT_FAILS}/${SPOT_CHECKED} spot-checked pod(s) missing the sealed state env"
fi

#------------------------------------------------------------------------------
# Verdict
#------------------------------------------------------------------------------

if [[ ${FAILURES} -gt 0 ]]; then
    step_fail "${FAILURES} convergence check(s) failed — wake/migrate the stragglers and re-run this gate"
fi
step_ok "11 (./11-deploy-r3-cleanup.sh)"
