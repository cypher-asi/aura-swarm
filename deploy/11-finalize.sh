#!/bin/bash
# 11-finalize.sh - Final fleet health report + EFS backup retention handling.
#
# Prints the end-state report (nodes, runtime classes, schema version, KBS,
# zbilling) and reminds about the pre-R2 EFS recovery point. The recovery
# point is deleted ONLY on explicit confirmation, and only after the
# retention window (default 14 days) has elapsed.
#
# Usage:
#   ./11-finalize.sh
#   EFS_BACKUP_RETENTION_DAYS=30 ./11-finalize.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "11" "Finalize: fleet report + backup retention"

require_cmds aws kubectl jq curl
require_aws_auth
ensure_kubectl_context
trap gw_stop_port_forward EXIT

FAILURES=0
report() { # report <ok:0|1> <pass-msg> <fail-msg>
    if [[ "$1" -eq 0 ]]; then
        echo -e "${GREEN}✓${NC} $2"
    else
        echo -e "${RED}✗${NC} $3"
        FAILURES=$((FAILURES + 1))
    fi
}

#------------------------------------------------------------------------------
# Final fleet health report
#------------------------------------------------------------------------------

echo -e "${CYAN}Nodes${NC}"
kubectl get nodes -o custom-columns='NAME:.metadata.name,INSTANCE:.metadata.labels.node\.kubernetes\.io/instance-type,CONFIDENTIAL:.metadata.labels.swarm\.io/confidential-node' --no-headers | sed 's/^/  /'
echo ""

echo -e "${CYAN}Platform${NC}"
PLATFORM_OK=0
for d in aura-swarm-gateway aura-swarm-control aura-swarm-scheduler kbs; do
    READY=$(kubectl get deployment "${d}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo "0")
    if [[ "${READY:-0}" -ge 1 ]]; then
        echo -e "  ${GREEN}✓${NC} ${d}: ready"
    else
        echo -e "  ${RED}✗${NC} ${d}: not ready"
        PLATFORM_OK=1
    fi
done
report "${PLATFORM_OK}" "All platform deployments ready" "one or more platform deployments unhealthy"
echo ""

print_fleet_report
echo ""

# Hard end-state assertions
KATA_FC=$(count_pods_on_runtime_class "kata-fc")
[[ "${KATA_FC}" == "0" ]] && RES=0 || RES=1
report "${RES}" "Zero kata-fc pods" "${KATA_FC} pod(s) on kata-fc"

kubectl get runtimeclass kata-fc >/dev/null 2>&1 && RES=1 || RES=0
report "${RES}" "kata-fc RuntimeClass retired" "kata-fc RuntimeClass still exists"

gw_start_port_forward || step_fail "gateway port-forward failed"
SCHEMA=$(gw_schema_version || echo "null")
gw_stop_port_forward
[[ "${SCHEMA}" == "2" ]] && RES=0 || RES=1
report "${RES}" "Store schema_version=2" "schema_version=${SCHEMA}"

kbs_in_cluster_check >/dev/null 2>&1 && RES=0 || RES=1
report "${RES}" "KBS responding in-cluster" "KBS not responding in-cluster"

Z_BILLING_URL="${Z_BILLING_URL:-https://z-billing.onrender.com}"
ZB_CODE=$(curl -s -o /dev/null -w '%{http_code}' --connect-timeout 5 --max-time 10 \
    "${Z_BILLING_URL}/health" 2>/dev/null || echo "000")
[[ "${ZB_CODE}" == "200" ]] && RES=0 || RES=1
report "${RES}" "zbilling reachable (${Z_BILLING_URL})" "zbilling returned HTTP ${ZB_CODE}"
echo ""

#------------------------------------------------------------------------------
# EFS backup retention (prompt, never auto-delete)
#------------------------------------------------------------------------------

echo -e "${CYAN}Pre-R2 EFS recovery point${NC}"
RETENTION_DAYS="${EFS_BACKUP_RETENTION_DAYS:-14}"

if [[ ! -f "${EFS_BACKUP_STATE_FILE}" ]]; then
    echo "  No recorded recovery point (${EFS_BACKUP_STATE_FILE} missing) — nothing to manage."
else
    # shellcheck disable=SC1090
    source <(tr -d '\r' < "${EFS_BACKUP_STATE_FILE}")
    echo "  Recovery point: ${EFS_BACKUP_RECOVERY_POINT_ARN}"
    echo "  Taken at:       ${EFS_BACKUP_TAKEN_AT} (retention window: ${RETENTION_DAYS} days)"

    TAKEN_EPOCH=$(date -d "${EFS_BACKUP_TAKEN_AT}" +%s 2>/dev/null || date -j -f "%Y-%m-%dT%H:%M:%SZ" "${EFS_BACKUP_TAKEN_AT}" +%s 2>/dev/null || echo 0)
    NOW_EPOCH=$(date +%s)
    AGE_DAYS=$(( (NOW_EPOCH - TAKEN_EPOCH) / 86400 ))
    echo "  Age:            ${AGE_DAYS} day(s)"

    if [[ ${AGE_DAYS} -lt ${RETENTION_DAYS} ]]; then
        echo ""
        echo -e "  ${YELLOW}Still inside the retention window — keeping the recovery point.${NC}"
        echo "  Re-run this script after $(( RETENTION_DAYS - AGE_DAYS )) more day(s), or delete manually with:"
        echo "    aws backup delete-recovery-point --backup-vault-name ${EFS_BACKUP_VAULT} \\"
        echo "        --recovery-point-arn ${EFS_BACKUP_RECOVERY_POINT_ARN}"
    else
        echo ""
        echo -e "  ${YELLOW}The retention window has elapsed.${NC} Delete the pre-R2 recovery point?"
        read -r -p "  Type 'delete' to delete it now, anything else to keep it: " answer
        if [[ "${answer}" == "delete" ]]; then
            aws backup delete-recovery-point \
                --backup-vault-name "${EFS_BACKUP_VAULT}" \
                --recovery-point-arn "${EFS_BACKUP_RECOVERY_POINT_ARN}"
            rm -f "${EFS_BACKUP_STATE_FILE}"
            echo -e "  ${GREEN}✓${NC} Recovery point deleted"
        else
            echo "  Kept. Re-run ./11-finalize.sh whenever you want to revisit."
        fi
    fi
fi
echo ""

#------------------------------------------------------------------------------
# Verdict
#------------------------------------------------------------------------------

if [[ ${FAILURES} -gt 0 ]]; then
    step_fail "${FAILURES} final health check(s) failed"
fi
step_ok "— rollout complete. Run ./06-r1-soak-check.sh any time for a fleet spot-check."
