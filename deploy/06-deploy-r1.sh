#!/bin/bash
# 06-deploy-r1.sh - Build, push and deploy gateway/control/scheduler at the
# R1 (dual-mode) ref. Every NEW agent becomes a confidential SNP VM with
# sealed storage; legacy agents must remain byte-for-byte untouched.
#
# Verifies: rollouts complete, /internal/health OK, legacy agent pods
# untouched (same pods, same runtime class, same spec hash), and one new
# test agent lands on kata-remote with a sealed env and a billing sku.
#
# Usage:
#   ./06-deploy-r1.sh                 # deploys SWARM_R1_REF (default fa93895)
#   ./06-deploy-r1.sh --ref <git-ref>
#   ./06-deploy-r1.sh --skip-test-agent
#
# The test-agent check needs an owner JWT in SMOKE_TEST_TOKEN (env) or
# .secrets/SMOKE_TEST_TOKEN.

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
        *) echo "Unknown option: $1"; echo "Usage: $0 [--ref GIT_REF] [--skip-test-agent]"; exit 1 ;;
    esac
done

step_banner "06" "Deploy R1 (dual-mode) at ref ${REF}"

require_cmds aws kubectl jq curl docker git openssl
require_aws_auth
ensure_kubectl_context

SMOKE_TEST_TOKEN="${SMOKE_TEST_TOKEN:-$(load_secret SMOKE_TEST_TOKEN)}"
if [[ "${SKIP_TEST_AGENT}" != "true" && -z "${SMOKE_TEST_TOKEN}" ]]; then
    step_fail "test-agent verification needs an owner JWT: set SMOKE_TEST_TOKEN or create .secrets/SMOKE_TEST_TOKEN (or pass --skip-test-agent)"
fi

TMP_DIR=$(mktemp -d)
TEST_AGENT_ID=""
cleanup() {
    if [[ -n "${TEST_AGENT_ID}" ]]; then
        gw_user_api DELETE "/v1/agents/${TEST_AGENT_ID}" >/dev/null 2>&1 || true
    fi
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
echo "Legacy (kata-fc) pods before deploy: ${LEGACY_COUNT}"
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
echo -e "${GREEN}✓${NC} Gateway /internal/health OK"

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
echo -e "${GREEN}✓${NC} All ${LEGACY_COUNT} legacy kata-fc pod(s) untouched (same pods, runtime class, spec hash)"

#------------------------------------------------------------------------------
# Verify: one new test agent lands on SNP with sealed env + billing sku
#------------------------------------------------------------------------------

if [[ "${SKIP_TEST_AGENT}" == "true" ]]; then
    echo -e "${YELLOW}⚠${NC} Skipping test-agent verification (--skip-test-agent)"
else
    echo ""
    echo "Creating R1 test agent..."
    gw_start_port_forward || step_fail "gateway port-forward failed"

    CREATE_RESP=$(gw_user_api POST "/v1/agents" \
        '{"name": "r1-deploy-smoke", "tier": "standard"}') \
        || step_fail "test agent creation failed (check SMOKE_TEST_TOKEN)"
    TEST_AGENT_ID=$(echo "${CREATE_RESP}" | jq -r '.agent_id // .id // empty')
    [[ -n "${TEST_AGENT_ID}" ]] || step_fail "could not parse agent id from create response: ${CREATE_RESP}"
    echo "  Test agent: ${TEST_AGENT_ID}"

    # Wait for its pod and inspect runtime class + sealed-env injection.
    POD_NAME=""
    ELAPSED=0
    while [[ ${ELAPSED} -le 600 ]]; do
        POD_NAME=$(kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" \
            -l "app=swarm-agent,swarm.io/agent-id=${TEST_AGENT_ID}" \
            -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || echo "")
        if [[ -n "${POD_NAME}" ]]; then
            PHASE=$(kubectl get pod "${POD_NAME}" -n "${K8S_NAMESPACE_AGENTS}" \
                -o jsonpath='{.status.phase}' 2>/dev/null || echo "")
            [[ "${PHASE}" == "Running" ]] && break
        fi
        echo "  [${ELAPSED}s] waiting for test agent pod (pod=${POD_NAME:-none})"
        sleep 15
        ELAPSED=$((ELAPSED + 15))
    done
    [[ -n "${POD_NAME}" ]] || step_fail "test agent pod never appeared"

    POD_JSON=$(kubectl get pod "${POD_NAME}" -n "${K8S_NAMESPACE_AGENTS}" -o json)
    RUNTIME_CLASS=$(echo "${POD_JSON}" | jq -r '.spec.runtimeClassName // "<none>"')
    [[ "${RUNTIME_CLASS}" == "kata-remote" ]] \
        || step_fail "test agent pod is on '${RUNTIME_CLASS}', expected kata-remote"
    echo -e "${GREEN}✓${NC} Test agent pod on kata-remote"

    SEALED=$(echo "${POD_JSON}" | jq -r '[.spec.containers[0].env[]? | select(.name == "AURA_STATE_ENCRYPTION") | .value] | first // ""')
    KBS_ENV=$(echo "${POD_JSON}" | jq -r '[.spec.containers[0].env[]? | select(.name == "AURA_KBS_URL") | .value] | first // ""')
    [[ "${SEALED}" == "sealed" ]] || step_fail "test agent pod missing AURA_STATE_ENCRYPTION=sealed (got '${SEALED}')"
    [[ -n "${KBS_ENV}" ]] || step_fail "test agent pod missing AURA_KBS_URL"
    echo -e "${GREEN}✓${NC} Test agent env: AURA_STATE_ENCRYPTION=sealed, AURA_KBS_URL=${KBS_ENV}"

    # Billing sku: the usage API must report tier/sku data for the new agent.
    USAGE=$(gw_user_api GET "/v1/agents/${TEST_AGENT_ID}/usage" || echo "")
    SKU_SEEN=$(echo "${USAGE}" | jq -r '[.. | objects | (.sku? // .tier?) | select(. != null)] | length' 2>/dev/null || echo "0")
    if [[ "${SKU_SEEN}" == "0" ]]; then
        step_fail "usage API shows no sku/tier for the test agent — tier billing not visible (response: ${USAGE:0:200})"
    fi
    echo -e "${GREEN}✓${NC} Billing sku/tier visible in the usage API"

    echo "  Destroying test agent..."
    gw_user_api DELETE "/v1/agents/${TEST_AGENT_ID}" >/dev/null \
        && TEST_AGENT_ID=""
    gw_stop_port_forward
    echo -e "${GREEN}✓${NC} Test agent cleaned up"
fi

step_ok "07 (./07-r1-soak-check.sh — run repeatedly during the soak window)"
