#!/bin/bash
# 03-coco-operator.sh - Install the confidential runtime (Cloud API Adaptor).
#
# Confidential agents run as Peer Pods: the Cloud API Adaptor (CAA) Helm release
# (pinned CAA_CHART_VERSION) installs the kata-remote RuntimeClass and boots a
# per-agent AWS-managed SEV-SNP pod VM off-cluster. Workers stay ordinary
# instances (no per-node kata payload, no SNP metal pool).
# Verifies: CAA daemonset Ready on the workers, kata-remote RuntimeClass exists.
#
# Usage: ./03-coco-operator.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "03" "Confidential runtime (Cloud API Adaptor / Peer Pods)" "ops-admin"

require_cmds helm aws kubectl jq
require_aws_auth
ensure_kubectl_context

# CAA cannot launch a confidential pod VM without a pod-VM AMI.
PODVM_AMI_ID="${PODVM_AMI_ID:-}"
[[ -n "${PODVM_AMI_ID}" ]] \
    || step_fail "PODVM_AMI_ID is empty — set it in config.env or auto-discover with ./configure.sh (see deploy/PEER-PODS-PLAN.md §3)"

PODVM_INSTANCE_TYPE="${PODVM_INSTANCE_TYPE:-m6a.large}"
CAA_VXLAN_PORT="${CAA_VXLAN_PORT:-9000}"
CAA_FORWARDER_PORT="${CAA_FORWARDER_PORT:-15150}"
CAA_CHART_VERSION="${CAA_CHART_VERSION:-}"
[[ -n "${CAA_CHART_VERSION}" ]] \
    || step_fail "CAA_CHART_VERSION is empty — pin the Cloud API Adaptor chart version in config.env before installing"

# CAA install targets (kept overridable; defaults match the upstream peerpods chart).
CAA_NAMESPACE="${CAA_NAMESPACE:-confidential-containers-system}"
CAA_RELEASE="${CAA_RELEASE:-cloud-api-adaptor}"
CAA_DAEMONSET="${CAA_DAEMONSET:-cloud-api-adaptor-daemonset}"
# The Cloud API Adaptor (Peer Pods) is published as the `peerpods` OCI chart
# under the cloud-api-adaptor package — NOT a standalone `cloud-api-adaptor`
# chart (that path 403s on GHCR; it does not exist). The chart bundles
# kata-deploy (which creates the kata-remote RuntimeClass and installs the
# remote shim on the workers), a peerpod controller, and a mutating webhook.
# Inspect published versions with:
#   helm show chart oci://ghcr.io/confidential-containers/cloud-api-adaptor/charts/peerpods --version <v>
CAA_CHART_REF="${CAA_CHART_REF:-oci://ghcr.io/confidential-containers/cloud-api-adaptor/charts/peerpods}"
CAA_INSTALL_TIMEOUT="${CAA_INSTALL_TIMEOUT_SECS:-600}"

# The peerpods mutating webhook rewrites kata-remote pods to request the
# kata.peerpods.io/vm extended resource instead of cpu/memory, so confidential
# agents schedule by pod-VM count rather than competing for worker CPU. It is
# REQUIRED for correct peer-pod scheduling (without it agents go Unschedulable
# "Insufficient cpu" on busy nodes), so it defaults ON. It needs cert-manager,
# which this step auto-installs (CAA_AUTO_INSTALL_CERT_MANAGER). Set
# CAA_ENABLE_WEBHOOK=false only on a throwaway cluster where you accept that
# kata-remote pods burn real worker CPU.
CAA_ENABLE_WEBHOOK="${CAA_ENABLE_WEBHOOK:-true}"
# The CAA DaemonSet hard-codes nodeSelector node.kubernetes.io/worker="" (it is
# NOT a chart value), so the workers must carry that label or it stays at 0
# desired. Label all nodes by default (every node here is a worker); narrow it
# by setting CAA_WORKER_NODE_SELECTOR to a kubectl label selector.
CAA_WORKER_NODE_SELECTOR="${CAA_WORKER_NODE_SELECTOR:-}"
# A leftover CoCo operator in the namespace conflicts with the chart's bundled
# kata-deploy (RuntimeClass / payload ownership); refuse to install over it
# unless explicitly told to.
CAA_IGNORE_COCO_OPERATOR="${CAA_IGNORE_COCO_OPERATOR:-false}"

# Terraform-provided network/IAM wiring.
AGENT_SUBNET_IDS_RAW="$(tf_output agent_subnet_ids)"
# agent_subnet_ids may be a TF list output; normalize to comma-separated.
if printf '%s' "${AGENT_SUBNET_IDS_RAW}" | jq -e 'type=="array"' >/dev/null 2>&1; then
    AGENT_SUBNET_IDS="$(printf '%s' "${AGENT_SUBNET_IDS_RAW}" | jq -r 'join(",")')"
else
    AGENT_SUBNET_IDS="${AGENT_SUBNET_IDS_RAW}"
fi
# The peerpods chart takes a SINGLE pod-VM subnet (AWS_SUBNET_ID), not a list;
# launch pod VMs into the first agent subnet (must be EFS-reachable — see
# PEER-PODS-PLAN §6.1). Override with CAA_PODVM_SUBNET_ID to pin another.
CAA_PODVM_SUBNET_ID="${CAA_PODVM_SUBNET_ID:-${AGENT_SUBNET_IDS%%,*}}"
NODE_SG_ID="$(tf_output node_security_group_id)"

# The CAA IRSA role is owned by org-admin's ./01-iam.sh (IAM left terraform for
# separation of duties), so its ARN is derived deterministically from the
# account + resource prefix rather than read from a terraform output. ops-admin
# has iam:GetRole on it, which also validates that org-admin provisioned it.
CAA_ROLE_NAME="${RESOURCE_PREFIX}-caa-role"
CAA_ROLE_ARN="arn:aws:iam::${AWS_ACCOUNT_ID}:role/${CAA_ROLE_NAME}"

[[ -n "${AGENT_SUBNET_IDS}" ]] \
    || step_fail "terraform output agent_subnet_ids is empty — apply the CAA infra (subnets/SG) first (./02-snp-node-group.sh)"
[[ -n "${NODE_SG_ID}" ]] \
    || step_fail "terraform output node_security_group_id is empty — apply the CAA infra first (./02-snp-node-group.sh)"
if ! aws iam get-role --role-name "${CAA_ROLE_NAME}" >/dev/null 2>&1; then
    step_fail "CAA IRSA role ${CAA_ROLE_NAME} not found. It is provisioned by org-admin's ./01-iam.sh on a
  cluster-aware re-run AFTER ./02-snp-node-group.sh. Re-run ./01-iam.sh (org-admin) before this step."
fi

echo "Installing Cloud API Adaptor (chart ${CAA_CHART_REF} @ ${CAA_CHART_VERSION}) into ${CAA_NAMESPACE}..."
echo "  pod-VM AMI:        ${PODVM_AMI_ID}"
echo "  pod-VM type:       ${PODVM_INSTANCE_TYPE}"
echo "  region:            ${AWS_REGION}"
echo "  pod-VM subnet:     ${CAA_PODVM_SUBNET_ID}"
echo "  node SG:           ${NODE_SG_ID}"
echo "  vxlan/forwarder:   ${CAA_VXLAN_PORT} / ${CAA_FORWARDER_PORT}"
echo "  webhook:           ${CAA_ENABLE_WEBHOOK} (cert-manager required when true)"
echo "  CAA image:         ${CAA_IMAGE_NAME}:${CAA_IMAGE_TAG:-<chart default>}"
echo "  IRSA role:         ${CAA_ROLE_ARN:-<none — using node role>}"

kubectl create namespace "${CAA_NAMESPACE}" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

# The bundled mutating webhook needs cert-manager. Ensure it BEFORE the CAA
# release so the webhook's Certificate resources can be issued, instead of
# failing midway through a half-applied release.
if [[ "${CAA_ENABLE_WEBHOOK}" == "true" ]]; then
    if kubectl get crd certificates.cert-manager.io >/dev/null 2>&1; then
        echo -e "${GREEN}✓${NC} cert-manager present (peer-pods webhook prerequisite)"
    elif [[ "${CAA_AUTO_INSTALL_CERT_MANAGER:-true}" == "true" ]]; then
        ensure_cert_manager
    else
        step_fail "CAA_ENABLE_WEBHOOK=true but cert-manager is not installed (no certificates.cert-manager.io CRD) and CAA_AUTO_INSTALL_CERT_MANAGER=false.
  Install cert-manager first, set CAA_AUTO_INSTALL_CERT_MANAGER=true, or set CAA_ENABLE_WEBHOOK=false to install without the peer-pod webhook."
    fi
fi

# A leftover CoCo operator owns kata RuntimeClasses and a kata-deploy payload
# that collide with this chart's bundled kata-deploy. PEER-PODS-PLAN §4 has the
# CAA Helm install REPLACE the operator, so stop until it is removed.
if kubectl get deployment cc-operator-controller-manager -n "${CAA_NAMESPACE}" >/dev/null 2>&1 \
    && [[ "${CAA_IGNORE_COCO_OPERATOR}" != "true" ]]; then
    step_fail "the deprecated CoCo operator is still installed in ${CAA_NAMESPACE} and will conflict with the chart's bundled kata-deploy.
  Remove it first (this step replaces it — see deploy/PEER-PODS-PLAN.md §4):
    kubectl delete ccruntime --all -A --ignore-not-found
    kubectl delete -n ${CAA_NAMESPACE} deployment cc-operator-controller-manager --ignore-not-found
    kubectl delete -n ${CAA_NAMESPACE} daemonset cc-operator-daemon-install cc-operator-daemon-uninstall --ignore-not-found
    kubectl get runtimeclass   # delete any operator-owned kata* RuntimeClasses that remain
  Then re-run ./03-coco-operator.sh. (Override with CAA_IGNORE_COCO_OPERATOR=true only if you are sure they will not collide.)"
fi

# The CAA DaemonSet hard-codes nodeSelector node.kubernetes.io/worker="", so the
# workers must carry that label or it stays at 0 desired (and the readiness
# check below never passes). Label them before installing.
if [[ -n "${CAA_WORKER_NODE_SELECTOR}" ]]; then
    echo "Labeling nodes (-l ${CAA_WORKER_NODE_SELECTOR}) with node.kubernetes.io/worker=..."
    kubectl label nodes -l "${CAA_WORKER_NODE_SELECTOR}" node.kubernetes.io/worker="" --overwrite >/dev/null
else
    echo "Labeling all nodes with node.kubernetes.io/worker= (CAA daemonset placement)..."
    kubectl label nodes --all node.kubernetes.io/worker="" --overwrite >/dev/null
fi

# peerpods chart value keys: provider + env-style providerConfigs.aws.* (mirrors
# the chart's providers/aws.yaml). --set-string keeps AMI ids / ports / bools as
# the string env-vars CAA expects.
#
# AWS credentials: KEYLESS via IRSA. The chart's daemonset.serviceAccount
# .annotations hook (chart >= 0.3.x) stamps the eks.amazonaws.com/role-arn
# annotation onto the cloud-api-adaptor SA so the EKS identity webhook injects a
# web-identity token, and the CAA AWS provider (>= appVersion v0.21, upstream PR
# #3059) picks it up via the AWS SDK default chain. No static AWS keys, no
# peer-pods-secret, no SSO creds (which expire). Requires CAA_CHART_VERSION with
# IRSA support — 0.2.x charts only understand static keys and will CrashLoop.
CAA_SET_ARGS=(
    --set "provider=aws"
    --set-string "providerConfigs.aws.AWS_REGION=${AWS_REGION}"
    --set-string "providerConfigs.aws.AWS_SUBNET_ID=${CAA_PODVM_SUBNET_ID}"
    --set-string "providerConfigs.aws.AWS_SG_IDS=${NODE_SG_ID}"
    --set-string "providerConfigs.aws.PODVM_AMI_ID=${PODVM_AMI_ID}"
    --set-string "providerConfigs.aws.PODVM_INSTANCE_TYPE=${PODVM_INSTANCE_TYPE}"
    --set-string "providerConfigs.aws.DISABLECVM=false"
    --set-string "providerConfigs.aws.VXLAN_PORT=${CAA_VXLAN_PORT}"
    --set-string "providerConfigs.aws.FORWARDER_PORT=${CAA_FORWARDER_PORT}"
    --set "webhook.enabled=${CAA_ENABLE_WEBHOOK}"
)

# Wire the IRSA role onto the CAA service account AT INSTALL TIME via the chart
# hook. --set-json (not --set) because the annotation key has dots and a slash,
# which plain --set would parse as nested map keys.
if [[ -n "${CAA_ROLE_ARN}" ]]; then
    CAA_SET_ARGS+=(--set-json "daemonset.serviceAccount.annotations={\"eks.amazonaws.com/role-arn\":\"${CAA_ROLE_ARN}\"}")
fi

# Override the chart's pinned CAA image. The 0.3.1 chart pins a v0.21.1 image
# whose entrypoint still hard-requires static AWS keys; an image with the
# IRSA-aware entrypoint (one_of AWS_SECRET_ACCESS_KEY/AWS_ROLE_ARN) is required
# for keyless auth. Skipped when CAA_IMAGE_TAG is empty (use the chart default).
if [[ -n "${CAA_IMAGE_TAG:-}" ]]; then
    CAA_SET_ARGS+=(--set "image.name=${CAA_IMAGE_NAME}" --set-string "image.tag=${CAA_IMAGE_TAG}")
fi

# NOTE: no `--wait` here on purpose — helm --wait would block the full timeout
# even when the daemonset pods are CrashLooping in the first few seconds. We let
# helm return as soon as the manifests are applied, then fail FAST below.
if ! helm upgrade --install "${CAA_RELEASE}" "${CAA_CHART_REF}" \
    --version "${CAA_CHART_VERSION}" \
    --namespace "${CAA_NAMESPACE}" \
    "${CAA_SET_ARGS[@]}"; then
    step_fail "helm upgrade --install ${CAA_RELEASE} (${CAA_CHART_REF} @ ${CAA_CHART_VERSION}) failed — see helm output above"
fi
echo -e "${GREEN}✓${NC} Cloud API Adaptor Helm release ${CAA_RELEASE} applied"
if [[ -n "${CAA_ROLE_ARN}" ]]; then
    echo -e "${GREEN}✓${NC} CAA service account wired to IRSA role ${CAA_ROLE_ARN} (keyless; no static AWS keys)"
fi
echo ""

# Verify: CAA daemonset Ready on the workers — fails fast on CrashLoopBackOff
# (e.g. missing AWS creds / IRSA not injected) instead of waiting the full timeout.
if ! wait_daemonset_ready "${CAA_NAMESPACE}" "${CAA_DAEMONSET}" "${CAA_INSTALL_TIMEOUT}"; then
    step_fail "CAA daemonset ${CAA_DAEMONSET} did not become Ready (diagnostics above).
  If the logs show '\$AWS_ACCESS_KEY_ID is NOT set', the CAA image lacks the IRSA-aware
  entrypoint — set CAA_IMAGE_TAG to an image that has it (config.env).
  If they show 'At least one of these must be SET: AWS_SECRET_ACCESS_KEY AWS_ROLE_ARN',
  the IRSA env was not injected — check the SA annotation + the cluster's IAM OIDC provider."
fi

# Verify: the kata-remote RuntimeClass exists.
kubectl get runtimeclass kata-remote >/dev/null 2>&1 \
    || step_fail "RuntimeClass kata-remote does not exist after the CAA install"
echo -e "${GREEN}✓${NC} RuntimeClass kata-remote exists"

# Verify: when enabled, the peer-pods mutating webhook is actually registered.
# This is what makes kata-remote pods schedule by kata.peerpods.io/vm instead of
# competing for worker CPU; without it the peer-pods smoke test (step 05) and
# real agents go Unschedulable "Insufficient cpu" on busy nodes.
if [[ "${CAA_ENABLE_WEBHOOK}" == "true" ]]; then
    WEBHOOK_DEADLINE=$((SECONDS + ${CAA_WEBHOOK_WAIT_SECS:-60}))
    until peerpods_webhook_present || [[ ${SECONDS} -ge ${WEBHOOK_DEADLINE} ]]; do
        sleep 5
    done
    if peerpods_webhook_present; then
        echo -e "${GREEN}✓${NC} Peer-pods mutating webhook registered (kata-remote pods get kata.peerpods.io/vm; cpu/memory stripped)"
    else
        step_fail "CAA_ENABLE_WEBHOOK=true but no peer-pods MutatingWebhookConfiguration appeared after the install — kata-remote pods would keep their cpu/memory and go Unschedulable. Check the CAA release / cert-manager, or set CAA_ENABLE_WEBHOOK=false to opt out."
    fi
fi

#------------------------------------------------------------------------------
# Worker host prerequisite: fuse / mount.fuse.
#
# kata-remote pulls the workload image inside the guest pod VM and presents it
# to containerd via the nydus-overlayfs snapshotter — a FUSE mount that needs
# the `mount.fuse` helper on the worker. Stock EKS AL2/AL2023 AMIs don't ship
# `fuse`, so without this every kata-remote pod sandbox fails with
# 'exec: "mount.fuse": executable file not found in $PATH' (stuck in
# ContainerCreating). Install it via a privileged DaemonSet (idempotent; also
# covers nodes added by later scale-ups).
#------------------------------------------------------------------------------
echo ""
echo "Ensuring the kata-remote host prerequisite (fuse/mount.fuse) on workers..."
FUSE_DS_MANIFEST="${DEPLOY_DIR}/k8s/coco-node-fuse-installer.yaml"
[[ -f "${FUSE_DS_MANIFEST}" ]] || step_fail "missing fuse installer manifest: ${FUSE_DS_MANIFEST}"
sed "s|__CAA_NAMESPACE__|${CAA_NAMESPACE}|g" "${FUSE_DS_MANIFEST}" | kubectl apply -f - >/dev/null
if ! wait_daemonset_ready "${CAA_NAMESPACE}" "kata-remote-fuse-installer" "${CAA_INSTALL_TIMEOUT}"; then
    step_fail "the fuse installer daemonset did not become Ready — workers still lack mount.fuse, which kata-remote needs.
  Inspect: kubectl -n ${CAA_NAMESPACE} logs ds/kata-remote-fuse-installer"
fi
echo -e "${GREEN}✓${NC} Worker fuse prerequisite ensured (mount.fuse present for nydus-overlayfs)"

step_ok "04 (./04-trustee-kbs.sh)"
