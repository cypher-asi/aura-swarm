#!/bin/bash
# 01-snp-node-group.sh - Terraform plan/apply for the SNP bare-metal node
# group (and any related infra drift), with the EFS-encryption replacement
# hazard guarded explicitly.
#
# CRITICAL GUARD: the storage module now hardcodes encrypted=true on the EFS
# filesystem. If the live filesystem is UNENCRYPTED, terraform will plan a
# REPLACEMENT (delete + create) — destroying all agent state. This script
# detects that and ABORTS, pointing at ./01b-efs-encryption-migration.sh.
#
# Verifies: SNP nodes joined, labeled swarm.io/confidential-node=true, tainted.
#
# Usage: ./01-snp-node-group.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "01" "SNP node group (terraform)"

require_cmds aws terraform kubectl jq
require_aws_auth

cd "${SCRIPT_DIR}/terraform"

#------------------------------------------------------------------------------
# tfvars + init (idempotent)
#------------------------------------------------------------------------------

regenerate_tfvars
terraform init -upgrade -input=false >/dev/null
echo -e "${GREEN}✓${NC} Terraform initialized"
echo ""

#------------------------------------------------------------------------------
# Plan, with the EFS replacement hazard check BEFORE anything is applied
#------------------------------------------------------------------------------

PLAN_FILE="snp-node-group.tfplan"

echo "Planning (full root module; SNP node group is the expected delta)..."
terraform plan -out="${PLAN_FILE}"

if plan_replaces_efs "${PLAN_FILE}"; then
    echo ""
    echo -e "${RED}================================================================${NC}"
    echo -e "${RED}  ABORT: this plan REPLACES the EFS filesystem${NC}"
    echo -e "${RED}================================================================${NC}"
    echo ""
    echo "The storage module now mandates encrypted=true, but the live EFS"
    echo "filesystem is unencrypted. Encryption cannot be enabled in place, so"
    echo "terraform wants to delete and recreate it — which would DESTROY ALL"
    echo "AGENT STATE on the shared filesystem."
    echo ""
    echo "Do NOT apply this plan. Instead run the guided migration:"
    echo ""
    echo "    ./01b-efs-encryption-migration.sh"
    echo ""
    echo "which creates a new encrypted filesystem, copies the data, repoints"
    echo "terraform state, and verifies — then re-run this script (the plan"
    echo "will no longer touch the EFS filesystem)."
    step_fail "terraform plan would replace the EFS filesystem (unencrypted -> encrypted); run ./01b-efs-encryption-migration.sh first"
fi
echo -e "${GREEN}✓${NC} EFS hazard check passed (plan does not replace the filesystem)"

confirm_plan "${PLAN_FILE}"

echo ""
echo "Applying (bare-metal node provisioning can take 15-25 minutes)..."
terraform apply "${PLAN_FILE}"
echo ""

#------------------------------------------------------------------------------
# Verify: SNP nodes joined, labeled, tainted
#------------------------------------------------------------------------------

ensure_kubectl_context

EXPECTED_SNP_NODES="${CONFIDENTIAL_NODE_DESIRED_COUNT}"
TIMEOUT="${SNP_NODE_JOIN_TIMEOUT_SECS:-1800}"
POLL=30
ELAPSED=0
SNP_READY=0

echo "Waiting for ${EXPECTED_SNP_NODES} SNP node(s) to join and become Ready (timeout ${TIMEOUT}s)..."
while [[ ${ELAPSED} -le ${TIMEOUT} ]]; do
    SNP_READY=$(kubectl get nodes -l swarm.io/confidential-node=true -o json 2>/dev/null \
        | jq '[.items[] | select(.status.conditions[] | select(.type=="Ready" and .status=="True"))] | length')
    if [[ "${SNP_READY}" -ge "${EXPECTED_SNP_NODES}" ]]; then
        break
    fi
    echo "  [${ELAPSED}s] SNP nodes Ready: ${SNP_READY}/${EXPECTED_SNP_NODES}"
    sleep "${POLL}"
    ELAPSED=$((ELAPSED + POLL))
done

if [[ "${SNP_READY}" -lt "${EXPECTED_SNP_NODES}" ]]; then
    step_fail "only ${SNP_READY}/${EXPECTED_SNP_NODES} SNP node(s) Ready after ${TIMEOUT}s"
fi
echo -e "${GREEN}✓${NC} ${SNP_READY} SNP node(s) Ready with label swarm.io/confidential-node=true"

UNTAINTED=$(kubectl get nodes -l swarm.io/confidential-node=true -o json \
    | jq '[.items[] | select([.spec.taints[]? | select(.key=="swarm.io/confidential-node" and .effect=="NoSchedule")] | length == 0)] | length')
if [[ "${UNTAINTED}" != "0" ]]; then
    step_fail "${UNTAINTED} SNP node(s) missing the swarm.io/confidential-node:NoSchedule taint"
fi
echo -e "${GREEN}✓${NC} All SNP nodes carry the swarm.io/confidential-node:NoSchedule taint"

step_ok "02 (./02-coco-operator.sh)"
