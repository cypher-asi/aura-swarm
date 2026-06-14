#!/bin/bash
# 04-trustee-kbs.sh - Deploy the Trustee KBS (built-in Attestation Service),
# generate/sync the KBS admin keypair, and make sure the gateway/scheduler
# service secrets (INTERNAL_TOKEN) exist before the platform deploy.
#
# Verifies: kbs pod ready, kbs service resolves and the HTTP listener
# responds in-cluster.
#
# Usage: ./04-trustee-kbs.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "04" "Trustee KBS + admin keypair + service secrets" "ops-admin"

require_cmds aws kubectl jq openssl curl
require_aws_auth
ensure_kubectl_context

# The Trustee KBS + Attestation Service live in swarm-system and release the
# per-agent DEK after SNP attestation; with Peer Pods the CDH runs inside the
# AWS-managed pod VM. Before cutover confirm:
log_warn "Peer Pods attestation: before cutover confirm that"
log_detail "  - the AS reference values / policy.rego accept the AWS pod-VM launch"
log_detail "    measurement + firmware (AWS does not document the launch digest, so"
log_detail "    baseline it empirically or verify VLEK->AMD-root authenticity only);"
log_detail "  - KBS<->CAA CDH/AS protocol versions are compatible with the pinned CAA chart."

#------------------------------------------------------------------------------
# Namespaces + service secrets
#------------------------------------------------------------------------------

log_info "Ensuring namespaces..."
kubectl apply -f "${SCRIPT_DIR}/k8s/00-namespaces.yaml" >/dev/null
log_ok "Namespaces ${K8S_NAMESPACE_SYSTEM} / ${K8S_NAMESPACE_AGENTS}"

# INTERNAL_TOKEN: the service bearer token shared by gateway, scheduler and
# the deploy tooling. Generated once into .secrets/; injected into the
# aura-swarm-secrets manifest by the deploy steps (07/10/12).
ensure_internal_token

#------------------------------------------------------------------------------
# KBS admin keypair (.secrets/kbs-admin.key + kbs-auth-public-key secret)
#------------------------------------------------------------------------------

echo ""
log_info "Ensuring Trustee KBS admin keypair..."
ensure_kbs_admin_keypair

#------------------------------------------------------------------------------
# Trustee deploy (KBS + repository PVC + service + network policies)
#------------------------------------------------------------------------------

echo ""
log_info "Applying Trustee manifests..."
# The KBS repository PVC needs the EFS storage class; make sure it exists
# (terraform output provides the filesystem id, exactly like the legacy flow).
EFS_ID=$(tf_output efs_filesystem_id)
[[ -n "${EFS_ID}" ]] || step_fail "could not read EFS filesystem ID from terraform output"
TMP_SC=$(mktemp)
sed "s/EFS_FILESYSTEM_ID/${EFS_ID}/g" "${SCRIPT_DIR}/k8s/01-storage-class.yaml" > "${TMP_SC}"
kubectl apply -f "${TMP_SC}" >/dev/null
rm -f "${TMP_SC}"

kubectl apply -f "${SCRIPT_DIR}/k8s/11-trustee.yaml"
log_ok "Trustee manifests applied"

echo ""
log_info "Waiting for the KBS deployment..."
kubectl rollout status deployment/kbs -n "${K8S_NAMESPACE_SYSTEM}" --timeout=300s \
    || step_fail "KBS deployment did not become ready (check kbs-repository PVC binding and the kbs-auth-public-key secret)"
log_ok "KBS pod ready"

#------------------------------------------------------------------------------
# Verify: service resolves + listener answers from inside the cluster
#------------------------------------------------------------------------------

echo ""
log_info "Checking KBS reachability in-cluster..."
kbs_in_cluster_check \
    || step_fail "KBS service did not respond in-cluster (kbs.${K8S_NAMESPACE_SYSTEM}.svc.cluster.local:8080)"

step_ok "05 (./05-peer-pods-smoke-test.sh)"
