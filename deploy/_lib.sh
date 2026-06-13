#!/bin/bash
# _lib.sh - Shared helpers for the staged TEE rollout scripts (00-12).
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

# Owner JWT used by the test-agent checks (steps 06/07); empty when unused.
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
# Responsible Identity Center user for this stage (separation of duties): either
# "org-admin" (all IAM) or "ops-admin" (everything else). Set by step_banner's
# optional 3rd arg and appended in brackets to every OK/FAILED line.
STEP_OWNER=""

step_banner() {
    STEP_ID="$1"
    local title="$2"
    STEP_OWNER="${3:-}"
    echo "=============================================="
    echo "  Aura Swarm TEE Rollout - Step ${STEP_ID}"
    echo "  ${title}"
    if [[ -n "${STEP_OWNER}" ]]; then
        echo "  Run as: ${STEP_OWNER}"
    fi
    echo "=============================================="
    echo ""
}

# Bracketed owner suffix (e.g. " [ops-admin]") or empty when no owner is set.
_step_owner_tag() {
    [[ -n "${STEP_OWNER}" ]] && printf ' [%s]' "${STEP_OWNER}"
}

step_ok() {
    local next="${1:-}"
    echo ""
    if [[ -n "${next}" ]]; then
        echo -e "${GREEN}STEP ${STEP_ID} OK$(_step_owner_tag) — proceed to ${next}${NC}"
    else
        echo -e "${GREEN}STEP ${STEP_ID} OK$(_step_owner_tag)${NC}"
    fi
}

# Writes to stderr so the failure line survives command substitution
# (e.g. failures inside render_k8s_manifests).
step_fail() {
    echo "" >&2
    echo -e "${RED}STEP ${STEP_ID} FAILED$(_step_owner_tag): $*${NC}" >&2
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

# `docker info` blocks indefinitely when the daemon is stopped or unreachable;
# wrap it so preflight/build scripts fail fast instead of hanging.
docker_daemon_ok() {
    local timeout_secs="${1:-15}"
    if command -v timeout >/dev/null 2>&1; then
        timeout "${timeout_secs}" docker info >/dev/null 2>&1
        return $?
    fi
    docker info >/dev/null 2>&1 &
    local pid=$!
    local elapsed=0
    while kill -0 "${pid}" 2>/dev/null; do
        if [[ ${elapsed} -ge ${timeout_secs} ]]; then
            kill "${pid}" 2>/dev/null || true
            wait "${pid}" 2>/dev/null || true
            return 124
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done
    wait "${pid}"
}

require_docker() {
    local timeout_secs=15
    if ! docker_daemon_ok "${timeout_secs}"; then
        step_fail "Docker is not running (or not responding within ${timeout_secs}s)"
    fi
    echo -e "${GREEN}✓${NC} Docker daemon is responding"
}

#------------------------------------------------------------------------------
# Separation-of-duties IAM (step 01, org-admin)
#
# This org logs in through IAM Identity Center (SSO): each deploy principal is an
# AWSReservedSSO_* permission-set role. Such roles are NOT directly modifiable
# (iam:AttachRolePolicy -> UnmodifiableEntity), and IAM groups only apply to IAM
# users, so the only way to grant the permissions is to attach the customer-
# managed policy to the PERMISSION SET and re-provision it.
#
# org-admin (the sole IAM owner) provisions BOTH permission sets/policies and ALL
# cluster service roles (EKS cluster, node, CAA IRSA) + the IRSA OIDC provider.
# ops-admin's permission set grants no IAM-write — only a scoped iam:PassRole
# + iam:Get*/List* on the cluster/node role ARNs.
#------------------------------------------------------------------------------

# Render a policy JSON template, substituting the deploy placeholders.
render_iam_policy_doc() {
    local template="$1"
    [[ -f "${template}" ]] || step_fail "missing IAM policy template: ${template}"
    sed \
        -e "s/__AWS_REGION__/${AWS_REGION}/g" \
        -e "s/__AWS_ACCOUNT_ID__/${AWS_ACCOUNT_ID}/g" \
        -e "s/__EKS_CLUSTER_NAME__/${EKS_CLUSTER_NAME}/g" \
        -e "s/__RESOURCE_PREFIX__/${RESOURCE_PREFIX}/g" \
        -e "s/__ORG_IAM_POLICY__/${ORG_IAM_POLICY}/g" \
        -e "s/__OPS_IAM_POLICY__/${OPS_IAM_POLICY}/g" \
        -e "s/__TF_STATE_BUCKET__/${TF_STATE_BUCKET}/g" \
        < "${template}"
}

iam_policy_arn() {
    echo "arn:aws:iam::${AWS_ACCOUNT_ID}:policy/$1"
}

# ensure_iam_policy <policy_name> <template_file>
# Idempotently create/update a customer-managed policy from a template.
ensure_iam_policy() {
    local policy_name="$1" template="$2"
    local doc policy_arn
    doc=$(render_iam_policy_doc "${template}")
    policy_arn=$(iam_policy_arn "${policy_name}")

    if aws iam get-policy --policy-arn "${policy_arn}" >/dev/null 2>&1; then
        # Managed policies allow at most 5 versions; prune oldest non-default
        # ones so repeated updates don't hit LimitExceeded.
        local versions vid
        versions=$(aws iam list-policy-versions --policy-arn "${policy_arn}" --output json 2>/dev/null \
            | jq -r '[.Versions[] | select(.IsDefaultVersion | not)]
                     | sort_by(.CreateDate) | .[].VersionId')
        while [[ $(aws iam list-policy-versions --policy-arn "${policy_arn}" \
                    --query 'length(Versions)' --output text 2>/dev/null) -ge 5 ]]; do
            vid=$(echo "${versions}" | head -1)
            versions=$(echo "${versions}" | tail -n +2)
            [[ -z "${vid}" ]] && break
            aws iam delete-policy-version --policy-arn "${policy_arn}" --version-id "${vid}" >/dev/null 2>&1 \
                && echo -e "  ${YELLOW}↺${NC} Pruned old policy version ${vid}"
        done
        aws iam create-policy-version \
            --policy-arn "${policy_arn}" \
            --policy-document "${doc}" \
            --set-as-default >/dev/null
        echo -e "${GREEN}✓${NC} Updated IAM policy ${policy_name}"
    else
        aws iam create-policy \
            --policy-name "${policy_name}" \
            --policy-document "${doc}" \
            --description "Aura Swarm staged rollout permissions for ${RESOURCE_PREFIX}" \
            --tags "Key=Project,Value=${PROJECT_NAME}" "Key=Environment,Value=${ENVIRONMENT}" \
            >/dev/null
        echo -e "${GREEN}✓${NC} Created IAM policy ${policy_name}"
    fi
}

#------------------------------------------------------------------------------
# Cluster service roles + IRSA (the IAM that left terraform; org-admin owns it).
#------------------------------------------------------------------------------

# ensure_service_role <role_name> <service> <managed_policy_arn>...
# Idempotent create-or-adopt of a service role with a static service trust and a
# set of AWS-managed policy attachments. Static trust => safe to run pre-cluster.
ensure_service_role() {
    local role_name="$1" service="$2"; shift 2
    local managed=("$@")
    local trust
    trust=$(printf '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Principal":{"Service":"%s"},"Action":"sts:AssumeRole"}]}' "${service}")

    if aws iam get-role --role-name "${role_name}" >/dev/null 2>&1; then
        aws iam update-assume-role-policy --role-name "${role_name}" --policy-document "${trust}" >/dev/null
        echo -e "${GREEN}✓${NC} IAM role ${role_name} present (trust refreshed)"
    else
        aws iam create-role --role-name "${role_name}" \
            --assume-role-policy-document "${trust}" \
            --tags "Key=Project,Value=${PROJECT_NAME}" "Key=Environment,Value=${ENVIRONMENT}" >/dev/null
        echo -e "${GREEN}✓${NC} Created IAM role ${role_name}"
    fi
    local arn
    for arn in "${managed[@]}"; do
        aws iam attach-role-policy --role-name "${role_name}" --policy-arn "${arn}" >/dev/null
    done
    echo -e "  ${GREEN}✓${NC} ${role_name}: ${#managed[@]} managed policy attachment(s) ensured"
}

# Create-or-adopt the EKS cluster + node service roles ops-admin's terraform
# reads (data sources) and passes (scoped iam:PassRole). Runs pre-cluster.
ensure_cluster_service_roles() {
    ensure_service_role "${RESOURCE_PREFIX}-eks-cluster-role" "eks.amazonaws.com" \
        "arn:aws:iam::aws:policy/AmazonEKSClusterPolicy" \
        "arn:aws:iam::aws:policy/AmazonEKSVPCResourceController"
    ensure_service_role "${RESOURCE_PREFIX}-eks-node-role" "ec2.amazonaws.com" \
        "arn:aws:iam::aws:policy/AmazonEKSWorkerNodePolicy" \
        "arn:aws:iam::aws:policy/AmazonEKS_CNI_Policy" \
        "arn:aws:iam::aws:policy/AmazonEC2ContainerRegistryReadOnly" \
        "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

# SHA1 thumbprint of the last cert in the OIDC issuer's TLS chain (closest to
# the root CA). AWS ignores it for EKS issuers fronted by a known CA, but the
# CreateOpenIDConnectProvider API still requires a valid-looking value.
oidc_thumbprint() {
    local host="${1%%/*}"
    local chain
    chain=$(echo | openssl s_client -servername "${host}" -showcerts -connect "${host}:443" 2>/dev/null)
    [[ -n "${chain}" ]] || return 1
    local last
    last=$(printf '%s\n' "${chain}" | awk '
        /-----BEGIN CERTIFICATE-----/ { buf=""; inblock=1 }
        inblock { buf = buf $0 "\n" }
        /-----END CERTIFICATE-----/ { last=buf; inblock=0 }
        END { printf "%s", last }')
    [[ -n "${last}" ]] || return 1
    printf '%s' "${last}" | openssl x509 -fingerprint -sha1 -noout 2>/dev/null \
        | sed 's/^.*=//; s/://g' | tr '[:upper:]' '[:lower:]'
}

# Cluster-aware: when the EKS cluster + OIDC issuer exist, create-or-update the
# IAM OIDC provider and the Peer Pods / CAA IRSA role (+ inline pod-VM lifecycle
# policy, incl. ec2:Describe*). When the cluster is not up yet, DEFER (not an
# error): re-run ./01-iam.sh after ./02-snp-node-group.sh creates the cluster.
# Returns 0 when provisioned, 2 when deferred.
ensure_oidc_provider_and_caa_role() {
    # Distinguish "cluster absent" (legit pre-cluster defer) from "org-admin
    # lacks eks:DescribeCluster" (an access problem that must be surfaced, not
    # silently deferred as if the cluster did not exist).
    local issuer rc
    issuer=$(aws eks describe-cluster --name "${EKS_CLUSTER_NAME}" --region "${AWS_REGION}" \
        --query 'cluster.identity.oidc.issuer' --output text 2>&1)
    rc=$?
    if [[ ${rc} -ne 0 ]]; then
        if echo "${issuer}" | grep -qiE 'ResourceNotFound|No cluster found'; then
            echo -e "${YELLOW}ℹ${NC} EKS cluster ${EKS_CLUSTER_NAME} not found yet — deferring OIDC provider + CAA IRSA role."
            echo "  Re-run ./01-iam.sh (org-admin) AFTER ./02-snp-node-group.sh creates the cluster."
            return 2
        fi
        if echo "${issuer}" | grep -qiE 'AccessDenied|not authorized|UnauthorizedException'; then
            step_fail "org-admin cannot eks:DescribeCluster ${EKS_CLUSTER_NAME} (needed to read the OIDC issuer for the CAA role).
  This means ${ORG_IAM_POLICY} is not attached to the ${ORG_SSO_PERMISSION_SET} permission set yet — that policy grants
  eks:DescribeCluster. Attach it from the Identity Center management/delegated-admin account (see the guidance
  printed by the '== Permission sets ==' phase), re-login, and re-run ./01-iam.sh.
  (aws said: ${issuer})"
        fi
        step_fail "eks describe-cluster ${EKS_CLUSTER_NAME} failed: ${issuer}"
    fi
    if [[ -z "${issuer}" || "${issuer}" == "None" ]]; then
        echo -e "${YELLOW}ℹ${NC} EKS cluster ${EKS_CLUSTER_NAME} has no OIDC issuer yet — deferring CAA IRSA role."
        return 2
    fi
    local host="${issuer#https://}"
    echo -e "${GREEN}✓${NC} Cluster OIDC issuer: ${issuer}"

    local provider_arn
    provider_arn=$(aws iam list-open-id-connect-providers --output json 2>/dev/null \
        | jq -r --arg h "${host}" '[.OpenIDConnectProviderList[]?.Arn | select(test($h))][0] // empty')
    if [[ -z "${provider_arn}" ]]; then
        local thumbprint
        thumbprint=$(oidc_thumbprint "${host}") && [[ -n "${thumbprint}" ]] \
            || step_fail "could not compute the OIDC TLS thumbprint for ${host} (need openssl + network egress)"
        provider_arn=$(aws iam create-open-id-connect-provider \
            --url "${issuer}" \
            --client-id-list "sts.amazonaws.com" \
            --thumbprint-list "${thumbprint}" \
            --tags "Key=Project,Value=${PROJECT_NAME}" "Key=Environment,Value=${ENVIRONMENT}" \
            --query 'OpenIDConnectProviderArn' --output text)
        echo -e "${GREEN}✓${NC} Created IAM OIDC provider ${provider_arn}"
    else
        echo -e "${GREEN}✓${NC} IAM OIDC provider present (${provider_arn})"
    fi

    local role_name="${RESOURCE_PREFIX}-caa-role"
    local trust
    trust=$(cat <<JSON
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Effect": "Allow",
      "Principal": { "Federated": "${provider_arn}" },
      "Action": "sts:AssumeRoleWithWebIdentity",
      "Condition": {
        "StringEquals": {
          "${host}:aud": "sts.amazonaws.com",
          "${host}:sub": "system:serviceaccount:confidential-containers-system:cloud-api-adaptor"
        }
      }
    }
  ]
}
JSON
)
    if aws iam get-role --role-name "${role_name}" >/dev/null 2>&1; then
        aws iam update-assume-role-policy --role-name "${role_name}" --policy-document "${trust}" >/dev/null
        echo -e "${GREEN}✓${NC} CAA IRSA role ${role_name} present (trust refreshed)"
    else
        aws iam create-role --role-name "${role_name}" \
            --assume-role-policy-document "${trust}" \
            --tags "Key=Project,Value=${PROJECT_NAME}" "Key=Environment,Value=${ENVIRONMENT}" >/dev/null
        echo -e "${GREEN}✓${NC} Created CAA IRSA role ${role_name}"
    fi

    # Inline pod-VM lifecycle policy. RunInstances/TerminateInstances act on
    # ephemeral pod VMs (ids unknown ahead of time), so resource is "*"; CAA tags
    # every pod VM and the smoke test asserts create + terminate. ec2:Describe*
    # covers DescribeInstances/Images/InstanceTypes/Subnets/SecurityGroups/Vpcs/
    # KeyPairs/LaunchTemplates without per-call whack-a-mole (all read-only,
    # cannot be resource-scoped). This is the grant the daemonset was missing.
    local caa_policy
    caa_policy=$(cat <<JSON
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "PodVMLifecycle",
      "Effect": "Allow",
      "Action": ["ec2:RunInstances", "ec2:TerminateInstances", "ec2:CreateTags", "ec2:Describe*"],
      "Resource": "*"
    },
    {
      "Sid": "PassPodVMInstanceRole",
      "Effect": "Allow",
      "Action": "iam:PassRole",
      "Resource": "arn:aws:iam::${AWS_ACCOUNT_ID}:role/${RESOURCE_PREFIX}-podvm-*"
    }
  ]
}
JSON
)
    aws iam put-role-policy --role-name "${role_name}" \
        --policy-name "${RESOURCE_PREFIX}-caa-policy" \
        --policy-document "${caa_policy}" >/dev/null
    echo -e "${GREEN}✓${NC} CAA inline policy ensured (pod-VM lifecycle incl. ec2:Describe*)"
    echo -e "  ${CYAN}CAA role ARN:${NC} arn:aws:iam::${AWS_ACCOUNT_ID}:role/${role_name}"
}

sso_instance_arn() {
    aws sso-admin list-instances \
        --query 'Instances[0].InstanceArn' --output text 2>/dev/null
}

# Echo the permission-set ARN whose Name matches $2, paginating list-permission-sets.
find_permission_set_arn() {
    local instance_arn="$1" target="$2" next="" out ps name
    while :; do
        if [[ -n "${next}" ]]; then
            out=$(aws sso-admin list-permission-sets --instance-arn "${instance_arn}" \
                --max-results 100 --next-token "${next}" --output json 2>/dev/null) || return 1
        else
            out=$(aws sso-admin list-permission-sets --instance-arn "${instance_arn}" \
                --max-results 100 --output json 2>/dev/null) || return 1
        fi
        while IFS= read -r ps; do
            [[ -z "${ps}" ]] && continue
            name=$(aws sso-admin describe-permission-set --instance-arn "${instance_arn}" \
                --permission-set-arn "${ps}" --query 'PermissionSet.Name' --output text 2>/dev/null)
            if [[ "${name}" == "${target}" ]]; then
                echo "${ps}"
                return 0
            fi
        done < <(echo "${out}" | jq -r '.PermissionSets[]?')
        next=$(echo "${out}" | jq -r '.NextToken // empty')
        [[ -z "${next}" ]] && break
    done
    return 1
}

print_sso_manual_guidance() {
    local policy="$1" instance_arn="${2:-\$INSTANCE_ARN}" ps_arn="${3:-\$PS_ARN}"
    echo -e "${YELLOW}ℹ${NC} Attach ${policy} to the permission set with an account"
    echo "  that has IAM Identity Center admin (management or delegated-admin), then re-login:"
    echo ""
    echo "    aws sso-admin attach-customer-managed-policy-reference-to-permission-set \\"
    echo "      --instance-arn ${instance_arn} --permission-set-arn ${ps_arn} \\"
    echo "      --customer-managed-policy-reference Name=${policy},Path=/"
    echo "    aws sso-admin provision-permission-set \\"
    echo "      --instance-arn ${instance_arn} --permission-set-arn ${ps_arn} \\"
    echo "      --target-type ALL_PROVISIONED_ACCOUNTS"
    echo ""
    echo "  Then refresh credentials (aws sso login, or paste fresh portal keys) and re-run."
}

# ensure_policy_on_permission_set <policy_name> <permission_set_name>
# Attach the customer-managed policy to the SSO permission set and re-provision.
# Best-effort: prints manual guidance if sso-admin is unavailable.
ensure_policy_on_permission_set() {
    local policy="$1" name="$2"
    local instance_arn ps_arn

    instance_arn=$(sso_instance_arn)
    if [[ -z "${instance_arn}" || "${instance_arn}" == "None" ]]; then
        echo -e "${YELLOW}⚠${NC} No IAM Identity Center instance visible from this account."
        print_sso_manual_guidance "${policy}"
        return 0
    fi
    echo -e "${GREEN}✓${NC} Identity Center instance: ${instance_arn}"

    if ! ps_arn=$(find_permission_set_arn "${instance_arn}" "${name}") || [[ -z "${ps_arn}" ]]; then
        echo -e "${YELLOW}⚠${NC} Permission set '${name}' not found (need management/delegated-admin access)."
        print_sso_manual_guidance "${policy}"
        return 0
    fi
    echo -e "${GREEN}✓${NC} Permission set: ${name} (${ps_arn})"

    if aws sso-admin list-customer-managed-policy-references-in-permission-set \
        --instance-arn "${instance_arn}" --permission-set-arn "${ps_arn}" --output json 2>/dev/null \
        | jq -e --arg n "${policy}" \
            '.CustomerManagedPolicyReferences[]? | select(.Name == $n)' >/dev/null 2>&1; then
        echo -e "${GREEN}✓${NC} Permission set ${name} already references ${policy}"
    elif aws sso-admin attach-customer-managed-policy-reference-to-permission-set \
        --instance-arn "${instance_arn}" --permission-set-arn "${ps_arn}" \
        --customer-managed-policy-reference "Name=${policy},Path=/" >/dev/null 2>&1; then
        echo -e "${GREEN}✓${NC} Attached ${policy} reference to permission set ${name}"
    else
        echo -e "${YELLOW}⚠${NC} Could not modify permission set (need sso-admin / delegated-admin)."
        print_sso_manual_guidance "${policy}" "${instance_arn}" "${ps_arn}"
        return 0
    fi

    if aws sso-admin provision-permission-set \
        --instance-arn "${instance_arn}" --permission-set-arn "${ps_arn}" \
        --target-type ALL_PROVISIONED_ACCOUNTS >/dev/null 2>&1; then
        echo -e "${GREEN}✓${NC} Re-provisioned permission set ${name} (propagating to assigned accounts)"
    else
        echo -e "${YELLOW}⚠${NC} Attach recorded but provisioning failed; run provision-permission-set manually."
    fi

    echo ""
    echo -e "${YELLOW}ℹ${NC} Customer-managed policy '${policy}' must exist (same name/path)"
    echo "  in every account this permission set is assigned to."
}

# verify_sso_role_action <role_prefix> <action>...
# Simulate the permission-set ROLE (not the live session) for one or more
# actions. Non-fatal — purely informational.
verify_sso_role_action() {
    local prefix="$1"; shift
    local actions=("$@")
    local role_arn result
    role_arn=$(aws iam list-roles --output json 2>/dev/null \
        | jq -r --arg p "${prefix}" \
            '[.Roles[]? | select(.RoleName | startswith($p)) | .Arn][0] // empty')

    if [[ -z "${role_arn}" ]]; then
        echo -e "${YELLOW}⚠${NC} Role matching ${prefix} not provisioned in this account yet."
        echo "  It appears after the permission set is provisioned + assigned; skipping simulation."
        return 0
    fi

    echo "Simulating ${actions[*]} for ${role_arn}..."
    result=$(aws iam simulate-principal-policy \
        --policy-source-arn "${role_arn}" \
        --action-names "${actions[@]}" \
        --query 'EvaluationResults[?EvalDecision!=`allowed`].ActionName' \
        --output text 2>&1) || {
        echo -e "${YELLOW}⚠${NC} Could not simulate policy (need iam:SimulatePrincipalPolicy): ${result}"
        return 0
    }

    if [[ -n "${result}" && "${result}" != "None" ]]; then
        echo -e "${YELLOW}⚠${NC} Role ${prefix} still missing: ${result}"
        echo "  Provisioning may still be propagating, or the user must re-login to pick it up."
    else
        echo -e "${GREEN}✓${NC} Role ${prefix} can perform: ${actions[*]}"
    fi
}

#------------------------------------------------------------------------------
# EKS node-group convergence (definitive, SCP-proof recovery for step 02)
#
# terraform's post-update WAIT calls eks:DescribeUpdate; if that one action is
# denied (by an identity policy OR an SCP), terraform errors out even though the
# node-group update itself was accepted and applies fine. simulate-principal-
# policy can't see SCPs, so it is not authoritative. These helpers do REAL API
# calls and let step 02 converge via eks:DescribeNodegroup instead.
#------------------------------------------------------------------------------

# Real authorization probe for eks:DescribeUpdate. Uses a dummy update-id so it
# works even when no update exists: AccessDenied => denied, ResourceNotFound/
# validation => allowed. Echoes one of: allowed | denied | unknown.
eks_describe_update_access() {
    local out rc
    out=$(aws eks describe-update \
        --name "${EKS_CLUSTER_NAME}" \
        --nodegroup-name "${RESOURCE_PREFIX}-node-group" \
        --update-id "00000000-0000-0000-0000-000000000000" \
        --region "${AWS_REGION}" 2>&1)
    rc=$?
    if [[ ${rc} -eq 0 ]]; then
        echo "allowed"; return 0
    fi
    if echo "${out}" | grep -qiE 'AccessDenied|not authorized|UnauthorizedException'; then
        echo "denied"; return 0
    fi
    if echo "${out}" | grep -qiE 'ResourceNotFound|No update found|ValidationException|InvalidParameter'; then
        echo "allowed"; return 0
    fi
    echo "unknown"
}

nodegroup_status() {
    aws eks describe-nodegroup \
        --cluster-name "${EKS_CLUSTER_NAME}" \
        --nodegroup-name "$1" \
        --region "${AWS_REGION}" \
        --query 'nodegroup.status' --output text 2>/dev/null || echo "MISSING"
}

# Wait until the managed node group reports ACTIVE (uses DescribeNodegroup,
# NOT DescribeUpdate). Returns 0 on convergence, 1 on timeout.
wait_nodegroups_active() {
    local timeout="${1:-1800}" poll=30 elapsed=0
    local groups=("${RESOURCE_PREFIX}-node-group")
    while [[ ${elapsed} -le ${timeout} ]]; do
        local all_ok=true g status
        for g in "${groups[@]}"; do
            status=$(nodegroup_status "${g}")
            [[ "${status}" == "ACTIVE" ]] || all_ok=false
            echo "  [${elapsed}s] ${g}: ${status}"
        done
        [[ "${all_ok}" == "true" ]] && return 0
        sleep "${poll}"
        elapsed=$((elapsed + poll))
    done
    return 1
}

# True when the saved plan creates or updates an aws_eks_node_group (i.e.
# terraform will enter a DescribeUpdate wait after apply).
plan_changes_nodegroup() {
    local plan_file="$1" n
    n=$(terraform show -json "${plan_file}" 2>/dev/null | jq '
        [.resource_changes[]?
         | select(.type == "aws_eks_node_group")
         | select((.change.actions | index("create")) or (.change.actions | index("update")))
        ] | length')
    [[ "${n:-0}" -gt 0 ]]
}

# Definitive apply for the node-group step: run terraform, and if it fails ONLY
# because eks:DescribeUpdate is denied, converge via DescribeNodegroup instead.
apply_with_describeupdate_fallback() {
    local plan_file="$1" timeout="${2:-1800}"
    local log rc
    log=$(mktemp)

    if terraform apply "${plan_file}" 2>&1 | tee "${log}"; then
        rc=0
    else
        rc=${PIPESTATUS[0]}
    fi

    if [[ ${rc} -eq 0 ]]; then
        rm -f "${log}"
        return 0
    fi

    if grep -qiE 'DescribeUpdate|AccessDeniedException|not authorized to perform' "${log}"; then
        echo ""
        echo -e "${YELLOW}⚠${NC} terraform could not poll the node-group update"
        echo "  (eks:DescribeUpdate denied — identity policy or an org SCP)."
        echo "  Falling back to direct convergence via eks:DescribeNodegroup..."
        rm -f "${log}"
        if wait_nodegroups_active "${timeout}"; then
            echo -e "${GREEN}✓${NC} Node groups reached ACTIVE — the update applied despite the polling denial."
            echo -e "${YELLOW}ℹ${NC} terraform state may not have recorded this update's completion."
            echo "  A later terraform run reconciles once eks:DescribeUpdate is granted"
            echo "  (attach ${OPS_IAM_POLICY} to the ${OPS_SSO_PERMISSION_SET:-ops-admin} permission set and"
            echo "  clear any SCP denying eks:DescribeUpdate)."
            return 0
        fi
        step_fail "node groups did not reach ACTIVE — eks:DescribeUpdate is denied AND convergence failed; fix the identity policy/SCP (see ./01-iam.sh) before retrying"
    fi

    rm -f "${log}"
    step_fail "terraform apply failed (see output above)"
}

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
#
# spec_hash detects ANY spec change (image, env, resources, runtime class). jq
# has no hash builtin, so we emit (pod_name, base64(canonical spec)) pairs and
# sha256 each in the shell. We hash the base64 text directly rather than the
# decoded JSON: base64 is a deterministic, bijective encoding, so the hash is
# an equally valid change detector, and it is only ever compared against other
# hashes from this same function (pre/post snapshots). Hashing the base64 also
# avoids a `base64 --decode` round-trip that is both needless and fragile
# across platforms (a failed decode silently yields the empty-string hash for
# every pod, which would make distinct specs look identical).
snapshot_agent_pods() {
    local output_path="$1"
    local raw
    raw=$(kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent -o json)

    # Windows-native jq.exe writes CRLF on stdout; strip CR so the per-line
    # `read` does not capture a trailing \r and so the final --argjson payload
    # below is valid JSON (a trailing \r makes jq reject it).
    local spec_hashes
    spec_hashes=$(
        printf '%s' "${raw}" \
            | jq -r '.items[] | [(.metadata.name // ""), (.spec | @json | @base64)] | @tsv' \
            | tr -d '\r' \
            | while IFS=$'\t' read -r pod_name spec_b64; do
                [[ -z "${pod_name}" ]] && continue
                hash=$(printf '%s' "${spec_b64}" | sha256sum | awk '{print $1}')
                printf '%s\t%s\n' "${pod_name}" "${hash}"
              done \
            | jq -R -s -c 'split("\n") | map(select(length > 0) | split("\t") | {(.[0]): .[1]}) | add // {}' \
            | tr -d '\r'
    )

    printf '%s' "${raw}" | jq --argjson spec_hashes "${spec_hashes:-{}}" '
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
                spec_hash: ($spec_hashes[(.metadata.name // "")] // "")
            }
        ] | sort_by(.agent_id, .pod_name)
    ' > "${output_path}"
}

# Counts only RUNNING pods on the given runtime class. Terminal pods
# (Failed/Succeeded) and ones still Pending/Terminating do not count — the
# R2 migration monitor and convergence gates reason about running workloads,
# and must not be wedged by a stuck non-running kata-fc pod.
count_pods_on_runtime_class() {
    local runtime_class="$1"
    kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent -o json 2>/dev/null \
        | jq --arg rc "${runtime_class}" \
            '[.items[] | select(.spec.runtimeClassName == $rc and .status.phase == "Running")] | length'
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
# Scripted config.env updates
#
# Persist deploy knobs into config.env while keeping the `${VAR:-default}`
# override pattern intact, so an inline `export VAR=...` in the environment
# still wins at runtime. ./configure.sh and the rollout steps use these so
# operators never hand-edit config.env between steps.
#------------------------------------------------------------------------------

CONFIG_FILE="${DEPLOY_DIR}/config.env"

# Idempotently set the default for KEY in config.env by rewriting its
# `export KEY="${KEY:-VALUE}"` line (the line must already exist). The match is
# anchored on `=` so KEY does not collide with KEY_SUFFIX. Per-line CRLF is
# preserved. Also exports KEY into the current shell so a step that sets a value
# mid-run sees it immediately. Fails if the key has no export line.
set_config_value() {
    local key="$1" value="$2"
    [[ -f "${CONFIG_FILE}" ]] || step_fail "config.env not found at ${CONFIG_FILE}"
    [[ "${key}" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || step_fail "invalid config key: '${key}'"

    local tmp found=0 line bare cr
    tmp=$(mktemp)
    while IFS= read -r line || [[ -n "${line}" ]]; do
        bare="${line%$'\r'}"
        cr=""
        [[ "${line}" != "${bare}" ]] && cr=$'\r'
        if [[ "${bare}" =~ ^[[:space:]]*export[[:space:]]+${key}= ]]; then
            printf 'export %s="${%s:-%s}"%s\n' "${key}" "${key}" "${value}" "${cr}" >> "${tmp}"
            found=1
        else
            printf '%s\n' "${line}" >> "${tmp}"
        fi
    done < "${CONFIG_FILE}"

    if [[ "${found}" -eq 0 ]]; then
        rm -f "${tmp}"
        step_fail "config key ${key} has no 'export ${key}=...' line in config.env"
    fi
    mv "${tmp}" "${CONFIG_FILE}"
    export "${key}=${value}"
    echo -e "${GREEN}✓${NC} config.env: ${key}=${value}"
}

# Echo the newest available x86_64 AMI matching (owners, name) in AWS_REGION, or
# empty. owners may be empty (search all visible images, incl. public community
# AMIs) or a space-separated list ("self", account ids).
_describe_newest_ami() {
    local owners="$1" name="$2"
    local args=(--region "${AWS_REGION}")
    # shellcheck disable=SC2206 # intentional word-split: owners may be a list
    [[ -n "${owners}" ]] && args+=(--owners ${owners})
    local ami
    ami=$(aws ec2 describe-images "${args[@]}" \
        --filters "Name=name,Values=${name}" \
                  "Name=architecture,Values=x86_64" \
                  "Name=state,Values=available" \
        --query 'sort_by(Images, &CreationDate)[-1].ImageId' \
        --output text 2>/dev/null || echo "")
    [[ "${ami}" == "None" ]] && ami=""
    printf '%s' "${ami}"
}

# Pod-VM AMI discovery for Peer Pods. Echoes an AMI id (and nothing else):
#   1. $PODVM_AMI_ID if already set — an explicit pin always wins.
#   2. an explicit $PODVM_AMI_NAME_FILTER (with $PODVM_AMI_OWNERS) if set.
#   3. else the CoCo community published image for this CAA version
#      (podvm-fedora-amd64-<CAA_CHART_VERSION with dots as dashes>),
#   4. else any podvm-fedora-amd64-* (newest), 5. else a self-built podvm-*.
# Echoes empty when none is found. The lookup needs AWS auth; callers that may
# reach the network path should require_aws_auth first.
resolve_podvm_ami() {
    if [[ -n "${PODVM_AMI_ID:-}" ]]; then
        printf '%s' "${PODVM_AMI_ID}"
        return 0
    fi
    if [[ -n "${PODVM_AMI_NAME_FILTER:-}" ]]; then
        _describe_newest_ami "${PODVM_AMI_OWNERS:-}" "${PODVM_AMI_NAME_FILTER}"
        return 0
    fi
    local owners="${PODVM_AMI_OWNERS:-}" ver="${CAA_CHART_VERSION:-}" ami=""
    if [[ -n "${ver}" ]]; then
        ami=$(_describe_newest_ami "${owners}" "podvm-fedora-amd64-${ver//./-}")
    fi
    [[ -z "${ami}" ]] && ami=$(_describe_newest_ami "${owners}" "podvm-fedora-amd64-*")
    [[ -z "${ami}" ]] && ami=$(_describe_newest_ami "self" "podvm-*")
    printf '%s' "${ami}"
}

# Ensure PODVM_AMI_ID is set in config.env: keep an explicit value, otherwise
# auto-discover the newest matching pod-VM AMI in AWS_REGION and persist it
# (and export it into the current shell). Callers must require_aws_auth first.
ensure_podvm_ami() {
    if [[ -n "${PODVM_AMI_ID:-}" ]]; then
        echo -e "${GREEN}✓${NC} PODVM_AMI_ID set: ${PODVM_AMI_ID}"
        return 0
    fi
    if [[ -n "${PODVM_AMI_NAME_FILTER:-}" ]]; then
        echo "No PODVM_AMI_ID set — discovering a pod-VM AMI in ${AWS_REGION} (name='${PODVM_AMI_NAME_FILTER}', owners='${PODVM_AMI_OWNERS:-<any>}')..."
    else
        echo "No PODVM_AMI_ID set — discovering a pod-VM AMI in ${AWS_REGION} (CoCo community 'podvm-fedora-amd64-${CAA_CHART_VERSION:-?}', else any podvm image)..."
    fi
    local ami
    ami="$(resolve_podvm_ami)"
    [[ -n "${ami}" ]] || step_fail "no pod-VM AMI found in ${AWS_REGION}. Pin one explicitly:
  ./configure.sh PODVM_AMI_ID=ami-XXXX
or point discovery at an image:
  ./configure.sh PODVM_AMI_NAME_FILTER='podvm-fedora-amd64-*'
or build a self-built SEV-SNP image (TEE_PLATFORM=amd; see deploy/PEER-PODS-PLAN.md §3)."
    set_config_value PODVM_AMI_ID "${ami}"
}

# Dump describe(Events) + recent logs for the pods of a daemonset (diagnostics).
dump_daemonset_diagnostics() {
    local ns="$1" ds="$2" pod
    echo -e "${YELLOW}--- ${ds} diagnostics ---${NC}"
    kubectl -n "${ns}" get pods -o wide 2>&1 | sed 's/^/  /' || true
    pod=$(kubectl -n "${ns}" get pods -o json 2>/dev/null \
        | jq -r --arg ds "${ds}" '[.items[] | select((.metadata.ownerReferences // [])[]?.name == $ds) | .metadata.name][0] // ""' 2>/dev/null || echo "")
    if [[ -n "${pod}" ]]; then
        echo "  Events for ${pod}:"
        kubectl -n "${ns}" describe pod "${pod}" 2>/dev/null | sed -n '/Events:/,$p' | sed 's/^/    /' || true
        echo "  Logs (current) for ${pod}:"
        kubectl -n "${ns}" logs "${pod}" --tail=50 2>&1 | sed 's/^/    /' || true
        echo "  Logs (previous) for ${pod}:"
        kubectl -n "${ns}" logs "${pod}" --previous --tail=50 2>&1 | sed 's/^/    /' || true
    fi
    echo -e "${YELLOW}--- end diagnostics ---${NC}"
}

# Wait for a DaemonSet to be fully Ready, but FAIL FAST when its pods enter
# CrashLoopBackOff or restart repeatedly instead of burning the whole timeout.
# Dumps diagnostics on failure. Returns 0 Ready, 1 on crashloop/timeout.
# Args: ns ds [timeout=300] [poll=10] [restart_threshold=3]
wait_daemonset_ready() {
    local ns="$1" ds="$2" timeout="${3:-300}" poll="${4:-10}" restarts="${5:-3}"
    local elapsed=0 desired ready bad
    echo "Waiting for daemonset ${ds} in ${ns} (fail-fast on CrashLoopBackOff, timeout ${timeout}s)..."
    while (( elapsed <= timeout )); do
        desired=$(kubectl -n "${ns}" get ds "${ds}" -o jsonpath='{.status.desiredNumberScheduled}' 2>/dev/null || echo 0)
        ready=$(kubectl -n "${ns}" get ds "${ds}" -o jsonpath='{.status.numberReady}' 2>/dev/null || echo 0)
        if [[ "${desired:-0}" -ge 1 && "${ready:-0}" -ge "${desired:-0}" ]]; then
            echo -e "${GREEN}✓${NC} daemonset ${ds} Ready (${ready}/${desired})"
            return 0
        fi
        # Fail fast: any owned pod with a CrashLoopBackOff container or restartCount >= threshold.
        bad=$(kubectl -n "${ns}" get pods -o json 2>/dev/null | jq -r --arg ds "${ds}" --argjson n "${restarts}" '
            [ .items[]
              | select((.metadata.ownerReferences // [])[]?.name == $ds)
              | . as $p | ($p.status.containerStatuses // [])[]
              | select((.state.waiting.reason // "") == "CrashLoopBackOff" or (.restartCount // 0) >= $n)
              | "\($p.metadata.name): \(.state.waiting.reason // ("restartCount=" + ((.restartCount // 0)|tostring)))"
            ] | .[0] // ""' 2>/dev/null || echo "")
        if [[ -n "${bad}" ]]; then
            echo -e "${RED}✗${NC} daemonset ${ds} is failing fast — ${bad}"
            dump_daemonset_diagnostics "${ns}" "${ds}"
            return 1
        fi
        echo "  [${elapsed}s] ${ds} Ready: ${ready:-0}/${desired:-0}"
        sleep "${poll}"; elapsed=$((elapsed + poll))
    done
    echo -e "${RED}✗${NC} daemonset ${ds} not Ready (${ready:-0}/${desired:-0}) after ${timeout}s"
    dump_daemonset_diagnostics "${ns}" "${ds}"
    return 1
}

# Read-only consistency check for the confidential (Peer Pods / CAA) config
# (uses the already-sourced live values). Prints warnings; never mutates.
# Returns the number of problems found so callers can decide.
validate_confidential_runtime_config() {
    local problems=0
    echo -e "${GREEN}✓${NC} Confidential agents run as Peer Pods (CAA kata-remote; per-agent SEV-SNP pod VM)"
    if [[ -z "${PODVM_AMI_ID:-}" ]]; then
        echo -e "${YELLOW}⚠${NC} PODVM_AMI_ID is empty — CAA cannot launch pod VMs (run: ./configure.sh PODVM_AMI_ID=ami-...)"
        problems=$((problems + 1))
    fi
    return "${problems}"
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

# EKS configuration. Agents run as Peer Pods (off-cluster SEV-SNP pod VMs);
# there is no on-node confidential pool. The CAA worker<->pod-VM SG rules are
# always created; the CAA IRSA role is org-admin's (./01-iam.sh), not terraform.
eks_version         = "${EKS_VERSION}"
node_instance_type  = "${NODE_INSTANCE_TYPE}"
node_desired_count  = ${NODE_DESIRED_COUNT}
node_min_count      = ${NODE_MIN_COUNT}
node_max_count      = ${NODE_MAX_COUNT}

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

# Detects the "IAM left terraform" hazard: returns 0 (true) when the plan would
# DESTROY any IAM role / role-policy / attachment / OIDC provider. Those moved to
# org-admin's ./01-iam.sh, so a destroy here means terraform STATE still tracks
# them and must be `terraform state rm`'d first — applying would delete live IAM
# (the cluster/node service roles, the CAA role, the IRSA OIDC provider) that the
# running cluster depends on.
plan_destroys_external_iam() {
    local plan_file="$1" n
    n=$(terraform show -json "${plan_file}" 2>/dev/null | jq '
        [.resource_changes[]?
         | select(.type == "aws_iam_role"
               or .type == "aws_iam_role_policy"
               or .type == "aws_iam_role_policy_attachment"
               or .type == "aws_iam_openid_connect_provider")
         | select((.change.actions | index("delete")))
        ] | length')
    [[ "${n:-0}" -gt 0 ]]
}

# Echo the `terraform state rm` addresses for every IAM resource the plan would
# destroy (one per line) — used to print exact remediation in the step 02 guard.
plan_external_iam_addresses() {
    local plan_file="$1"
    terraform show -json "${plan_file}" 2>/dev/null | jq -r '
        .resource_changes[]?
        | select(.type == "aws_iam_role"
              or .type == "aws_iam_role_policy"
              or .type == "aws_iam_role_policy_attachment"
              or .type == "aws_iam_openid_connect_provider")
        | select((.change.actions | index("delete")))
        | .address'
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
        step_fail "could not resolve an immutable harness digest; run ./06-build-harness.sh or set AURA_HARNESS_IMAGE=<image@sha256:...>"
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

    # Agents reason via the aura-router proxy (AURA_ROUTER_URL), so no raw LLM
    # provider keys are injected here. Only the internal token + zbilling/zero-id
    # secrets are wired in.
    local zero_id zbilling token
    zero_id=$(load_secret "ZERO_ID_SECRET")
    zbilling=$(load_secret "Z_BILLING_API_KEY")
    token=$(load_secret "INTERNAL_TOKEN")
    [[ -n "${token}" ]] || step_fail "missing .secrets/INTERNAL_TOKEN (run ./04-trustee-kbs.sh first)"

    local tmp_dir
    tmp_dir=$(mktemp -d)
    cp "${DEPLOY_DIR}/k8s"/*.yaml "${tmp_dir}/"

    sed -i "s/EFS_FILESYSTEM_ID/${efs_id}/g" "${tmp_dir}/01-storage-class.yaml"

    local secrets_yaml="${tmp_dir}/03-secrets.yaml"
    sed -i "s|REPLACE_WITH_ECR_REGISTRY/RESOURCE_PREFIX-runtime:v0.1.0|${registry}/${RESOURCE_PREFIX}-runtime:${image_tag}|g" "${secrets_yaml}"
    sed -i "s|ECR_REGISTRY/RESOURCE_PREFIX-harness:IMAGE_TAG|${PINNED_HARNESS_IMAGE}|g" "${secrets_yaml}"
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

    # Note: the kata-remote RuntimeClass is created by the Cloud API Adaptor
    # Helm chart (./03-coco-operator.sh), not by a manifest here.
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
        "11-trustee.yaml"
    )

    local manifest
    for manifest in "${manifests[@]}"; do
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
