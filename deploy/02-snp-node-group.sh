#!/bin/bash
# 02-snp-node-group.sh - Terraform plan/apply for the SNP bare-metal node
# group (and any related infra drift), with the EFS-encryption replacement
# hazard guarded explicitly.
#
# CRITICAL GUARD: the storage module now hardcodes encrypted=true on the EFS
# filesystem. If the live filesystem is UNENCRYPTED, terraform will plan a
# REPLACEMENT (delete + create) — destroying all agent state. This script
# detects that and ABORTS, pointing at ./02b-efs-encryption-migration.sh.
#
# Verifies (CONFIDENTIAL_RUNTIME=snp_local, default): SNP nodes joined, labeled
# swarm.io/confidential-node=true, tainted.
# Verifies (CONFIDENTIAL_RUNTIME=peer_pods): NO confidential metal pool present
# (or scaled to 0) and ordinary workers Ready — peer_pods runs confidential
# workloads in per-agent AWS-managed pod VMs, so no SEV-SNP metal node is needed.
# The EFS-replacement hazard guard and eks:DescribeUpdate fallback run in BOTH
# modes (they protect the shared filesystem / let the apply converge).
#
# Usage: ./02-snp-node-group.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "02" "SNP node group (terraform)"

# snp_local = on-node kata-qemu-snp metal pool (default/fallback);
# peer_pods = CAA kata-remote, confidential workloads run in AWS pod VMs (no metal pool).
CONFIDENTIAL_RUNTIME="${CONFIDENTIAL_RUNTIME:-snp_local}"

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
    echo "    ./02b-efs-encryption-migration.sh"
    echo ""
    echo "which creates a new encrypted filesystem, copies the data, repoints"
    echo "terraform state, and verifies — then re-run this script (the plan"
    echo "will no longer touch the EFS filesystem)."
    step_fail "terraform plan would replace the EFS filesystem (unencrypted -> encrypted); run ./02b-efs-encryption-migration.sh first"
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
echo "Applying (bare-metal node provisioning can take 15-25 minutes)..."
apply_with_describeupdate_fallback "${PLAN_FILE}" "${SNP_NODE_JOIN_TIMEOUT_SECS:-1800}"
echo ""

#------------------------------------------------------------------------------
# Verify (mode-dependent):
#   snp_local  -> SNP nodes joined, labeled, tainted
#   peer_pods  -> no confidential metal pool (or scaled to 0) + workers Ready
#------------------------------------------------------------------------------

ensure_kubectl_context

if [[ "${CONFIDENTIAL_RUNTIME}" == "peer_pods" ]]; then
    # peer_pods runs confidential workloads in AWS-managed pod VMs via kata-remote,
    # so the SEV-SNP metal node group must be absent or scaled to 0; ordinary
    # workers only host the kata shim + agent-protocol-forwarder.
    echo "CONFIDENTIAL_RUNTIME=peer_pods — verifying the confidential metal pool is gone (or 0) and workers are Ready..."

    CONF_NG="${RESOURCE_PREFIX}-confidential-node-group"
    NG_STATUS="$(nodegroup_status "${CONF_NG}")"
    CONF_DESIRED="${CONFIDENTIAL_NODE_DESIRED_COUNT:-0}"
    SNP_NODES=$(kubectl get nodes -l swarm.io/confidential-node=true --no-headers 2>/dev/null | wc -l | tr -d ' ')

    if [[ "${NG_STATUS}" != "MISSING" && "${CONF_DESIRED}" != "0" ]]; then
        step_fail "peer_pods needs no metal pool, but '${CONF_NG}' exists with CONFIDENTIAL_NODE_DESIRED_COUNT=${CONF_DESIRED}; set it to 0 (or remove the node group) and re-run"
    fi
    if [[ "${SNP_NODES}" -ne 0 ]]; then
        step_fail "peer_pods needs no metal pool, but ${SNP_NODES} confidential metal node(s) are still registered (swarm.io/confidential-node=true); scale CONFIDENTIAL_NODE_* to 0 and let them drain"
    fi
    if [[ "${NG_STATUS}" == "MISSING" ]]; then
        echo -e "${GREEN}✓${NC} No confidential metal node group present (${CONF_NG})"
    else
        echo -e "${GREEN}✓${NC} Confidential metal node group present but scaled to 0 (no SNP nodes registered)"
    fi

    WORKERS_READY=$(kubectl get nodes -l '!swarm.io/confidential-node' -o json 2>/dev/null \
        | jq '[.items[] | select(.status.conditions[]? | select(.type=="Ready" and .status=="True"))] | length')
    if [[ "${WORKERS_READY:-0}" -lt 1 ]]; then
        step_fail "no ordinary worker nodes are Ready; peer_pods needs healthy workers to host the kata-remote shim + agent-protocol-forwarder"
    fi
    echo -e "${GREEN}✓${NC} ${WORKERS_READY} ordinary worker node(s) Ready"
    echo -e "${YELLOW}ℹ${NC} Peer Pods needs no SEV-SNP metal pool — confidential VMs run as per-agent AWS-managed pod VMs."
else
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
fi

step_ok "03 (./03-coco-operator.sh)"
