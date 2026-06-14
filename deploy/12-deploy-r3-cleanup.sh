#!/bin/bash
# 12-deploy-r3-cleanup.sh - Deploy the R3 (cleanup) builds, retire the legacy
# kata-fc runtime class, and shrink the old node group into the system pool.
#
# Refuses to run until the R2 convergence gate (step 10) passes.
#
# Verifies: no kata-fc artifacts remain (RuntimeClass or pods), fleet healthy.
#
# Usage:
#   ./12-deploy-r3-cleanup.sh                  # deploys SWARM_R3_REF (default master)
#   ./12-deploy-r3-cleanup.sh --ref <git-ref>
#
# System pool sizing comes from NODE_DESIRED_COUNT / NODE_MIN_COUNT /
# NODE_MAX_COUNT in config.env (override via env to shrink further).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

REF="${SWARM_R3_REF:-${R3_DEFAULT_REF}}"
while [[ $# -gt 0 ]]; do
    case "$1" in
        --ref) REF="$2"; shift 2 ;;
        *) echo "Unknown option: $1"; echo "Usage: $0 [--ref GIT_REF]"; exit 1 ;;
    esac
done

step_banner "12" "Deploy R3 (cleanup) at ref ${REF}" "ops-admin"

require_cmds aws kubectl jq curl docker git terraform openssl
require_aws_auth
ensure_kubectl_context
trap gw_stop_port_forward EXIT

#------------------------------------------------------------------------------
# Gate: R2 must have converged (no kata-fc pods, schema v2)
#------------------------------------------------------------------------------

KATA_FC=$(count_pods_on_runtime_class "kata-fc")
[[ "${KATA_FC}" == "0" ]] \
    || step_fail "${KATA_FC} pod(s) still on kata-fc — run ./11-r2-convergence.sh until it passes before R3"

gw_start_port_forward || step_fail "gateway port-forward failed"
SCHEMA=$(gw_schema_version || echo "null")
gw_stop_port_forward
[[ "${SCHEMA}" == "2" ]] \
    || step_fail "schema_version=${SCHEMA}, expected 2 — R2 has not completed"
log_ok "R2 convergence preconditions hold (schema v2, zero kata-fc pods)"
echo ""

#------------------------------------------------------------------------------
# Deploy R3 builds
#------------------------------------------------------------------------------

deploy_platform_at_ref "${REF}"

gw_start_port_forward || step_fail "gateway port-forward failed after deploy"
HEALTH_STATUS=$(gw_internal_get "/internal/health" | jq -r '.status // empty')
gw_stop_port_forward
[[ "${HEALTH_STATUS}" == "ok" ]] || step_fail "gateway /internal/health not ok after the R3 deploy"
log_ok "R3 platform deployed and healthy"
echo ""

#------------------------------------------------------------------------------
# Retire kata-fc
#------------------------------------------------------------------------------

log_cmd "kubectl delete runtimeclass kata-fc --ignore-not-found"
kubectl delete runtimeclass kata-fc --ignore-not-found
if kubectl get runtimeclass kata-fc >/dev/null 2>&1; then
    step_fail "kata-fc RuntimeClass still exists after delete"
fi
log_ok "kata-fc RuntimeClass gone"
echo ""

#------------------------------------------------------------------------------
# Terraform: shrink the old node group into the system pool
#------------------------------------------------------------------------------

cd "${SCRIPT_DIR}/terraform"
regenerate_tfvars
terraform init -upgrade -input=false >/dev/null

PLAN_FILE="r3-system-pool.tfplan"
log_info "Planning the shrunk system pool (${NODE_DESIRED_COUNT} x ${NODE_INSTANCE_TYPE})..."
terraform plan -out="${PLAN_FILE}"

if plan_replaces_efs "${PLAN_FILE}"; then
    step_fail "plan would replace the EFS filesystem — investigate before applying (see ./efs-encryption-migration.sh)"
fi

confirm_plan "${PLAN_FILE}"
terraform apply "${PLAN_FILE}"
log_ok "System pool applied"
cd "${SCRIPT_DIR}"
echo ""

#------------------------------------------------------------------------------
# Verify: no kata-fc artifacts, fleet healthy
#------------------------------------------------------------------------------

KATA_FC=$(count_pods_on_runtime_class "kata-fc")
[[ "${KATA_FC}" == "0" ]] || step_fail "${KATA_FC} pod(s) reappeared on kata-fc"
if kubectl get runtimeclass kata-fc >/dev/null 2>&1; then
    step_fail "kata-fc RuntimeClass reappeared"
fi
log_ok "No kata-fc artifacts remain"

# Fleet health: platform rollouts already verified; check agent pods settle.
if ! wait_for_pods_condition '[.[] | select(.phase != "Running" or (.ready | not))] | length == 0' 600 \
    "every agent pod Running and Ready"; then
    kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent -o wide | indent
    step_fail "agent pods did not settle to Running/Ready within 600s"
fi

print_fleet_report

step_ok "13 (./13-finalize.sh)"
