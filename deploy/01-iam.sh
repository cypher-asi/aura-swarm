#!/bin/bash
# 01-iam.sh - ALL deploy IAM, consolidated under org-admin (separation of duties).
#
# This is the ONLY stage run by org-admin; every other stage is ops-admin. It
# provisions, idempotently:
#   1. Two customer-managed policies attached to two Identity Center permission
#      sets — org-admin (this IAM stage) and ops-admin (everything else; no
#      IAM-write, just a scoped iam:PassRole + iam:Get*/List* on the cluster/
#      node role ARNs).
#   2. The EKS cluster + node service roles ops-admin's terraform READS (data
#      sources) and PASSES. Static service trust, so created BEFORE the cluster.
#   3. (cluster-aware) The IRSA OIDC provider + the Peer Pods / CAA IRSA role
#      and its inline pod-VM lifecycle policy (incl. ec2:Describe*). The CAA
#      trust references the cluster OIDC issuer, which only exists after the
#      cluster is created, so this is DEFERRED until ./02-snp-node-group.sh has
#      run — re-run this script (org-admin) after step 02 to provision it.
#
# Run with an Identity Center admin session (management or delegated-admin).
# Verification is best-effort and non-fatal.
#
# Usage: ./01-iam.sh
#   Pass 1 (before step 02): provisions policies + cluster/node roles; defers CAA.
#   Pass 2 (after  step 02): also provisions the OIDC provider + CAA IRSA role.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "01" "All deploy IAM (permission sets, cluster/node roles, IRSA)" "org-admin"

require_cmds aws jq openssl
require_aws_auth

echo -e "${CYAN}org-admin policy / permission set${NC}  ${ORG_IAM_POLICY} → ${ORG_SSO_PERMISSION_SET}"
echo -e "${CYAN}ops-admin policy / permission set${NC}  ${OPS_IAM_POLICY} → ${OPS_SSO_PERMISSION_SET}"
echo ""

#------------------------------------------------------------------------------
# 1. Customer-managed policies + permission-set attachments (both users).
#------------------------------------------------------------------------------
echo -e "${CYAN}== Permission sets ==${NC}"
ensure_iam_policy "${ORG_IAM_POLICY}" "${DEPLOY_DIR}/iam/org-admin-policy.json"
ensure_iam_policy "${OPS_IAM_POLICY}" "${DEPLOY_DIR}/iam/ops-admin-policy.json"
echo ""
ensure_policy_on_permission_set "${ORG_IAM_POLICY}" "${ORG_SSO_PERMISSION_SET}"
echo ""
ensure_policy_on_permission_set "${OPS_IAM_POLICY}" "${OPS_SSO_PERMISSION_SET}"
echo ""

#------------------------------------------------------------------------------
# 2. EKS cluster + node service roles (pre-cluster; terraform reads/passes them).
#------------------------------------------------------------------------------
echo -e "${CYAN}== Cluster service roles ==${NC}"
ensure_cluster_service_roles
echo ""

#------------------------------------------------------------------------------
# 3. IRSA OIDC provider + CAA role (cluster-aware; deferred pre-cluster).
#------------------------------------------------------------------------------
echo -e "${CYAN}== IRSA OIDC provider + CAA role ==${NC}"
CAA_DEFERRED=0
ensure_oidc_provider_and_caa_role || CAA_DEFERRED=$?
echo ""

#------------------------------------------------------------------------------
# Best-effort verification of the ops-admin permission set's terraform polling.
#------------------------------------------------------------------------------
echo -e "${CYAN}== Verify ==${NC}"
verify_sso_role_action "${OPS_IAM_SSO_ROLE_PREFIX}" eks:DescribeUpdate eks:ListUpdates
echo ""

if [[ "${CAA_DEFERRED}" -eq 2 ]]; then
    echo -e "${YELLOW}ℹ${NC} CAA IRSA role deferred — the cluster does not exist yet."
    step_ok "02 (ops-admin: ./02-snp-node-group.sh), then RE-RUN ./01-iam.sh (org-admin) to provision the CAA role"
else
    echo -e "${GREEN}✓${NC} CAA IRSA role provisioned — ops-admin can proceed to the CAA install."
    step_ok "03 (ops-admin: ./03-coco-operator.sh)"
fi
