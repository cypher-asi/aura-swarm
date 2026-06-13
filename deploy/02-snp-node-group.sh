#!/bin/bash
# 02-snp-node-group.sh - Terraform plan/apply for the worker node group + Peer
# Pods / CAA infra (IAM/SG), with the EFS-encryption replacement hazard guarded
# explicitly.
#
# Confidential agents run as Peer Pods (per-agent AWS-managed SEV-SNP pod VMs
# launched off-cluster by CAA), so there is NO on-node SEV-SNP metal pool: the
# ordinary workers host the kata-remote shim + agent-protocol-forwarder, and
# terraform provisions the CAA IRSA role + worker<->pod-VM security-group rules.
#
# CRITICAL GUARD: the storage module now hardcodes encrypted=true on the EFS
# filesystem. If the live filesystem is UNENCRYPTED, terraform will plan a
# REPLACEMENT (delete + create) — destroying all agent state. This script
# detects that and ABORTS, pointing at ./efs-encryption-migration.sh.
#
# Verifies: ordinary workers Ready and the CAA IAM/SG infra is applied.
#
# Usage: ./02-snp-node-group.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "02" "Workers + Peer Pods/CAA infra (terraform)"

require_cmds aws terraform kubectl jq
require_aws_auth

#------------------------------------------------------------------------------
# Pod-VM AMI for the CAA-launched SEV-SNP pod VMs (consumed by step 03). Auto-
# discover + persist it now so the rest of the flow needs no manual configure
# step. Pin a specific image with: ./configure.sh PODVM_AMI_ID=ami-XXXX
#------------------------------------------------------------------------------
ensure_podvm_ami
echo ""

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
    echo "    ./efs-encryption-migration.sh"
    echo ""
    echo "which creates a new encrypted filesystem, copies the data, repoints"
    echo "terraform state, and verifies — then re-run this script (the plan"
    echo "will no longer touch the EFS filesystem)."
    step_fail "terraform plan would replace the EFS filesystem (unencrypted -> encrypted); run ./efs-encryption-migration.sh first"
fi
echo -e "${GREEN}✓${NC} EFS hazard check passed (plan does not replace the filesystem)"

#------------------------------------------------------------------------------
# eks:DescribeUpdate heads-up. terraform polls this action while waiting on a
# node-group update; if it is denied (identity policy or SCP), the apply below
# transparently falls back to DescribeNodegroup convergence.
#------------------------------------------------------------------------------
if plan_changes_nodegroup "${PLAN_FILE}"; then
    case "$(eks_describe_update_access)" in
        denied)
            echo -e "${YELLOW}⚠${NC} eks:DescribeUpdate is currently DENIED for this principal."
            echo "  terraform's post-update wait will fail; the apply will fall back to"
            echo "  direct node-group polling so the rollout still converges."
            echo "  Permanent fix: attach ${DEPLOY_IAM_POLICY} to the ${DEPLOY_SSO_PERMISSION_SET} permission set"
            echo "  (./01-iam.sh as an Identity Center admin) and clear any SCP denying eks:DescribeUpdate."
            ;;
        allowed)
            echo -e "${GREEN}✓${NC} eks:DescribeUpdate is allowed — terraform can poll node-group updates normally."
            ;;
        *)
            echo -e "${YELLOW}⚠${NC} Could not determine eks:DescribeUpdate access; will fall back to direct polling if needed."
            ;;
    esac
    echo ""
fi

confirm_plan "${PLAN_FILE}"

echo ""
echo "Applying..."
apply_with_describeupdate_fallback "${PLAN_FILE}" "${NODE_JOIN_TIMEOUT_SECS:-1800}"
echo ""

#------------------------------------------------------------------------------
# Verify: ordinary workers Ready (they host the kata-remote shim +
# agent-protocol-forwarder) and the CAA IAM/SG infra is applied. Confidential
# workloads run in per-agent AWS-managed pod VMs, so there is no metal pool.
#------------------------------------------------------------------------------

ensure_kubectl_context

WORKERS_READY=$(kubectl get nodes -o json 2>/dev/null \
    | jq '[.items[] | select(.status.conditions[]? | select(.type=="Ready" and .status=="True"))] | length')
if [[ "${WORKERS_READY:-0}" -lt 1 ]]; then
    step_fail "no worker nodes are Ready; Peer Pods needs healthy workers to host the kata-remote shim + agent-protocol-forwarder"
fi
echo -e "${GREEN}✓${NC} ${WORKERS_READY} worker node(s) Ready"

CAA_ROLE_ARN="$(tf_output caa_role_arn)"
if [[ -z "${CAA_ROLE_ARN}" ]]; then
    step_fail "terraform output caa_role_arn is empty — the CAA IRSA role was not applied (check the EKS module)"
fi
echo -e "${GREEN}✓${NC} CAA IRSA role applied (${CAA_ROLE_ARN})"
echo -e "${YELLOW}ℹ${NC} No SEV-SNP metal pool — confidential agents run as per-agent AWS-managed pod VMs (Peer Pods)."

step_ok "03 (./03-coco-operator.sh)"
