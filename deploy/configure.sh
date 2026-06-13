#!/bin/bash
# configure.sh - Scripted, idempotent updates to deploy/config.env.
#
# Persists deploy knobs into config.env (keeping the ${VAR:-default} override
# pattern) so every numbered step picks them up — no hand-editing between steps.
#
# Confidential agents always run as Peer Pods (CAA kata-remote); there is no
# runtime switch. This script sets the Peer Pods / pod-VM knobs.
#
# Usage:
#   ./configure.sh KEY=VALUE [KEY=VALUE ...]   # set one or more allowlisted knobs
#   ./configure.sh --discover-ami              # auto-discover + pin PODVM_AMI_ID from AWS
#   ./configure.sh --show                      # print current effective values
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
    PODVM_AMI_ID
    PODVM_AMI_OWNERS
    PODVM_AMI_NAME_FILTER
    PODVM_INSTANCE_TYPE
    CAA_CHART_VERSION
    CAA_VXLAN_PORT
    CAA_FORWARDER_PORT
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
        PODVM_AMI_ID)
            [[ -z "${v}" || "${v}" =~ ^ami-[0-9a-f]+$ ]] \
                || step_fail "PODVM_AMI_ID must look like ami-xxxx (got '${v}')" ;;
        CAA_VXLAN_PORT|CAA_FORWARDER_PORT)
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

if [[ $# -eq 0 ]]; then
    show_config
    echo ""
    echo "Nothing to change. Pass KEY=VALUE or a flag (see the header of this script)."
    exit 0
fi

PENDING=()
DISCOVER_AMI=0 SHOW_ONLY=0

while [[ $# -gt 0 ]]; do
    arg="$1"
    case "${arg}" in
        --discover-ami) DISCOVER_AMI=1 ;;
        --show)         SHOW_ONLY=1 ;;
        -h|--help)      sed -n '2,18p' "$0"; exit 0 ;;
        *=*)            PENDING+=("${arg%%=*}" "${arg#*=}") ;;
        *)              step_fail "unrecognized argument: ${arg}" ;;
    esac
    shift
done

if [[ "${SHOW_ONLY}" -eq 1 ]]; then
    show_config
    exit 0
fi

# Explicit KEY=VALUE pairs first (so discovery below sees any value just set).
i=0
while [[ ${i} -lt ${#PENDING[@]} ]]; do
    apply_kv "${PENDING[${i}]}" "${PENDING[$((i + 1))]}"
    i=$((i + 2))
done

if [[ "${DISCOVER_AMI}" -eq 1 ]]; then
    # Step 02 already does this automatically; this flag is just a manual
    # override path (e.g. to re-pin after clearing PODVM_AMI_ID).
    require_cmds aws
    require_aws_auth
    ensure_podvm_ami
fi

echo ""
# shellcheck disable=SC1090
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
show_config
echo ""
echo -e "${YELLOW}ℹ${NC} Re-run the relevant numbered step to apply (e.g. ./02-snp-node-group.sh, then ./03-coco-operator.sh)."
