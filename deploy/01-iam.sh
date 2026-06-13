#!/bin/bash
# 01-iam.sh - Deploy-operator IAM for an IAM Identity Center (SSO) org.
#
# Creates/updates the customer-managed deploy policy and attaches it to the
# Identity Center PERMISSION SET (AWSReservedSSO_* roles cannot be modified
# directly, and IAM groups only apply to IAM users). Re-provisions the
# permission set so the change reaches the assigned account(s).
#
# Does NOT touch cluster service roles (EKS cluster/node IRSA, EFS CSI, etc.) —
# those stay in the terraform modules applied by later steps.
#
# Run the policy/permission-set wiring with an Identity Center admin session
# (management or delegated-admin). Verification is best-effort and non-fatal.
#
# Usage: ./01-iam.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "01" "Deploy-operator IAM (SSO permission set)"

require_cmds aws jq
require_aws_auth

echo -e "${CYAN}Policy${NC}          ${DEPLOY_IAM_POLICY}"
echo -e "${CYAN}Permission set${NC}  ${DEPLOY_SSO_PERMISSION_SET}"
echo ""

ensure_deploy_iam_policy
echo ""
ensure_deploy_policy_on_permission_set
echo ""
verify_deploy_iam_permissions

step_ok "02 (./02-snp-node-group.sh)"
