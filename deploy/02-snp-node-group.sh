#!/bin/bash
# 02-snp-node-group.sh - Terraform plan/apply for the worker node group + Peer
# Pods / CAA security-group rules, with the EFS-encryption replacement hazard
# guarded explicitly.
#
# Confidential agents run as Peer Pods (per-agent AWS-managed SEV-SNP pod VMs
# launched off-cluster by CAA), so there is NO on-node SEV-SNP metal pool: the
# ordinary workers host the kata-remote shim + agent-protocol-forwarder, and
# terraform provisions the worker<->pod-VM security-group rules. ALL IAM (the
# cluster/node service roles terraform reads + passes, the IRSA OIDC provider,
# and the CAA IRSA role) is owned by org-admin's ./01-iam.sh — this step has no
# IAM-write permission and only reads/passes the pre-created roles.
#
# CRITICAL GUARD: the storage module now hardcodes encrypted=true on the EFS
# filesystem. If the live filesystem is UNENCRYPTED, terraform will plan a
# REPLACEMENT (delete + create) — destroying all agent state. This script
# detects that and ABORTS, pointing at ./efs-encryption-migration.sh.
#
# Verifies: ordinary workers Ready and the worker security-group is applied.
#
# Usage: ./02-snp-node-group.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "02" "Workers + Peer Pods/CAA infra (terraform)" "ops-admin"

require_cmds aws terraform kubectl jq
require_aws_auth

#------------------------------------------------------------------------------
# Pod-VM AMI for the CAA-launched SEV-SNP pod VMs (consumed by step 03). Auto-
# discover + persist it now so the rest of the flow needs no manual configure
# step. Pin a specific image with: ./configure.sh PODVM_AMI_ID=ami-XXXX
#------------------------------------------------------------------------------
log_section "Pod-VM AMI"
ensure_podvm_ami

cd "${SCRIPT_DIR}/terraform"

#------------------------------------------------------------------------------
# tfvars + init (idempotent)
#------------------------------------------------------------------------------

log_section "Terraform init"
regenerate_tfvars
log_cmd "terraform init -upgrade -input=false"
terraform init -upgrade -input=false >/dev/null
log_ok "Terraform initialized"

#------------------------------------------------------------------------------
# Preflight: the node group must be ACTIVE before we plan/apply. EKS rejects
# UpdateNodegroupVersion with a 409 ResourceInUseException ("Nodegroup cannot be
# updated as it is currently not in Active State") if an earlier update (an AMI
# rollout from a prior ./02 run, or a console/CLI change) is still in flight.
# Check the live state FIRST so we never build a plan we cannot apply: wait out
# a transient CREATE/UPDATE, and abort on a stuck state.
#------------------------------------------------------------------------------
log_section "Node group preflight"
NG_NAME="${RESOURCE_PREFIX}-node-group"
NG_STATUS="$(nodegroup_status "${NG_NAME}")"
case "${NG_STATUS}" in
    MISSING)
        log_info "Node group ${NG_NAME} does not exist yet — plan will create it."
        ;;
    ACTIVE)
        log_ok "Node group ${NG_NAME} is ACTIVE."
        ;;
    CREATING|UPDATING)
        log_warn "Node group ${NG_NAME} is ${NG_STATUS} — an operation is still in flight."
        log_detail "Applying now would fail with a 409 (ResourceInUseException). Waiting for it"
        log_detail "to reach ACTIVE before planning (timeout ${NODE_JOIN_TIMEOUT_SECS:-1800}s)..."
        if ! wait_nodegroups_active "${NODE_JOIN_TIMEOUT_SECS:-1800}"; then
            step_fail "node group ${NG_NAME} did not reach ACTIVE within ${NODE_JOIN_TIMEOUT_SECS:-1800}s; re-run ./02 once it settles (check: aws eks describe-nodegroup --cluster-name ${EKS_CLUSTER_NAME} --nodegroup-name ${NG_NAME} --region ${AWS_REGION})"
        fi
        log_ok "Node group ${NG_NAME} reached ACTIVE."
        ;;
    *)
        # DEGRADED / DELETING / etc. — not safe to update and won't self-heal by waiting.
        log_abort "ABORT: node group ${NG_NAME} is ${NG_STATUS}, not ACTIVE"
        log_detail "EKS rejects any node-group update (409 ResourceInUseException) while the"
        log_detail "group is not ACTIVE. Investigate the health issues below before re-running:"
        echo ""
        aws eks describe-nodegroup --cluster-name "${EKS_CLUSTER_NAME}" \
            --nodegroup-name "${NG_NAME}" --region "${AWS_REGION}" \
            --query 'nodegroup.health.issues' --output table 2>/dev/null | indent || true
        echo ""
        step_fail "node group ${NG_NAME} is ${NG_STATUS}; resolve its health issues (above) until it returns to ACTIVE, then re-run ./02"
        ;;
esac

#------------------------------------------------------------------------------
# Plan, with the EFS replacement hazard check BEFORE anything is applied
#------------------------------------------------------------------------------

log_section "Plan"
PLAN_FILE="snp-node-group.tfplan"

log_cmd "terraform plan -out=${PLAN_FILE}"
log_detail "(full root module; SNP node group is the expected delta)"
terraform plan -out="${PLAN_FILE}"

if plan_replaces_efs "${PLAN_FILE}"; then
    log_abort "ABORT: this plan REPLACES the EFS filesystem"
    log_detail "The storage module now mandates encrypted=true, but the live EFS"
    log_detail "filesystem is unencrypted. Encryption cannot be enabled in place, so"
    log_detail "terraform wants to delete and recreate it — which would DESTROY ALL"
    log_detail "AGENT STATE on the shared filesystem."
    echo ""
    log_detail "Do NOT apply this plan. Instead run the guided migration:"
    log_cmd "./efs-encryption-migration.sh"
    log_detail "which creates a new encrypted filesystem, copies the data, repoints"
    log_detail "terraform state, and verifies — then re-run this script (the plan"
    log_detail "will no longer touch the EFS filesystem)."
    step_fail "terraform plan would replace the EFS filesystem (unencrypted -> encrypted); run ./efs-encryption-migration.sh first"
fi
log_ok "EFS hazard check passed (plan does not replace the filesystem)"

#------------------------------------------------------------------------------
# IAM-left-terraform guard. All IAM now lives in org-admin's ./01-iam.sh, but
# clusters provisioned before that refactor still TRACK the cluster/node/CAA
# roles + OIDC provider in terraform state. With those resources gone from the
# config, terraform plans to DESTROY them — which would delete live IAM the
# cluster depends on. Abort and tell the operator to `terraform state rm` them
# (state-only; no AWS deletion) before applying.
#------------------------------------------------------------------------------
if plan_destroys_external_iam "${PLAN_FILE}"; then
    log_abort "ABORT: this plan would DESTROY IAM now owned by ./01-iam.sh"
    log_detail "IAM (cluster/node/CAA roles + the IRSA OIDC provider) moved out of"
    log_detail "terraform into ./01-iam.sh (org-admin). terraform state still tracks the"
    log_detail "old resources, so applying would DELETE the live roles/provider the"
    log_detail "cluster (and the CAA daemonset) depend on."
    echo ""
    log_detail "Make terraform FORGET them (state-only — nothing is deleted in AWS), then"
    log_detail "re-run this script:"
    echo ""
    log_cmd "cd \"${SCRIPT_DIR}/terraform\""
    while IFS= read -r addr; do
        [[ -n "${addr}" ]] && log_cmd "terraform state rm '${addr}'"
    done < <(plan_external_iam_addresses "${PLAN_FILE}")
    echo ""
    step_fail "terraform plan would destroy IAM now managed by ./01-iam.sh; 'terraform state rm' those resources first (commands above)"
fi
log_ok "IAM hazard check passed (plan does not destroy ./01-iam.sh-owned roles/OIDC)"

#------------------------------------------------------------------------------
# eks:DescribeUpdate heads-up. terraform polls this action while waiting on a
# node-group update; if it is denied (identity policy or SCP), the apply below
# transparently falls back to DescribeNodegroup convergence.
#------------------------------------------------------------------------------
if plan_changes_nodegroup "${PLAN_FILE}"; then
    case "$(eks_describe_update_access)" in
        denied)
            log_warn "eks:DescribeUpdate is currently DENIED for this principal."
            log_detail "terraform's post-update wait will fail; the apply will fall back to"
            log_detail "direct node-group polling so the rollout still converges."
            log_detail "Permanent fix: attach ${OPS_IAM_POLICY} to the ${OPS_SSO_PERMISSION_SET} permission set"
            log_detail "(./01-iam.sh as an Identity Center admin) and clear any SCP denying eks:DescribeUpdate."
            ;;
        allowed)
            log_ok "eks:DescribeUpdate is allowed — terraform can poll node-group updates normally."
            ;;
        *)
            log_warn "Could not determine eks:DescribeUpdate access; will fall back to direct polling if needed."
            ;;
    esac
fi

confirm_plan "${PLAN_FILE}"

log_section "Apply"
apply_with_describeupdate_fallback "${PLAN_FILE}" "${NODE_JOIN_TIMEOUT_SECS:-1800}"

#------------------------------------------------------------------------------
# Verify: ordinary workers Ready (they host the kata-remote shim +
# agent-protocol-forwarder) and the worker security-group is applied.
# Confidential workloads run in per-agent AWS-managed pod VMs, so there is no
# metal pool. The CAA IRSA role is org-admin's (./01-iam.sh), not a tf output.
#------------------------------------------------------------------------------

log_section "Verify"
ensure_kubectl_context

WORKERS_READY=$(kubectl get nodes -o json 2>/dev/null \
    | jq '[.items[] | select(.status.conditions[]? | select(.type=="Ready" and .status=="True"))] | length')
if [[ "${WORKERS_READY:-0}" -lt 1 ]]; then
    step_fail "no worker nodes are Ready; Peer Pods needs healthy workers to host the kata-remote shim + agent-protocol-forwarder"
fi
log_ok "${WORKERS_READY} worker node(s) Ready"

NODE_SG_ID="$(tf_output node_security_group_id)"
if [[ -z "${NODE_SG_ID}" ]]; then
    step_fail "terraform output node_security_group_id is empty — the worker SG (incl. CAA worker<->pod-VM rules) was not applied"
fi
log_ok "Worker security-group applied (${NODE_SG_ID})"
log_info "No SEV-SNP metal pool — confidential agents run as per-agent AWS-managed pod VMs (Peer Pods)."
log_info "Next: re-run ./01-iam.sh (org-admin) to provision the CAA IRSA role now that the cluster exists."

step_ok "01 re-run (org-admin: ./01-iam.sh — CAA IRSA role), then 03 (./03-coco-operator.sh)"
