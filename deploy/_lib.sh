#!/bin/bash
# _lib.sh - Shared helpers for the staged TEE rollout scripts (00-11).
#
# Source AFTER config.env:
#   SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
#   source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
#   source "${SCRIPT_DIR}/_lib.sh"
#
# Conventions carried over from deploy/legacy/:
# - .secrets/ at the repo root holds INTERNAL_TOKEN, kbs-admin.key, API keys
# - the harness image is always pinned to an immutable @sha256 digest,
#   persisted in deploy/.last-harness-image.env by the build helper
# - terraform.tfvars is regenerated from config.env with feature flags
#   preserved; plans are reviewed with explicit destroy warnings
# - gateway /internal/* APIs are reached via kubectl port-forward using the
#   INTERNAL_TOKEN bearer secret

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

# config.env derives DEPLOY_DIR/PROJECT_ROOT from BASH_SOURCE, which resolves
# to /dev/fd when sourced through the CRLF-stripping process substitution —
# re-derive them from the caller's SCRIPT_DIR (always set before sourcing).
DEPLOY_DIR="${SCRIPT_DIR}"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
export DEPLOY_DIR PROJECT_ROOT

SECRETS_DIR="${PROJECT_ROOT}/.secrets"
HARNESS_STATE_FILE="${DEPLOY_DIR}/.last-harness-image.env"
EFS_BACKUP_STATE_FILE="${DEPLOY_DIR}/.last-efs-backup.env"

# Owner JWT used by the test-agent checks (steps 05/06); empty when unused.
SMOKE_TEST_TOKEN="${SMOKE_TEST_TOKEN:-}"

# Staged rollout refs (overridable from the environment / CLI):
#   R1 (dual-mode)  default fa93895
#   R2 (migration)  default af9e034
#   R3 (cleanup)    default master
R1_DEFAULT_REF="fa93895"
R2_DEFAULT_REF="af9e034"
R3_DEFAULT_REF="master"

#------------------------------------------------------------------------------
# Step framing: every script begins with step_banner and ends with exactly one
# step_ok (prints "STEP NN OK — proceed to <next>") or step_fail (prints
# "STEP NN FAILED: <why>" and exits non-zero).
#------------------------------------------------------------------------------

STEP_ID=""

step_banner() {
    STEP_ID="$1"
    local title="$2"
    echo "=============================================="
    echo "  Aura Swarm TEE Rollout - Step ${STEP_ID}"
    echo "  ${title}"
    echo "=============================================="
    echo ""
}

step_ok() {
    local next="${1:-}"
    echo ""
    if [[ -n "${next}" ]]; then
        echo -e "${GREEN}STEP ${STEP_ID} OK — proceed to ${next}${NC}"
    else
        echo -e "${GREEN}STEP ${STEP_ID} OK${NC}"
    fi
}

# Writes to stderr so the failure line survives command substitution
# (e.g. failures inside render_k8s_manifests).
step_fail() {
    echo "" >&2
    echo -e "${RED}STEP ${STEP_ID} FAILED: $*${NC}" >&2
    exit 1
}

#------------------------------------------------------------------------------
# Tooling / auth preflight
#------------------------------------------------------------------------------

require_cmds() {
    local cmd
    for cmd in "$@"; do
        if ! command -v "${cmd}" >/dev/null 2>&1; then
            step_fail "missing required command: ${cmd}"
        fi
    done
}

# Verifies AWS credentials and exports AWS_ACCOUNT_ID / AWS_CALLER_ARN.
require_aws_auth() {
    local arn
    if ! arn=$(aws sts get-caller-identity --output text --query Arn 2>&1); then
        echo "  aws sts get-caller-identity said:"
        echo "    ${arn}"
        step_fail "AWS credentials are missing or expired; refresh your keys/SSO session and re-run"
    fi
    AWS_CALLER_ARN="${arn}"
    AWS_ACCOUNT_ID=$(aws sts get-caller-identity --query Account --output text)
    export AWS_CALLER_ARN AWS_ACCOUNT_ID
    echo -e "${GREEN}✓${NC} AWS identity: ${AWS_CALLER_ARN} (account ${AWS_ACCOUNT_ID}, region ${AWS_REGION})"
}

ecr_registry() {
    echo "${AWS_ACCOUNT_ID}.dkr.ecr.${AWS_REGION}.amazonaws.com"
}

ecr_login() {
    aws ecr get-login-password --region "${AWS_REGION}" \
        | docker login --username AWS --password-stdin "$(ecr_registry)" >/dev/null
    echo -e "${GREEN}✓${NC} Docker authenticated with ECR ($(ecr_registry))"
}

# Verifies kubectl can talk to the deploy cluster; refreshes kubeconfig from
# EKS once before failing (same recovery legacy 07-build-images.sh suggested).
ensure_kubectl_context() {
    if kubectl auth can-i get deployments -n "${K8S_NAMESPACE_SYSTEM}" >/dev/null 2>&1; then
        echo -e "${GREEN}✓${NC} kubectl authenticated to ${EKS_CLUSTER_NAME}"
        return 0
    fi
    echo -e "${YELLOW}⚠${NC} kubectl cannot reach the cluster; refreshing kubeconfig..."
    aws eks update-kubeconfig --region "${AWS_REGION}" --name "${EKS_CLUSTER_NAME}" >/dev/null
    if kubectl auth can-i get deployments -n "${K8S_NAMESPACE_SYSTEM}" >/dev/null 2>&1; then
        echo -e "${GREEN}✓${NC} kubectl authenticated to ${EKS_CLUSTER_NAME} (kubeconfig refreshed)"
        return 0
    fi
    step_fail "kubectl cannot authenticate to ${EKS_CLUSTER_NAME} even after kubeconfig refresh"
}

require_docker() {
    local timeout_secs=15
    local ok=false
    if command -v timeout >/dev/null 2>&1; then
        timeout "${timeout_secs}" docker info >/dev/null 2>&1 && ok=true
    else
        docker info >/dev/null 2>&1 && ok=true
    fi
    if [[ "${ok}" != "true" ]]; then
        step_fail "Docker is not running (or not responding within ${timeout_secs}s)"
    fi
    echo -e "${GREEN}✓${NC} Docker daemon is responding"
}

#------------------------------------------------------------------------------
# Secrets (.secrets/ folder conventions from legacy 08-deploy-k8s.sh)
#------------------------------------------------------------------------------

load_secret() {
    local file="${SECRETS_DIR}/$1"
    if [[ -f "${file}" ]]; then
        tr -d '\n\r' < "${file}"
    else
        echo ""
    fi
}

# INTERNAL_TOKEN is the service bearer token shared by gateway, scheduler and
# deploy verification for /internal/* routes. Generated once if missing.
ensure_internal_token() {
    if [[ -n "$(load_secret INTERNAL_TOKEN)" ]]; then
        echo -e "${GREEN}✓${NC} .secrets/INTERNAL_TOKEN present"
        return 0
    fi
    mkdir -p "${SECRETS_DIR}"
    openssl rand -hex 32 | tr -d '\n' > "${SECRETS_DIR}/INTERNAL_TOKEN"
    echo -e "${GREEN}✓${NC} Generated new .secrets/INTERNAL_TOKEN"
}

internal_token() {
    local token
    token="$(load_secret INTERNAL_TOKEN)"
    if [[ -n "${token}" ]]; then
        printf '%s' "${token}"
        return 0
    fi
    local encoded
    encoded=$(kubectl get secret aura-swarm-secrets -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.data.INTERNAL_TOKEN}' 2>/dev/null || true)
    [[ -z "${encoded}" ]] && return 1
    printf '%s' "${encoded}" | base64 --decode
}

# Trustee KBS admin keypair: private half stays in .secrets/kbs-admin.key,
# public half is synced into the kbs-auth-public-key secret (legacy 08 logic).
ensure_kbs_admin_keypair() {
    local key="${SECRETS_DIR}/kbs-admin.key"
    if [[ ! -f "${key}" ]]; then
        echo "  Generating new Ed25519 KBS admin keypair at .secrets/kbs-admin.key..."
        mkdir -p "${SECRETS_DIR}"
        openssl genpkey -algorithm ed25519 -out "${key}"
    fi
    local pub_tmp
    pub_tmp=$(mktemp)
    openssl pkey -in "${key}" -pubout -out "${pub_tmp}"
    kubectl create secret generic kbs-auth-public-key \
        --from-file=kbs.pem="${pub_tmp}" \
        -n "${K8S_NAMESPACE_SYSTEM}" \
        --dry-run=client -o yaml | kubectl apply -f -
    rm -f "${pub_tmp}"
    echo -e "${GREEN}✓${NC} kbs-auth-public-key secret in sync with .secrets/kbs-admin.key"
}

#------------------------------------------------------------------------------
# Gateway internal API via port-forward (from legacy _redeploy_verify.sh)
#------------------------------------------------------------------------------

GW_PORT_FORWARD_PID=""
GW_PORT_FORWARD_PORT=""

gw_stop_port_forward() {
    if [[ -n "${GW_PORT_FORWARD_PID}" ]] && kill -0 "${GW_PORT_FORWARD_PID}" 2>/dev/null; then
        kill "${GW_PORT_FORWARD_PID}" 2>/dev/null || true
        wait "${GW_PORT_FORWARD_PID}" 2>/dev/null || true
    fi
    GW_PORT_FORWARD_PID=""
    GW_PORT_FORWARD_PORT=""
}

gw_internal_get() {
    local endpoint="$1"
    local token
    if ! token="$(internal_token)"; then
        echo -e "${RED}✗${NC} Missing INTERNAL_TOKEN for gateway internal API." >&2
        return 1
    fi
    curl -fsS -H "Authorization: Bearer ${token}" \
        "http://127.0.0.1:${GW_PORT_FORWARD_PORT}${endpoint}"
}

# Authenticated user-facing API call through the same port-forward.
# Usage: gw_user_api <method> <endpoint> [json-body]
gw_user_api() {
    local method="$1" endpoint="$2" body="${3:-}"
    local args=(-fsS -X "${method}" -H "Authorization: Bearer ${SMOKE_TEST_TOKEN}")
    if [[ -n "${body}" ]]; then
        args+=(-H "Content-Type: application/json" -d "${body}")
    fi
    curl "${args[@]}" "http://127.0.0.1:${GW_PORT_FORWARD_PORT}${endpoint}"
}

gw_start_port_forward() {
    local log_path
    log_path=$(mktemp)
    local attempt
    for attempt in 1 2 3 4 5; do
        gw_stop_port_forward
        GW_PORT_FORWARD_PORT="$((18080 + RANDOM % 1000))"
        : > "${log_path}"
        kubectl port-forward -n "${K8S_NAMESPACE_SYSTEM}" svc/aura-swarm-gateway \
            "${GW_PORT_FORWARD_PORT}:8080" >"${log_path}" 2>&1 &
        GW_PORT_FORWARD_PID=$!
        local _i
        for _i in $(seq 1 20); do
            if gw_internal_get "/internal/health" >/dev/null 2>&1; then
                rm -f "${log_path}"
                return 0
            fi
            if ! kill -0 "${GW_PORT_FORWARD_PID}" 2>/dev/null; then
                break
            fi
            sleep 1
        done
    done
    echo -e "${RED}✗${NC} Failed to port-forward aura-swarm-gateway after multiple attempts."
    if [[ -s "${log_path}" ]]; then
        sed 's/^/  /' "${log_path}" || true
    fi
    rm -f "${log_path}"
    return 1
}

gw_schema_version() {
    gw_internal_get "/internal/health" | jq -r '.schema_version // "null"'
}

#------------------------------------------------------------------------------
# Agent pod snapshots
#------------------------------------------------------------------------------

# JSON array: {agent_id, pod_name, phase, ready, runtime_class, digest, spec_hash}
snapshot_agent_pods() {
    local output_path="$1"
    kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent -o json \
        | jq '
            [
                .items[] | {
                    agent_id: (.metadata.labels["swarm.io/agent-id"] // ""),
                    pod_name: (.metadata.name // ""),
                    phase: (.status.phase // "Unknown"),
                    ready: (
                        [(.status.conditions // [])[]? | select(.type == "Ready" and .status == "True")]
                        | length > 0
                    ),
                    runtime_class: (.spec.runtimeClassName // "<none>"),
                    digest: (
                        (.status.containerStatuses[0].imageID // "") as $image_id
                        | if ($image_id | test("sha256:[a-f0-9]{64}")) then
                            ($image_id | capture("(?<d>sha256:[a-f0-9]{64})").d)
                          else "" end
                    ),
                    spec_hash: (.spec | tostring | @base64 | .[0:24])
                }
            ] | sort_by(.agent_id, .pod_name)
        ' > "${output_path}"
}

count_pods_on_runtime_class() {
    local runtime_class="$1"
    kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent -o json 2>/dev/null \
        | jq --arg rc "${runtime_class}" \
            '[.items[] | select(.spec.runtimeClassName == $rc)] | length'
}

print_fleet_report() {
    echo -e "${CYAN}Fleet report${NC}"
    local pods_json
    pods_json=$(mktemp)
    snapshot_agent_pods "${pods_json}"
    local total
    total=$(jq 'length' "${pods_json}")
    echo "  Agent pods: ${total}"
    jq -r 'group_by(.runtime_class) | .[] | "    \(.[0].runtime_class): \(length)"' "${pods_json}"
    rm -f "${pods_json}"

    if kubectl get svc aura-swarm-gateway -n "${K8S_NAMESPACE_SYSTEM}" >/dev/null 2>&1; then
        if gw_start_port_forward; then
            local agents schema
            agents=$(gw_internal_get "/internal/agents/all" 2>/dev/null | jq 'length' || echo "?")
            schema=$(gw_schema_version || echo "?")
            gw_stop_port_forward
            echo "  Persisted agents: ${agents}"
            echo "  Store schema_version: ${schema}"
        else
            echo -e "  ${YELLOW}⚠${NC} Gateway unreachable; skipping persisted-agent / schema report"
        fi
    else
        echo "  Gateway service not deployed yet"
    fi
}

#------------------------------------------------------------------------------
# Terraform helpers (from legacy _helpers.sh / 02-deploy-network.sh)
#------------------------------------------------------------------------------

TFVARS_FILE="${DEPLOY_DIR}/terraform/terraform.tfvars"

read_tfvar_flag() {
    local flag_name="$1"
    local default_value="${2:-false}"
    if [[ -f "${TFVARS_FILE}" ]]; then
        local line value
        line=$(grep "^${flag_name}[[:space:]]*=" "${TFVARS_FILE}" 2>/dev/null | head -1)
        if [[ -n "${line}" ]]; then
            value=$(echo "${line}" | awk -F= '{print $2}' | tr -d ' ')
            if [[ "${value}" == "true" || "${value}" == "false" ]]; then
                echo "${value}"
                return
            fi
        fi
    fi
    echo "${default_value}"
}

# Regenerates terraform.tfvars from config.env, preserving feature flags that
# were already enabled (exactly the legacy 02-deploy-network.sh convention).
regenerate_tfvars() {
    local existing_network existing_storage existing_eks existing_ecr
    existing_network=$(read_tfvar_flag "enable_network" "false")
    existing_storage=$(read_tfvar_flag "enable_storage" "false")
    existing_eks=$(read_tfvar_flag "enable_eks" "false")
    existing_ecr=$(read_tfvar_flag "enable_ecr" "false")

    cat > "${TFVARS_FILE}" <<EOF
# Auto-generated by deploy scripts
# Environment: ${ENVIRONMENT}
# Generated: $(date -u +"%Y-%m-%dT%H:%M:%SZ")

aws_region          = "${AWS_REGION}"
project_name        = "${PROJECT_NAME}"
environment         = "${ENVIRONMENT}"

# Network configuration
vpc_cidr            = "${VPC_CIDR}"
public_subnet_cidr  = "${PUBLIC_SUBNET_CIDR}"
private_subnet_cidr = "${PRIVATE_SUBNET_CIDR}"
agent_subnet_cidr   = "${AGENT_SUBNET_CIDR}"
storage_subnet_cidr = "${STORAGE_SUBNET_CIDR}"

# EKS configuration
eks_version         = "${EKS_VERSION}"
node_instance_type  = "${NODE_INSTANCE_TYPE}"
node_desired_count  = ${NODE_DESIRED_COUNT}
node_min_count      = ${NODE_MIN_COUNT}
node_max_count      = ${NODE_MAX_COUNT}

# Confidential (SEV-SNP bare metal) node group
confidential_node_instance_type = "${CONFIDENTIAL_NODE_INSTANCE_TYPE}"
confidential_node_desired_count = ${CONFIDENTIAL_NODE_DESIRED_COUNT}
confidential_node_min_count     = ${CONFIDENTIAL_NODE_MIN_COUNT}
confidential_node_max_count     = ${CONFIDENTIAL_NODE_MAX_COUNT}

# Feature flags — preserved from previous state
enable_network = ${existing_network}
enable_storage = ${existing_storage}
enable_eks     = ${existing_eks}
enable_ecr     = ${existing_ecr}
EOF
    echo -e "${GREEN}✓${NC} Regenerated terraform.tfvars (flags: network=${existing_network} storage=${existing_storage} eks=${existing_eks} ecr=${existing_ecr})"
}

warn_about_destroys() {
    local plan_file="$1"
    PLAN_HAS_DESTROYS=false

    local show_output
    show_output=$(terraform show -no-color "${plan_file}" 2>/dev/null) || return 0

    local destroyed_lines
    destroyed_lines=$(echo "${show_output}" | grep "will be destroyed" || true)
    [[ -z "${destroyed_lines}" ]] && return 0

    PLAN_HAS_DESTROYS=true
    local destroy_count
    destroy_count=$(echo "${destroyed_lines}" | wc -l | tr -d ' ')

    echo ""
    echo -e "${RED}================================================================${NC}"
    echo -e "${RED}  WARNING: This plan will DESTROY ${destroy_count} resource(s)${NC}"
    echo -e "${RED}================================================================${NC}"
    echo "${destroyed_lines}" | sed 's/.*# /  - /; s/ will be destroyed.*//'
    echo ""
}

confirm_plan() {
    local plan_file="$1"
    warn_about_destroys "${plan_file}"
    echo ""
    echo -e "${YELLOW}Review the plan above before proceeding${NC}"
    if [[ "${PLAN_HAS_DESTROYS}" == "true" ]]; then
        echo -e "${RED}Type 'destroy' to confirm destructive changes, or anything else to abort.${NC}"
        read -r -p "Confirm: " confirm
        if [[ "${confirm}" != "destroy" ]]; then
            echo "Aborted. No changes applied."
            exit 0
        fi
    else
        read -r -p "Apply this plan? (yes/no) [no]: " confirm
        confirm=${confirm:-no}
        if [[ "${confirm}" != "yes" ]]; then
            echo "Aborted. Run this script again when ready."
            exit 0
        fi
    fi
}

# Detects the EFS-encryption replacement hazard: returns 0 (true) when the
# plan REPLACES the aws_efs_file_system (delete+create), which would destroy
# all agent state. Triggered by an existing unencrypted EFS meeting the
# now-mandatory encrypted=true in the storage module.
plan_replaces_efs() {
    local plan_file="$1"
    local replaced
    replaced=$(terraform show -json "${plan_file}" 2>/dev/null | jq -r '
        [.resource_changes[]?
         | select(.type == "aws_efs_file_system")
         | select((.change.actions | contains(["delete"])) and (.change.actions | contains(["create"])))
        ] | length')
    [[ "${replaced:-0}" -gt 0 ]]
}

tf_output() {
    terraform -chdir="${DEPLOY_DIR}/terraform" output -json 2>/dev/null \
        | jq -r --arg key "$1" '.[$key].value // empty'
}

#------------------------------------------------------------------------------
# Image build + push at a git ref (mechanics from legacy 07-build-images.sh,
# re-targeted at an arbitrary ref via a throwaway git worktree)
#------------------------------------------------------------------------------

export BUILDKIT_PROGRESS="${BUILDKIT_PROGRESS:-plain}"

resolve_git_ref() {
    git -C "${PROJECT_ROOT}" rev-parse --verify --quiet "$1^{commit}" \
        || step_fail "cannot resolve git ref '$1' in ${PROJECT_ROOT}"
}

# Builds gateway/control/scheduler images from PROJECT_ROOT at the given ref
# and pushes them tagged with the short sha. Sets PLATFORM_IMAGE_TAG.
build_and_push_platform_images() {
    local ref="$1"
    local sha
    sha=$(resolve_git_ref "${ref}")
    PLATFORM_IMAGE_TAG="${sha:0:7}"
    export PLATFORM_IMAGE_TAG

    local registry
    registry=$(ecr_registry)

    local worktree
    worktree=$(mktemp -d)/src
    echo "Checking out ${ref} (${sha}) into a throwaway worktree..."
    git -C "${PROJECT_ROOT}" worktree add --detach "${worktree}" "${sha}" >/dev/null

    local build_rc=0
    local service
    for service in gateway control scheduler; do
        local image_name="${RESOURCE_PREFIX}-${service}"
        local full_image="${registry}/${image_name}:${PLATFORM_IMAGE_TAG}"

        # Skip rebuild if this exact tag already exists in ECR (idempotent re-run)
        if aws ecr describe-images --repository-name "${image_name}" \
            --image-ids imageTag="${PLATFORM_IMAGE_TAG}" >/dev/null 2>&1; then
            echo -e "${GREEN}✓${NC} ${full_image} already in ECR — skipping build"
            continue
        fi

        echo ""
        echo "Building ${service} at ${PLATFORM_IMAGE_TAG}..."
        local cargo_features=""
        if [[ "${service}" == "gateway" && "${DEV_MODE:-false}" == "true" ]]; then
            cargo_features="--features dev-mode"
        fi
        if ! docker build \
            --progress=plain \
            --build-arg SERVICE="${service}" \
            --build-arg CARGO_FEATURES="${cargo_features}" \
            -t "${image_name}:${PLATFORM_IMAGE_TAG}" \
            -t "${full_image}" \
            -f "${worktree}/docker/Dockerfile" "${worktree}"; then
            build_rc=1
            break
        fi
        if ! docker push "${full_image}"; then
            build_rc=1
            break
        fi
        echo -e "${GREEN}✓${NC} Pushed ${full_image}"
    done

    git -C "${PROJECT_ROOT}" worktree remove --force "${worktree}" >/dev/null 2>&1 || true
    [[ "${build_rc}" -eq 0 ]] || step_fail "image build/push failed for ref ${ref}"
}

# Resolves the immutable harness digest (env > state file > ECR lookup),
# refusing mutable tags exactly like legacy 08-deploy-k8s.sh.
resolve_pinned_harness_image() {
    local registry
    registry=$(ecr_registry)
    PINNED_HARNESS_IMAGE="${AURA_HARNESS_IMAGE:-}"

    if [[ -z "${PINNED_HARNESS_IMAGE}" && -f "${HARNESS_STATE_FILE}" ]]; then
        # shellcheck disable=SC1090
        source <(tr -d '\r' < "${HARNESS_STATE_FILE}")
        PINNED_HARNESS_IMAGE="${AURA_HARNESS_IMAGE:-}"
    fi

    if [[ -z "${PINNED_HARNESS_IMAGE}" ]]; then
        local digest
        digest=$(aws ecr describe-images \
            --repository-name "${RESOURCE_PREFIX}-harness" \
            --image-ids imageTag="${IMAGE_TAG}" \
            --query 'imageDetails[0].imageDigest' \
            --output text 2>/dev/null || echo "")
        if [[ -n "${digest}" && "${digest}" != "None" ]]; then
            PINNED_HARNESS_IMAGE="${registry}/${RESOURCE_PREFIX}-harness@${digest}"
        fi
    fi

    if [[ -z "${PINNED_HARNESS_IMAGE}" ]]; then
        step_fail "could not resolve an immutable harness digest; run legacy/07-build-images.sh --harness or set AURA_HARNESS_IMAGE=<image@sha256:...>"
    fi
    if [[ ! "${PINNED_HARNESS_IMAGE}" =~ @sha256:[a-f0-9]{64}$ ]]; then
        step_fail "AURA_HARNESS_IMAGE must be pinned to an immutable digest (got: ${PINNED_HARNESS_IMAGE})"
    fi
    export PINNED_HARNESS_IMAGE
    echo -e "${GREEN}✓${NC} Pinned harness image: ${PINNED_HARNESS_IMAGE}"
}

#------------------------------------------------------------------------------
# K8s manifest templating + apply (from legacy 08-deploy-k8s.sh)
#------------------------------------------------------------------------------

# Renders all deploy/k8s manifests into a temp dir with images/secrets
# injected. Echoes the temp dir path. Caller must rm -rf it.
render_k8s_manifests() {
    local image_tag="$1"
    local registry efs_id
    registry=$(ecr_registry)
    efs_id=$(tf_output efs_filesystem_id)
    [[ -n "${efs_id}" ]] || step_fail "could not read EFS filesystem ID from terraform output"

    local anthropic openai zero_id zbilling token
    anthropic=$(load_secret "ANTHROPIC_API_KEY")
    openai=$(load_secret "OPENAI_API_KEY")
    zero_id=$(load_secret "ZERO_ID_SECRET")
    zbilling=$(load_secret "Z_BILLING_API_KEY")
    token=$(load_secret "INTERNAL_TOKEN")
    [[ -n "${anthropic}" ]] || step_fail "missing .secrets/ANTHROPIC_API_KEY"
    [[ -n "${token}" ]] || step_fail "missing .secrets/INTERNAL_TOKEN (run ./03-trustee-kbs.sh first)"

    local tmp_dir
    tmp_dir=$(mktemp -d)
    cp "${DEPLOY_DIR}/k8s"/*.yaml "${tmp_dir}/"

    sed -i "s/EFS_FILESYSTEM_ID/${efs_id}/g" "${tmp_dir}/01-storage-class.yaml"

    local secrets_yaml="${tmp_dir}/03-secrets.yaml"
    sed -i "s|REPLACE_WITH_ECR_REGISTRY/RESOURCE_PREFIX-runtime:v0.1.0|${registry}/${RESOURCE_PREFIX}-runtime:${image_tag}|g" "${secrets_yaml}"
    sed -i "s|ECR_REGISTRY/RESOURCE_PREFIX-harness:IMAGE_TAG|${PINNED_HARNESS_IMAGE}|g" "${secrets_yaml}"
    sed -i "s|__ANTHROPIC_API_KEY__|${anthropic}|g" "${secrets_yaml}"
    sed -i "s|__OPENAI_API_KEY__|${openai:-placeholder-not-set}|g" "${secrets_yaml}"
    sed -i "s|__ZERO_ID_SECRET__|${zero_id:-placeholder-not-set}|g" "${secrets_yaml}"
    sed -i "s|__Z_BILLING_API_KEY__|${zbilling:-}|g" "${secrets_yaml}"
    sed -i "s|__INTERNAL_TOKEN__|${token}|g" "${secrets_yaml}"
    sed -i "s|__DEFAULT_ISOLATION__|${DEFAULT_ISOLATION}|g" "${secrets_yaml}"

    local manifest
    for manifest in "${tmp_dir}"/05-*.yaml "${tmp_dir}"/06-*.yaml "${tmp_dir}"/07-*.yaml; do
        [[ -f "${manifest}" ]] || continue
        sed -i "s|ECR_REGISTRY|${registry}|g" "${manifest}"
        sed -i "s|RESOURCE_PREFIX|${RESOURCE_PREFIX}|g" "${manifest}"
        sed -i "s|IMAGE_TAG|${image_tag}|g" "${manifest}"
    done

    echo "${tmp_dir}"
}

# Applies the full platform manifest set with images at the given tag, then
# restarts the scheduler (env from ConfigMap is injected at pod creation).
apply_platform_manifests() {
    local image_tag="$1"
    local tmp_dir
    tmp_dir=$(render_k8s_manifests "${image_tag}")

    local manifests=(
        "00-namespaces.yaml"
        "01-storage-class.yaml"
        "02-pvc.yaml"
        "03-secrets.yaml"
        "04-rbac.yaml"
        "05-gateway.yaml"
        "06-control.yaml"
        "07-scheduler.yaml"
        "08-network-policies.yaml"
        "09-runtime-class.yaml"
        "10-coco-ccruntime.yaml"
        "11-trustee.yaml"
    )

    local manifest
    for manifest in "${manifests[@]}"; do
        if [[ "${manifest}" == "10-coco-ccruntime.yaml" ]] && \
           ! kubectl get crd ccruntimes.confidentialcontainers.org >/dev/null 2>&1; then
            echo -e "${YELLOW}⚠${NC} Skipping ${manifest} (CoCo operator not installed — run ./02-coco-operator.sh)"
            continue
        fi
        echo "Applying ${manifest}..."
        kubectl apply -f "${tmp_dir}/${manifest}" >/dev/null
    done
    rm -rf "${tmp_dir}"
    echo -e "${GREEN}✓${NC} Manifests applied (images at tag ${image_tag})"

    kubectl rollout restart deployment/aura-swarm-scheduler -n "${K8S_NAMESPACE_SYSTEM}" >/dev/null
}

wait_platform_rollouts() {
    local d
    for d in aura-swarm-gateway aura-swarm-control aura-swarm-scheduler; do
        kubectl rollout status "deployment/${d}" -n "${K8S_NAMESPACE_SYSTEM}" --timeout=300s \
            || step_fail "rollout of ${d} did not complete"
    done
    echo -e "${GREEN}✓${NC} Gateway/control/scheduler rollouts complete"
}

wait_pvcs_bound() {
    local timeout=60 elapsed=0 interval=5
    while [[ ${elapsed} -lt ${timeout} ]]; do
        local gw ctl
        gw=$(kubectl get pvc aura-swarm-gateway-data -n "${K8S_NAMESPACE_SYSTEM}" \
            -o jsonpath='{.status.phase}' 2>/dev/null || echo "NotFound")
        ctl=$(kubectl get pvc aura-swarm-control-data -n "${K8S_NAMESPACE_SYSTEM}" \
            -o jsonpath='{.status.phase}' 2>/dev/null || echo "NotFound")
        if [[ "${gw}" == "Bound" && "${ctl}" == "Bound" ]]; then
            echo -e "${GREEN}✓${NC} Platform PVCs bound"
            return 0
        fi
        echo "  [${elapsed}s] gateway-data=${gw} control-data=${ctl}"
        sleep "${interval}"
        elapsed=$((elapsed + interval))
    done
    step_fail "platform PVCs did not bind within ${timeout}s (check EFS CSI driver / IRSA; see legacy/fix-pending-pods.sh)"
}

# Full platform deploy at a git ref: build+push, render+apply, wait healthy.
deploy_platform_at_ref() {
    local ref="$1"
    require_docker
    ecr_login
    build_and_push_platform_images "${ref}"
    resolve_pinned_harness_image
    apply_platform_manifests "${PLATFORM_IMAGE_TAG}"
    wait_pvcs_bound
    wait_platform_rollouts
}

#------------------------------------------------------------------------------
# Misc verification helpers
#------------------------------------------------------------------------------

# In-cluster KBS reachability: service DNS resolves and the HTTP listener
# answers (KBS has no dedicated health route; any HTTP status proves liveness).
kbs_in_cluster_check() {
    local pod="kbs-healthcheck-$$"
    local code
    code=$(kubectl run "${pod}" -n "${K8S_NAMESPACE_SYSTEM}" --restart=Never --rm -i --quiet \
        --image=curlimages/curl:8.10.1 --command -- \
        curl -s -o /dev/null -w '%{http_code}' --max-time 10 \
        "http://kbs.${K8S_NAMESPACE_SYSTEM}.svc.cluster.local:8080/kbs/v0/auth" 2>/dev/null || echo "000")
    kubectl delete pod "${pod}" -n "${K8S_NAMESPACE_SYSTEM}" --ignore-not-found >/dev/null 2>&1 || true
    if [[ "${code}" == "000" ]]; then
        return 1
    fi
    echo -e "${GREEN}✓${NC} KBS service resolves and responds in-cluster (HTTP ${code})"
}

# Poll until a jq condition over the agent-pod snapshot holds.
# Usage: wait_for_pods_condition <jq-bool-expr> <timeout-secs> <description>
wait_for_pods_condition() {
    local expr="$1" timeout="$2" desc="$3"
    local poll=15 elapsed=0
    local snap
    snap=$(mktemp)
    while [[ ${elapsed} -le ${timeout} ]]; do
        snapshot_agent_pods "${snap}"
        if [[ "$(jq "${expr}" "${snap}")" == "true" ]]; then
            rm -f "${snap}"
            echo -e "${GREEN}✓${NC} ${desc} (after ${elapsed}s)"
            return 0
        fi
        echo "  [${elapsed}s] waiting: ${desc}"
        sleep "${poll}"
        elapsed=$((elapsed + poll))
    done
    rm -f "${snap}"
    return 1
}
