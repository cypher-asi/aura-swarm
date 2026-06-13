#!/bin/bash
# 10-deploy-r2-migrate.sh - Deploy the R2 (migration) ref and run the fleet
# migration:
#   1. refuse to run without the step-08 EFS rollback point
#   2. build + deploy gateway/control/scheduler at the R2 ref; the gateway
#      runs the v1 -> v2 store migration + KBS DEK backfill at startup
#   3. verify schema_version=2 via /internal/health
#   4. set MIGRATION_RECREATE_LEGACY_PODS=true and monitor the rolling
#      recreation (one legacy pod per ~30s reconciler pass) until no running
#      pod remains on kata-fc
#
# Hibernating/stopped agents migrate on their next wake/start — final
# convergence is gated by ./11-r2-convergence.sh.
#
# Usage:
#   ./10-deploy-r2-migrate.sh                  # deploys SWARM_R2_REF (default af9e034)
#   ./10-deploy-r2-migrate.sh --ref <git-ref>
#   ./10-deploy-r2-migrate.sh --skip-backup-check   # NOT recommended

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

REF="${SWARM_R2_REF:-${R2_DEFAULT_REF}}"
SKIP_BACKUP_CHECK=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --ref) REF="$2"; shift 2 ;;
        --skip-backup-check) SKIP_BACKUP_CHECK=true; shift ;;
        *) echo "Unknown option: $1"; echo "Usage: $0 [--ref GIT_REF] [--skip-backup-check]"; exit 1 ;;
    esac
done

step_banner "10" "Deploy R2 (migration) at ref ${REF}" "ops-admin"

require_cmds aws kubectl jq curl docker git openssl
require_aws_auth
ensure_kubectl_context
trap gw_stop_port_forward EXIT

#------------------------------------------------------------------------------
# Rollback point gate
#------------------------------------------------------------------------------

if [[ "${SKIP_BACKUP_CHECK}" != "true" ]]; then
    [[ -f "${EFS_BACKUP_STATE_FILE}" ]] \
        || step_fail "no EFS rollback point found (${EFS_BACKUP_STATE_FILE}) — run ./09-efs-backup.sh first"
    # shellcheck disable=SC1090
    source <(tr -d '\r' < "${EFS_BACKUP_STATE_FILE}")
    RP_STATUS=$(aws backup describe-recovery-point \
        --backup-vault-name "${EFS_BACKUP_VAULT}" \
        --recovery-point-arn "${EFS_BACKUP_RECOVERY_POINT_ARN}" \
        --query 'Status' --output text 2>/dev/null || echo "MISSING")
    [[ "${RP_STATUS}" == "COMPLETED" ]] \
        || step_fail "EFS rollback recovery point is ${RP_STATUS}, expected COMPLETED — re-run ./09-efs-backup.sh"
    echo -e "${GREEN}✓${NC} Rollback point verified: ${EFS_BACKUP_RECOVERY_POINT_ARN} (taken ${EFS_BACKUP_TAKEN_AT})"
    echo ""
fi

#------------------------------------------------------------------------------
# Deploy R2
#------------------------------------------------------------------------------

PRE_LEGACY=$(count_pods_on_runtime_class "kata-fc")
echo "Running legacy (kata-fc) pods before migration: ${PRE_LEGACY}"
echo ""

deploy_platform_at_ref "${REF}"
echo ""

#------------------------------------------------------------------------------
# Verify: store migration ran (schema_version=2) + DEK backfill
#------------------------------------------------------------------------------

echo "Waiting for the v1 -> v2 store migration (gateway startup)..."
gw_start_port_forward || step_fail "gateway port-forward failed after deploy"
SCHEMA=""
ELAPSED=0
while [[ ${ELAPSED} -le 300 ]]; do
    SCHEMA=$(gw_schema_version || echo "null")
    [[ "${SCHEMA}" == "2" ]] && break
    echo "  [${ELAPSED}s] schema_version=${SCHEMA}"
    sleep 10
    ELAPSED=$((ELAPSED + 10))
done
gw_stop_port_forward
[[ "${SCHEMA}" == "2" ]] \
    || step_fail "gateway reports schema_version=${SCHEMA}, expected 2 — check gateway logs: kubectl logs -n ${K8S_NAMESPACE_SYSTEM} deploy/aura-swarm-gateway"
echo -e "${GREEN}✓${NC} Store schema_version=2 (records migrated to tiered + sealed)"

GW_LOGS=$(kubectl logs -n "${K8S_NAMESPACE_SYSTEM}" deploy/aura-swarm-gateway --tail=500 2>/dev/null || echo "")
if echo "${GW_LOGS}" | grep -q "Store schema migration complete"; then
    echo -e "${GREEN}✓${NC} Gateway log: 'Store schema migration complete'"
else
    echo -e "${YELLOW}⚠${NC} Migration-complete log line not in the last 500 lines (schema_version=2 already confirms it ran)"
fi
if echo "${GW_LOGS}" | grep -q "DEK backfill pass complete"; then
    echo -e "${GREEN}✓${NC} Gateway log: 'DEK backfill pass complete'"
else
    echo -e "${YELLOW}⚠${NC} DEK backfill completion not in the last 500 lines; failures retry on the next gateway restart"
fi
echo ""

#------------------------------------------------------------------------------
# Enable + monitor the rolling pod migration
#------------------------------------------------------------------------------

echo "Enabling MIGRATION_RECREATE_LEGACY_PODS=true..."
kubectl patch configmap aura-swarm-config -n "${K8S_NAMESPACE_SYSTEM}" \
    --type merge -p '{"data":{"MIGRATION_RECREATE_LEGACY_PODS":"true"}}' >/dev/null
kubectl rollout restart deployment/aura-swarm-scheduler -n "${K8S_NAMESPACE_SYSTEM}" >/dev/null
kubectl rollout status deployment/aura-swarm-scheduler -n "${K8S_NAMESPACE_SYSTEM}" --timeout=300s >/dev/null \
    || step_fail "scheduler restart did not complete after enabling the migration gate"
echo -e "${GREEN}✓${NC} Scheduler restarted with the migration gate enabled"
echo ""

# One legacy pod is replaced per ~30s pass; budget generously.
TIMEOUT="${R2_MIGRATION_TIMEOUT_SECS:-$(( PRE_LEGACY * 120 + 600 ))}"
POLL=30
ELAPSED=0

echo "Monitoring the rolling recreation (timeout ${TIMEOUT}s)..."
LEGACY_LEFT="${PRE_LEGACY}"
while [[ ${ELAPSED} -le ${TIMEOUT} ]]; do
    LEGACY_LEFT=$(count_pods_on_runtime_class "kata-fc")
    SNP_NOW=$(count_pods_on_runtime_class "kata-remote")
    if [[ "${LEGACY_LEFT}" == "0" ]]; then
        break
    fi
    echo "  [${ELAPSED}s] pods remaining on kata-fc: ${LEGACY_LEFT}  (on kata-remote: ${SNP_NOW})"
    sleep "${POLL}"
    ELAPSED=$((ELAPSED + POLL))
done

if [[ "${LEGACY_LEFT}" != "0" ]]; then
    kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent -o wide | sed 's/^/  /' || true
    step_fail "${LEGACY_LEFT} running pod(s) still on kata-fc after ${TIMEOUT}s — this script is safe to re-run to keep monitoring"
fi
echo -e "${GREEN}✓${NC} No running pod remains on kata-fc"
echo ""
echo "Note: hibernating/stopped agents migrate on their next wake/start; the"
echo "convergence gate (step 10) checks records, not just running pods."

step_ok "11 (./11-r2-convergence.sh)"
