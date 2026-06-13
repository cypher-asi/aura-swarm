#!/bin/bash
# configure.sh - Scripted, idempotent updates to deploy/config.env.
#
# Persists deploy knobs into config.env (keeping the ${VAR:-default} override
# pattern) so every numbered step picks them up — no hand-editing between steps.
# This is the scripted entry point for switching the confidential runtime and
# the Peer Pods / Cloud API Adaptor (CAA) settings during the rollout.
#
# Usage:
#   ./configure.sh KEY=VALUE [KEY=VALUE ...]   # set one or more allowlisted knobs
#   ./configure.sh --enable-caa                # CAA infra on (auto-discovers AMI), runtime unchanged
#   ./configure.sh --peer-pods [--podvm-ami ami-XXXX] [--podvm-type m6a.large]
#   ./configure.sh --snp-local                 # revert to on-node kata-qemu-snp
#   ./configure.sh --retire-metal              # scale the SEV-SNP metal pool to 0
#   ./configure.sh --show                      # print current effective values
#
# --enable-caa and --peer-pods both auto-discover PODVM_AMI_ID from AWS when it
# is empty (newest AMI from PODVM_AMI_OWNERS matching PODVM_AMI_NAME_FILTER in
# AWS_REGION) and set ENABLE_CAA=true — so you don't pass the AMI/CAA flags.
# --peer-pods additionally flips CONFIDENTIAL_RUNTIME=peer_pods (fleet churn).
# Pass --podvm-ami / PODVM_AMI_ID only to pin a specific image.
#
# Peer Pods rollout (matches deploy/PEER-PODS-PLAN.md):
#   ./configure.sh --enable-caa                            # auto: AMI + CAA, no churn
#   ./02-snp-node-group.sh                                 # terraform: CAA IAM/SG
#   CONFIDENTIAL_RUNTIME=peer_pods ./03-coco-operator.sh   # install CAA (kata-remote)
#   ./05-peer-pods-smoke-test.sh                           # prove it end-to-end (GATE)
#   ./configure.sh --peer-pods                             # flip fleet to kata-remote
#   ./configure.sh --retire-metal                          # after convergence
#
# Note: terraform.tfvars is regenerated from config.env by ./02-snp-node-group.sh
# (regenerate_tfvars), so re-run the relevant numbered step after changing knobs.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

STEP_ID="configure"
echo "=============================================="
echo "  Aura Swarm — configure deploy/config.env"
echo "=============================================="
echo ""

# Knobs configure.sh may write (each must already have an export line in
# config.env). Anything else is rejected to catch typos.
ALLOWED_KEYS=(
    CONFIDENTIAL_RUNTIME
    ENABLE_CAA
    PODVM_AMI_ID
    PODVM_AMI_OWNERS
    PODVM_AMI_NAME_FILTER
    PODVM_INSTANCE_TYPE
    CAA_CHART_VERSION
    CAA_VXLAN_PORT
    CAA_FORWARDER_PORT
    CONFIDENTIAL_NODE_INSTANCE_TYPE
    CONFIDENTIAL_NODE_DESIRED_COUNT
    CONFIDENTIAL_NODE_MIN_COUNT
    CONFIDENTIAL_NODE_MAX_COUNT
    DEFAULT_ISOLATION
)

key_allowed() {
    local k="$1" a
    for a in "${ALLOWED_KEYS[@]}"; do
        [[ "${a}" == "${k}" ]] && return 0
    done
    return 1
}

validate_value() {
    local k="$1" v="$2"
    case "${k}" in
        CONFIDENTIAL_RUNTIME)
            [[ "${v}" == "snp_local" || "${v}" == "peer_pods" ]] \
                || step_fail "CONFIDENTIAL_RUNTIME must be snp_local or peer_pods (got '${v}')" ;;
        ENABLE_CAA)
            [[ "${v}" == "true" || "${v}" == "false" ]] \
                || step_fail "ENABLE_CAA must be true or false (got '${v}')" ;;
        PODVM_AMI_ID)
            [[ -z "${v}" || "${v}" =~ ^ami-[0-9a-f]+$ ]] \
                || step_fail "PODVM_AMI_ID must look like ami-xxxx (got '${v}')" ;;
        CONFIDENTIAL_NODE_DESIRED_COUNT|CONFIDENTIAL_NODE_MIN_COUNT|CONFIDENTIAL_NODE_MAX_COUNT|CAA_VXLAN_PORT|CAA_FORWARDER_PORT)
            [[ "${v}" =~ ^[0-9]+$ ]] || step_fail "${k} must be a non-negative integer (got '${v}')" ;;
        DEFAULT_ISOLATION)
            [[ "${v}" == "confidential_vm" || "${v}" == "container" ]] \
                || step_fail "DEFAULT_ISOLATION must be confidential_vm or container (got '${v}')" ;;
    esac
}

apply_kv() {
    local k="$1" v="$2"
    key_allowed "${k}" \
        || step_fail "key '${k}' is not configurable here (allowed: ${ALLOWED_KEYS[*]})"
    validate_value "${k}" "${v}"
    set_config_value "${k}" "${v}"
}

show_config() {
    echo -e "${CYAN}Effective deploy/config.env values${NC}"
    local k
    for k in "${ALLOWED_KEYS[@]}"; do
        printf '  %-32s = %s\n' "${k}" "${!k:-}"
    done
}

# Ensure PODVM_AMI_ID is set: keep an explicit value/--podvm-ami, otherwise
# auto-discover the newest matching pod-VM AMI from AWS and persist it.
ensure_podvm_ami_set() {
    local ami="${PODVM_AMI:-${PODVM_AMI_ID:-}}"
    [[ -n "${ami}" ]] && return 0
    if [[ -n "${PODVM_AMI_NAME_FILTER:-}" ]]; then
        echo "No PODVM_AMI_ID set — discovering a pod-VM AMI in ${AWS_REGION} (name='${PODVM_AMI_NAME_FILTER}', owners='${PODVM_AMI_OWNERS:-<any>}')..."
    else
        echo "No PODVM_AMI_ID set — discovering a pod-VM AMI in ${AWS_REGION} (CoCo community 'podvm-fedora-amd64-${CAA_CHART_VERSION:-?}', else any podvm image)..."
    fi
    require_cmds aws
    require_aws_auth
    ami="$(resolve_podvm_ami)"
    [[ -n "${ami}" ]] || step_fail "no pod-VM AMI found in ${AWS_REGION}. Options:
  (1) pin a published image:   ./configure.sh PODVM_AMI_ID=ami-XXXX
  (2) point discovery at one:  ./configure.sh PODVM_AMI_NAME_FILTER='podvm-fedora-amd64-*'
  (3) build your own (SEV-SNP): confidential-containers/cloud-api-adaptor → src/cloud-api-adaptor/podvm-mkosi, TEE_PLATFORM=amd
See deploy/PEER-PODS-PLAN.md (pod-VM image provenance) for the build/version-match details."
    echo -e "${GREEN}✓${NC} Discovered pod-VM AMI: ${ami}"
    apply_kv PODVM_AMI_ID "${ami}"
}

if [[ $# -eq 0 ]]; then
    show_config
    echo ""
    echo "Nothing to change. Pass KEY=VALUE or a flag (see the header of this script)."
    exit 0
fi

PENDING=()
PEER_PODS=0 SNP_LOCAL=0 RETIRE_METAL=0 SHOW_ONLY=0 ENABLE_CAA_ONLY=0
PODVM_AMI="" PODVM_TYPE=""

while [[ $# -gt 0 ]]; do
    arg="$1"
    case "${arg}" in
        --peer-pods)    PEER_PODS=1 ;;
        --enable-caa)   ENABLE_CAA_ONLY=1 ;;
        --snp-local)    SNP_LOCAL=1 ;;
        --retire-metal) RETIRE_METAL=1 ;;
        --podvm-ami)    PODVM_AMI="${2:?--podvm-ami needs an AMI id}"; shift ;;
        --podvm-type)   PODVM_TYPE="${2:?--podvm-type needs an instance type}"; shift ;;
        --show)         SHOW_ONLY=1 ;;
        -h|--help)      sed -n '2,30p' "$0"; exit 0 ;;
        *=*)            PENDING+=("${arg%%=*}" "${arg#*=}") ;;
        *)              step_fail "unrecognized argument: ${arg}" ;;
    esac
    shift
done

if [[ "${SHOW_ONLY}" -eq 1 ]]; then
    show_config
    exit 0
fi

[[ "${PEER_PODS}" -eq 1 && "${SNP_LOCAL}" -eq 1 ]] \
    && step_fail "--peer-pods and --snp-local are mutually exclusive"

# Convenience flags expand into concrete key/value updates (applied first so the
# peer-pods preconditions below see any AMI provided in the same invocation).
[[ -n "${PODVM_AMI}" ]]  && apply_kv PODVM_AMI_ID "${PODVM_AMI}"
[[ -n "${PODVM_TYPE}" ]] && apply_kv PODVM_INSTANCE_TYPE "${PODVM_TYPE}"

# --enable-caa: stand up CAA infra (ENABLE_CAA=true + auto-discovered AMI)
# WITHOUT flipping the runtime — the safe staging step before the smoke test.
if [[ "${ENABLE_CAA_ONLY}" -eq 1 ]]; then
    ensure_podvm_ami_set
    apply_kv ENABLE_CAA true
fi

# --peer-pods: everything --enable-caa does, plus flip the fleet to kata-remote.
if [[ "${PEER_PODS}" -eq 1 ]]; then
    ensure_podvm_ami_set
    apply_kv ENABLE_CAA true
    apply_kv CONFIDENTIAL_RUNTIME peer_pods
fi

if [[ "${SNP_LOCAL}" -eq 1 ]]; then
    apply_kv CONFIDENTIAL_RUNTIME snp_local
fi

if [[ "${RETIRE_METAL}" -eq 1 ]]; then
    apply_kv CONFIDENTIAL_NODE_DESIRED_COUNT 0
    apply_kv CONFIDENTIAL_NODE_MIN_COUNT 0
fi

# Explicit KEY=VALUE pairs (stored flattened: key, value, key, value, ...).
i=0
while [[ ${i} -lt ${#PENDING[@]} ]]; do
    apply_kv "${PENDING[${i}]}" "${PENDING[$((i + 1))]}"
    i=$((i + 2))
done

echo ""
# shellcheck disable=SC1090
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
show_config
echo ""
echo -e "${YELLOW}ℹ${NC} Re-run the relevant numbered step to apply (e.g. ./02-snp-node-group.sh, then ./03-coco-operator.sh)."
