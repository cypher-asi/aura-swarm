#!/bin/bash
# 07-deploy-r1.sh - Build, push and deploy gateway/control/scheduler at the
# R1 (dual-mode) ref. Every NEW agent becomes a confidential SNP VM with
# sealed storage; legacy agents must remain byte-for-byte untouched.
#
# Verifies: rollouts complete, /internal/health OK, legacy agent pods
# untouched (same pods, same runtime class, same spec hash), and one new
# test agent lands on kata-remote with a sealed env and a billing sku.
#
# Usage:
#   ./07-deploy-r1.sh                 # deploys SWARM_R1_REF (default fa93895)
#   ./07-deploy-r1.sh --ref <git-ref>
#   ./07-deploy-r1.sh --skip-test-agent
#   ./07-deploy-r1.sh --relogin       # force a fresh zOS login (ignore cache)
#
# The test-agent check needs an owner session. Unless --skip-test-agent is
# given, the script logs in to zOS with an email/password (prompted, or from
# SMOKE_TEST_EMAIL/SMOKE_TEST_PASSWORD) and caches the JWT, gitignored, in
# deploy/.smoke-session.jwt for reuse. A pre-minted SMOKE_TEST_TOKEN (env) or
# .secrets/SMOKE_TEST_TOKEN still takes precedence and skips the login.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

REF="${SWARM_R1_REF:-${R1_DEFAULT_REF}}"
SKIP_TEST_AGENT=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --ref) REF="$2"; shift 2 ;;
        --skip-test-agent) SKIP_TEST_AGENT=true; shift ;;
        --relogin) export SMOKE_FORCE_LOGIN=1; shift ;;
        *) echo "Unknown option: $1"; echo "Usage: $0 [--ref GIT_REF] [--skip-test-agent] [--relogin]"; exit 1 ;;
    esac
done

step_banner "07" "Deploy R1 (dual-mode) at ref ${REF}" "ops-admin"

require_cmds aws kubectl jq curl docker git openssl
require_aws_auth
ensure_kubectl_context

if [[ "${SKIP_TEST_AGENT}" != "true" ]]; then
    ensure_smoke_test_token \
        || step_fail "test-agent verification needs an owner session: log in when prompted, set SMOKE_TEST_EMAIL/SMOKE_TEST_PASSWORD, provide SMOKE_TEST_TOKEN, or pass --skip-test-agent"
fi

TMP_DIR=$(mktemp -d)
TEST_AGENT_ID=""
# The designated owner agent is intentionally PERSISTENT (step 08 reuses it), so
# cleanup never deletes it — it only tears down the port-forward + temp dir.
cleanup() {
    gw_stop_port_forward
    rm -rf "${TMP_DIR}"
}
trap cleanup EXIT

#------------------------------------------------------------------------------
# Pre-deploy snapshot of legacy (kata-fc) pods — must be untouched by R1
#------------------------------------------------------------------------------

PRE_LEGACY_JSON="${TMP_DIR}/pre-legacy-pods.json"
snapshot_agent_pods "${TMP_DIR}/pre-all-pods.json"
jq '[.[] | select(.runtime_class == "kata-fc")]' "${TMP_DIR}/pre-all-pods.json" > "${PRE_LEGACY_JSON}"
LEGACY_COUNT=$(jq 'length' "${PRE_LEGACY_JSON}")
log_info "Legacy (kata-fc) pods before deploy: ${LEGACY_COUNT}"
echo ""

#------------------------------------------------------------------------------
# Build + deploy at the R1 ref
#------------------------------------------------------------------------------

deploy_platform_at_ref "${REF}"
echo ""

#------------------------------------------------------------------------------
# Verify: /internal/health
#------------------------------------------------------------------------------

gw_start_port_forward || step_fail "gateway port-forward failed after deploy"
HEALTH=$(gw_internal_get "/internal/health" || echo "")
gw_stop_port_forward
[[ "$(echo "${HEALTH}" | jq -r '.status // empty')" == "ok" ]] \
    || step_fail "gateway /internal/health did not report ok (got: ${HEALTH:-no response})"
log_ok "Gateway /internal/health OK"

#------------------------------------------------------------------------------
# Verify: legacy pods untouched (R1 must not churn kata-fc pods)
#------------------------------------------------------------------------------

# Give the restarted scheduler a couple of reconcile passes to (not) act.
sleep 60
POST_LEGACY_JSON="${TMP_DIR}/post-legacy-pods.json"
snapshot_agent_pods "${TMP_DIR}/post-all-pods.json"
jq '[.[] | select(.runtime_class == "kata-fc")]' "${TMP_DIR}/post-all-pods.json" > "${POST_LEGACY_JSON}"

LEGACY_DIFF=$(jq -n \
    --slurpfile pre "${PRE_LEGACY_JSON}" --slurpfile post "${POST_LEGACY_JSON}" '
    ($pre[0] | map({key: .pod_name, value: {runtime_class, spec_hash}}) | from_entries) as $pre_map
    | ($post[0] | map({key: .pod_name, value: {runtime_class, spec_hash}}) | from_entries) as $post_map
    | {
        missing: ([$pre_map | keys[]] - [$post_map | keys[]]),
        changed: [ $pre_map | to_entries[] | select($post_map[.key] != null and $post_map[.key] != .value) | .key ]
      }')
LEGACY_MISSING=$(echo "${LEGACY_DIFF}" | jq -r '.missing | length')
LEGACY_CHANGED=$(echo "${LEGACY_DIFF}" | jq -r '.changed | length')

if [[ "${LEGACY_MISSING}" != "0" || "${LEGACY_CHANGED}" != "0" ]]; then
    echo "${LEGACY_DIFF}" | jq .
    step_fail "legacy kata-fc pods were touched by the R1 deploy (${LEGACY_MISSING} missing, ${LEGACY_CHANGED} spec-changed) — R1 must leave legacy agents unchanged"
fi
log_ok "All ${LEGACY_COUNT} legacy kata-fc pod(s) untouched (same pods, runtime class, spec hash)"

#------------------------------------------------------------------------------
# Verify: a confidential agent lands on kata-remote with sealed env + billing sku
#
# Creates (or reuses) the PERSISTENT designated owner agent and confirms it
# reaches Running. It is intentionally kept afterwards so step 08 can reuse it
# (continuity across the soak window). If it cannot spawn, ensure_owner_test_agent
# dumps root-cause diagnostics (pod events + scheduler error_message + CAA health).
#------------------------------------------------------------------------------

if [[ "${SKIP_TEST_AGENT}" == "true" ]]; then
    log_warn "Skipping test-agent verification (--skip-test-agent)"
else
    echo ""
    gw_start_port_forward || step_fail "gateway port-forward failed"

    ensure_owner_test_agent \
        || step_fail "designated owner agent '${SMOKE_AGENT_NAME}' never reached Running (root-cause diagnostics above)"

    POD_NAME=$(agent_pod_name "${TEST_AGENT_ID}")
    POD_JSON=$(kubectl get pod "${POD_NAME}" -n "${K8S_NAMESPACE_AGENTS}" -o json)
    RUNTIME_CLASS=$(echo "${POD_JSON}" | jq -r '.spec.runtimeClassName // "<none>"')
    [[ "${RUNTIME_CLASS}" == "kata-remote" ]] \
        || step_fail "test agent pod is on '${RUNTIME_CLASS}', expected kata-remote"
    log_ok "Test agent pod on kata-remote"

    SEALED=$(echo "${POD_JSON}" | jq -r '[.spec.containers[0].env[]? | select(.name == "AURA_STATE_ENCRYPTION") | .value] | first // ""')
    KBS_ENV=$(echo "${POD_JSON}" | jq -r '[.spec.containers[0].env[]? | select(.name == "AURA_KBS_URL") | .value] | first // ""')
    [[ "${SEALED}" == "sealed" ]] || step_fail "test agent pod missing AURA_STATE_ENCRYPTION=sealed (got '${SEALED}')"
    [[ -n "${KBS_ENV}" ]] || step_fail "test agent pod missing AURA_KBS_URL"
    log_ok "Test agent env: AURA_STATE_ENCRYPTION=sealed, AURA_KBS_URL=${KBS_ENV}"

    # Billing sku: the usage API must report tier/sku data for the new agent.
    USAGE=$(gw_user_api GET "/v1/agents/${TEST_AGENT_ID}/usage" || echo "")
    SKU_SEEN=$(echo "${USAGE}" | jq -r '[.. | objects | (.sku? // .tier?) | select(. != null)] | length' 2>/dev/null || echo "0")
    if [[ "${SKU_SEEN}" == "0" ]]; then
        step_fail "usage API shows no sku/tier for the test agent — tier billing not visible (response: ${USAGE:0:200})"
    fi
    log_ok "Billing sku/tier visible in the usage API"

    gw_stop_port_forward
    log_ok "Designated owner agent ${TEST_AGENT_ID} kept (step 08 reuses '${SMOKE_AGENT_NAME}')"
fi

step_ok "08 (./08-r1-soak-check.sh — run repeatedly during the soak window)"
