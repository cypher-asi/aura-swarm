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
CAA_PEERPODS_LIMIT_PER_NODE="${CAA_PEERPODS_LIMIT_PER_NODE:-10}"
[[ "${CAA_PEERPODS_LIMIT_PER_NODE}" =~ ^[1-9][0-9]*$ ]] \
    || step_fail "CAA_PEERPODS_LIMIT_PER_NODE must be a positive integer (got '${CAA_PEERPODS_LIMIT_PER_NODE}')"

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
# The peerpods chart takes a SINGLE pod-VM subnet (AWS_SUBNET_ID), not a list.
# Prefer the DEDICATED pod-VM subnet (large, isolated from the VPC CNI) so pod VMs
# never hit InsufficientFreeAddressesInSubnet the way the shared agent /25 did.
# Fall back to the first agent subnet for clusters not yet re-applied with the
# dedicated subnet. Override explicitly with CAA_PODVM_SUBNET_ID.
PODVM_SUBNET_IDS_RAW="$(tf_output podvm_subnet_ids 2>/dev/null || true)"
if printf '%s' "${PODVM_SUBNET_IDS_RAW}" | jq -e 'type=="array"' >/dev/null 2>&1; then
    PODVM_SUBNET_IDS="$(printf '%s' "${PODVM_SUBNET_IDS_RAW}" | jq -r 'join(",")')"
else
    PODVM_SUBNET_IDS="${PODVM_SUBNET_IDS_RAW}"
fi
if [[ -n "${CAA_PODVM_SUBNET_ID:-}" ]]; then
    : # operator-pinned; honor it
elif [[ -n "${PODVM_SUBNET_IDS%%,*}" && "${PODVM_SUBNET_IDS%%,*}" != "null" ]]; then
    CAA_PODVM_SUBNET_ID="${PODVM_SUBNET_IDS%%,*}"
else
    CAA_PODVM_SUBNET_ID="${AGENT_SUBNET_IDS%%,*}"
    log_warn "No dedicated pod-VM subnet found (terraform output podvm_subnet_ids empty) — falling back to the agent subnet."
    log_detail "Re-run ./02-snp-node-group.sh to create the dedicated pod-VM subnet (avoids InsufficientFreeAddressesInSubnet)."
fi
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

log_info "Installing Cloud API Adaptor (chart ${CAA_CHART_REF} @ ${CAA_CHART_VERSION}) into ${CAA_NAMESPACE}..."
log_kv "pod-VM AMI" "${PODVM_AMI_ID}"
log_kv "pod-VM type" "${PODVM_INSTANCE_TYPE}"
log_kv "region" "${AWS_REGION}"
log_kv "pod-VM subnet" "${CAA_PODVM_SUBNET_ID}"
log_kv "node SG" "${NODE_SG_ID}"
log_kv "vxlan/forwarder" "${CAA_VXLAN_PORT} / ${CAA_FORWARDER_PORT}"
log_kv "peer-pod capacity" "${CAA_PEERPODS_LIMIT_PER_NODE} VMs per worker"
log_kv "webhook" "${CAA_ENABLE_WEBHOOK} (cert-manager required when true)"
log_kv "CAA image" "${CAA_IMAGE_NAME}:${CAA_IMAGE_TAG:-<chart default>}"
log_kv "IRSA role" "${CAA_ROLE_ARN:-<none — using node role>}"

kubectl create namespace "${CAA_NAMESPACE}" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

# The bundled mutating webhook needs cert-manager. Ensure it BEFORE the CAA
# release so the webhook's Certificate resources can be issued, instead of
# failing midway through a half-applied release.
if [[ "${CAA_ENABLE_WEBHOOK}" == "true" ]]; then
    if kubectl get crd certificates.cert-manager.io >/dev/null 2>&1; then
        log_ok "cert-manager present (peer-pods webhook prerequisite)"
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
    log_info "Labeling nodes (-l ${CAA_WORKER_NODE_SELECTOR}) with node.kubernetes.io/worker= (CAA daemonset placement)..."
else
    log_info "Labeling all nodes with node.kubernetes.io/worker= (CAA daemonset placement)..."
fi
# Idempotent reconcile (covers the dedicated TEE pool + any late-joiner node).
ensure_worker_labels

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
    --set-string "providerConfigs.aws.PEERPODS_LIMIT_PER_NODE=${CAA_PEERPODS_LIMIT_PER_NODE}"
    --set-string "providerConfigs.aws.VXLAN_PORT=${CAA_VXLAN_PORT}"
    --set-string "providerConfigs.aws.FORWARDER_PORT=${CAA_FORWARDER_PORT}"
    --set "webhook.enabled=${CAA_ENABLE_WEBHOOK}"
    # The CAA DaemonSet runs hostNetwork with a startup probe on host :8000. The
    # chart default rollout (maxSurge=1, maxUnavailable=0) starts the new pod on
    # the SAME node before the old one frees the port, so the surge pod hits
    # 'listen tcp :8000: bind: address already in use' and its one-time
    # kata.peerpods.io/vm advertisement can be lost — leaving nodes Unschedulable
    # ('Insufficient kata.peerpods.io/vm'). Replace in place instead (one node at
    # a time, no surge) so every restart cleanly re-advertises.
    --set "daemonset.updateStrategy.maxSurge=0"
    --set "daemonset.updateStrategy.maxUnavailable=1"
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

# A prior run zeroed the kata-remote RuntimeClass overhead with `kubectl patch`
# (see below), which takes the "kubectl-patch" field-manager. The chart applies
# that RuntimeClass via server-side apply, so on re-run helm conflicts:
#   conflicts with "kubectl-patch" ... .overhead.podFixed.cpu/.memory
# Recreate the RuntimeClass cleanly first so helm re-owns it. Safe at deploy
# time; skip when any kata-remote pod is Running so a re-run never disrupts live
# confidential agents (the overhead is already zero from the prior run then).
if kubectl get runtimeclass kata-remote >/dev/null 2>&1; then
    KR_RUNNING=$(kubectl get pods -A -o json 2>/dev/null \
        | jq '[.items[] | select(.spec.runtimeClassName=="kata-remote" and .status.phase=="Running")] | length' 2>/dev/null || echo 0)
    # The only reliable reset is to DELETE the RuntimeClass so helm recreates and
    # owns it cleanly. Stripping managedFields does NOT work: the stripping
    # `kubectl patch` itself runs as the "kubectl-patch" manager and the apiserver
    # immediately re-attributes the surviving fields (incl. .overhead.podFixed) to
    # it, so the conflict returns. Deleting does NOT disrupt RUNNING pods (they
    # keep their already-resolved spec); it only briefly pauses NEW kata-remote
    # admission until helm recreates the RuntimeClass (seconds, in this upgrade).
    kubectl delete runtimeclass kata-remote --ignore-not-found >/dev/null 2>&1 || true
    if [[ "${KR_RUNNING:-0}" -gt 0 ]]; then
        log_ok "Reset kata-remote RuntimeClass field ownership before upgrade (deleted; ${KR_RUNNING} pod(s) keep running — brief new-admission gap until helm recreates it)"
    else
        log_ok "Reset kata-remote RuntimeClass field ownership before upgrade (deleted; helm recreates it)"
    fi
fi

# Same hazard for peer-pods-cm: a prior `kubectl patch` (e.g. hand-editing
# DISABLECVM or PODVM_AMI_ID) takes the "kubectl-patch" field-manager. The chart
# applies that ConfigMap via server-side apply, so on re-run helm conflicts:
#   conflict with "kubectl-patch" using v1: .data.DISABLECVM
# Just stripping managedFields does NOT work: `kubectl patch` itself runs as the
# "kubectl-patch" manager and immediately re-stamps ownership of the data it
# touches. Delete the ConfigMap instead (as we do for the RuntimeClass above) so
# no manager survives and helm re-creates + owns it. Lossless: helm rebuilds the
# cm from the --set values below, and running CAA pods read it only at start
# (helm recreates it in this same upgrade).
if kubectl get configmap peer-pods-cm -n "${CAA_NAMESPACE}" >/dev/null 2>&1; then
    kubectl delete configmap peer-pods-cm -n "${CAA_NAMESPACE}" --ignore-not-found >/dev/null 2>&1 || true
    log_ok "Reset peer-pods-cm field ownership before upgrade (deleted; helm recreates it from chart values)"
fi

# NOTE: no `--wait` here on purpose — helm --wait would block the full timeout
# even when the daemonset pods are CrashLooping in the first few seconds. We let
# helm return as soon as the manifests are applied, then fail FAST below.
if ! helm upgrade --install "${CAA_RELEASE}" "${CAA_CHART_REF}" \
    --version "${CAA_CHART_VERSION}" \
    --namespace "${CAA_NAMESPACE}" \
    "${CAA_SET_ARGS[@]}"; then
    step_fail "helm upgrade --install ${CAA_RELEASE} (${CAA_CHART_REF} @ ${CAA_CHART_VERSION}) failed — see helm output above.
  If it is a server-side-apply field conflict (e.g. 'conflict with \"kubectl-patch\" ... .data.<key>'),
  a manual kubectl patch owns that field. Clear the stale manager and re-run:
    kubectl patch <kind> <name> -n ${CAA_NAMESPACE} --type=json -p='[{\"op\":\"remove\",\"path\":\"/metadata/managedFields\"}]'"
fi
log_ok "Cloud API Adaptor Helm release ${CAA_RELEASE} applied"
if [[ -n "${CAA_ROLE_ARN}" ]]; then
    log_ok "CAA service account wired to IRSA role ${CAA_ROLE_ARN} (keyless; no static AWS keys)"
fi
echo ""

# Verify: CAA daemonset Ready on the workers — fails fast on CrashLoopBackOff
# (e.g. missing AWS creds / IRSA not injected) instead of waiting the full timeout.
# When only one healthy node is needed (CAA_HYPERVISOR_SOCKET_REQUIRE=one), accept
# >=1 Ready instead of blocking on the full fleet: a CAA pod can be Running and
# serving the hypervisor socket while its :8000 readiness probe is still failing,
# and the socket check below validates a genuinely working node anyway.
CAA_MIN_READY=0
[[ "${CAA_HYPERVISOR_SOCKET_REQUIRE:-all}" == "one" ]] && CAA_MIN_READY=1
if ! wait_daemonset_ready "${CAA_NAMESPACE}" "${CAA_DAEMONSET}" "${CAA_INSTALL_TIMEOUT}" 10 3 "${CAA_MIN_READY}"; then
    step_fail "CAA daemonset ${CAA_DAEMONSET} did not become Ready (diagnostics above).
  If the logs show '\$AWS_ACCESS_KEY_ID is NOT set', the CAA image lacks the IRSA-aware
  entrypoint — set CAA_IMAGE_TAG to an image that has it (config.env).
  If they show 'At least one of these must be SET: AWS_SECRET_ACCESS_KEY AWS_ROLE_ARN',
  the IRSA env was not injected — check the SA annotation + the cluster's IAM OIDC provider."
fi
if ! ensure_peerpods_node_capacity "${CAA_NAMESPACE}" "${CAA_DAEMONSET}" "${CAA_INSTALL_TIMEOUT}"; then
    step_fail "CAA daemonset is Ready but workers do not advertise kata.peerpods.io/vm
  even after a rollout restart$([[ "${CAA_SELF_ADVERTISE_VM_CAPACITY:-true}" == "true" ]] && echo " and a direct nodes/status patch").
  Mutated kata-remote pods will stay Pending with 'Insufficient kata.peerpods.io/vm'.
  CAA advertises this extended resource only once at pod startup; see the diagnostics
  above. Check peer-pods-cm PEERPODS_LIMIT_PER_NODE=${CAA_PEERPODS_LIMIT_PER_NODE}, that the
  cloud-api-adaptor SA can patch nodes/status, and the CAA log for the
  '[util/k8sops] Successfully set extended resource' line."
fi

# Verify: every CAA node actually created the remote-hypervisor socket the
# kata-remote shim dials (/run/peerpod/hypervisor.sock). This is SEPARATE from
# readiness and from advertising kata.peerpods.io/vm: an adaptor can be Ready and
# advertise capacity yet never have bound the socket (it logs "server started"
# but came up half-initialized after a surging hostNetwork rollout). When that
# happens, every kata-remote pod that lands on the node fails sandbox creation
# instantly with 'hypervisor.sock: no such file or directory' and is stuck
# ContainerCreating. Clean-restart (delete pod; maxSurge=0 => no collision) any
# node missing it. Set CAA_REQUIRE_HYPERVISOR_SOCKET=false to skip.
if [[ "${CAA_REQUIRE_HYPERVISOR_SOCKET:-true}" == "true" ]]; then
    if ! ensure_caa_hypervisor_socket "${CAA_NAMESPACE}" "${CAA_DAEMONSET}"; then
        step_fail "one or more CAA pods never created /run/peerpod/hypervisor.sock, so the kata-remote
  shim on those nodes cannot reach the hypervisor and every kata-remote pod there fails sandbox
  creation ('dial unix /run/peerpod/hypervisor.sock: connect: no such file or directory', stuck
  ContainerCreating). They were clean-restarted but still did not bind it (diagnostics above).
  Inspect the CAA log on a failing node:
    kubectl -n ${CAA_NAMESPACE} logs <pod> | grep -iE 'hypervisor|socket|listen|bind|server started'
  (or set CAA_REQUIRE_HYPERVISOR_SOCKET=false to skip this guard)."
    fi
fi

# Verify: the kata-remote RuntimeClass exists. We delete it before the upgrade
# (field-ownership reset above) and kata-deploy recreates it ASYNCHRONOUSLY, so
# poll rather than check once — a one-shot check races kata-deploy and spuriously
# fails right after a fresh install.
KR_RC_DEADLINE=$((SECONDS + ${KATA_REMOTE_RC_WAIT_SECS:-180}))
until kubectl get runtimeclass kata-remote >/dev/null 2>&1; do
    if (( SECONDS >= KR_RC_DEADLINE )); then
        step_fail "RuntimeClass kata-remote does not exist ${KATA_REMOTE_RC_WAIT_SECS:-180}s after the CAA install — kata-deploy did not (re)create it. Check the kata-deploy pods: kubectl -n ${CAA_NAMESPACE} get pods | grep kata-deploy"
    fi
    log_progress "waiting for kata-deploy to (re)create the kata-remote RuntimeClass..."
    sleep 5
done
log_ok "RuntimeClass kata-remote exists"

# Zero the kata-remote RuntimeClass pod overhead.
#
# Peer Pods scheduling goes through the kata.peerpods.io/vm extended resource
# (the webhook strips a pod's cpu/memory and requests kata.peerpods.io/vm:1).
# But kata-deploy stamps a DEFAULT kata overhead (podFixed cpu/memory) on the
# kata-remote RuntimeClass, and the scheduler ADDS that overhead to every
# kata-remote pod's effective requests. On a busy node pool that makes pods go
# Unschedulable "Insufficient cpu" even though their containers request 0 cpu
# (the real compute runs off-cluster in the pod VM). Scheduling must rely solely
# on the extended resource, so zero the overhead. kata-deploy re-stamps it on
# each install, so this is re-applied here every run.
CURRENT_OVERHEAD="$(kubectl get runtimeclass kata-remote -o jsonpath='{.overhead.podFixed}' 2>/dev/null || true)"
if [[ -n "${CURRENT_OVERHEAD}" && "${CURRENT_OVERHEAD}" != '{"cpu":"0","memory":"0"}' && "${CURRENT_OVERHEAD}" != "map[cpu:0 memory:0]" ]]; then
    if kubectl patch runtimeclass kata-remote --type=merge \
        -p '{"overhead":{"podFixed":{"cpu":"0","memory":"0"}}}' >/dev/null 2>&1; then
        log_ok "Zeroed kata-remote RuntimeClass overhead (was ${CURRENT_OVERHEAD}; peer-pods schedules via kata.peerpods.io/vm)"
    else
        log_warn "Could not patch the kata-remote RuntimeClass overhead (${CURRENT_OVERHEAD})."
        log_detail "kata-remote pods may go Unschedulable 'Insufficient cpu' on a busy pool; add node CPU or"
        log_detail "patch it manually: kubectl patch runtimeclass kata-remote --type=merge -p '{\"overhead\":{\"podFixed\":{\"cpu\":\"0\",\"memory\":\"0\"}}}'"
    fi
else
    log_ok "kata-remote RuntimeClass overhead already zero (scheduling via kata.peerpods.io/vm)"
fi

# Verify: when enabled, the peer-pods mutating webhook is actually registered.
# This is what makes kata-remote pods schedule by kata.peerpods.io/vm instead of
# competing for worker CPU; without it the peer-pods smoke test (step 05) and
# real agents go Unschedulable "Insufficient cpu" on busy nodes.
# A merely-registered webhook is not enough: if cert-manager has not injected its
# clientConfig.caBundle (or its service has no ready endpoints) the API server
# cannot call it, and with failurePolicy=Ignore it SILENTLY admits kata-remote
# pods unmutated. Wait for the webhook to be *effective*, allowing time for
# cert-manager CA injection after the install.
if [[ "${CAA_ENABLE_WEBHOOK}" == "true" ]]; then
    WEBHOOK_DEADLINE=$((SECONDS + ${CAA_WEBHOOK_WAIT_SECS:-120}))
    while ! peerpods_webhook_effective >/dev/null 2>&1 && [[ ${SECONDS} -lt ${WEBHOOK_DEADLINE} ]]; do
        sleep 5
    done
    WEBHOOK_DIAG=""
    WEBHOOK_RC=0
    WEBHOOK_DIAG=$(peerpods_webhook_effective) || WEBHOOK_RC=$?
    if [[ ${WEBHOOK_RC} -eq 0 ]]; then
        log_ok "Peer-pods mutating webhook effective (kata-remote pods get kata.peerpods.io/vm; cpu/memory stripped)"
        printf '%s\n' "${WEBHOOK_DIAG}" | indent
    else
        log_section "peer-pods webhook diagnostics"
        printf '%s\n' "${WEBHOOK_DIAG}" | indent
        log_detail "--- end diagnostics ---"
        if [[ ${WEBHOOK_RC} -eq 2 ]]; then
            step_fail "CAA_ENABLE_WEBHOOK=true but no peer-pods MutatingWebhookConfiguration appeared after the install — kata-remote pods would keep their cpu/memory and go Unschedulable. Check the CAA release / cert-manager, or set CAA_ENABLE_WEBHOOK=false to opt out."
        else
            step_fail "the peer-pods mutating webhook is registered but NOT effective (see diagnostics above): its clientConfig.caBundle is empty or its service has no ready endpoints, so the API server cannot call it. With failurePolicy=Ignore it SILENTLY admits kata-remote pods unmutated, so they keep cpu/memory and go Unschedulable 'Insufficient cpu'. Fix cert-manager CA injection (cainjector Running, the webhook Certificate Ready so the caBundle is injected) then re-run ./03-coco-operator.sh."
        fi
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
log_info "Ensuring the kata-remote host prerequisite (fuse/mount.fuse) on workers..."
FUSE_DS_MANIFEST="${DEPLOY_DIR}/k8s/coco-node-fuse-installer.yaml"
[[ -f "${FUSE_DS_MANIFEST}" ]] || step_fail "missing fuse installer manifest: ${FUSE_DS_MANIFEST}"
sed "s|__CAA_NAMESPACE__|${CAA_NAMESPACE}|g" "${FUSE_DS_MANIFEST}" | kubectl apply -f - >/dev/null
if ! wait_daemonset_ready "${CAA_NAMESPACE}" "kata-remote-fuse-installer" "${CAA_INSTALL_TIMEOUT}"; then
    step_fail "the fuse installer daemonset did not become Ready — workers still lack mount.fuse, which kata-remote needs.
  Inspect: kubectl -n ${CAA_NAMESPACE} logs ds/kata-remote-fuse-installer"
fi
log_ok "Worker fuse prerequisite ensured (mount.fuse present for nydus-overlayfs)"

#------------------------------------------------------------------------------
# Worker host prerequisite: containerd guest-pull (nydus) snapshotter flags.
#
# Even with the nydus snapshotter wired by kata-deploy, containerd must pass
# image annotations to it and keep layer blobs, or the WORKLOAD container is
# unpacked on the host and fails with "content digest ...: not found"
# (CreateContainerError) while the pause sandbox still guest-pulls. EKS defaults
# both flags to true and kata-deploy does not reliably fix them (wrong table on
# containerd 2.x / config v3 — kata PR #12577). This DaemonSet sets
# disable_snapshot_annotations=false + discard_unpacked_layers=false in a
# containerd drop-in and restarts containerd only when it changed (idempotent).
#------------------------------------------------------------------------------
echo ""
log_info "Ensuring the kata-remote host prerequisite (containerd guest-pull flags) on workers..."
GUESTPULL_DS_MANIFEST="${DEPLOY_DIR}/k8s/coco-node-containerd-guestpull.yaml"
[[ -f "${GUESTPULL_DS_MANIFEST}" ]] || step_fail "missing guest-pull installer manifest: ${GUESTPULL_DS_MANIFEST}"
sed "s|__CAA_NAMESPACE__|${CAA_NAMESPACE}|g" "${GUESTPULL_DS_MANIFEST}" | kubectl apply -f - >/dev/null
if ! wait_daemonset_ready "${CAA_NAMESPACE}" "kata-remote-containerd-guestpull" "${CAA_INSTALL_TIMEOUT}"; then
    step_fail "the containerd guest-pull daemonset did not become Ready — kata-remote workload containers will be unpacked on the host and fail with 'content digest not found' (CreateContainerError).
  Inspect: kubectl -n ${CAA_NAMESPACE} logs ds/kata-remote-containerd-guestpull"
fi
log_ok "Worker containerd guest-pull flags ensured (disable_snapshot_annotations=false, discard_unpacked_layers=false)"

step_ok "04 (./04-trustee-kbs.sh)"
