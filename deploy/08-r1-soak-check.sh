#!/bin/bash
# 08-r1-soak-check.sh - Repeatable R1 soak verification. Run this as often as
# you like during the R1 prod soak window; it is READ-ONLY against production
# agents and mutates only the designated owner test agent (persistent: created
# and kept by step 07, reused here by name; deleted only with --ephemeral).
#
# Checks (printed as a pass/fail checklist at the end):
#   platform-health   deployments ready + /internal/health ok
#   fleet-read-only   tiered agents on kata-remote, legacy agents on
#                     kata-fc, no error-state regressions
#   test-agent        designated owner agent lands on kata-remote with sealed
#                     env and a Ready (serving) harness; on failure, root-cause
#                     diagnostics are dumped
#   vault             PUT / GET(reveal) / LIST / DELETE secrets round-trip
#   tier-change       standard -> pro -> standard with pod recreate
#   cron-cycle        process registration + hibernate -> cron/wake -> run
#   usage-api         per-agent + per-user usage respond with data
#   logs-api          live log tail returns entries
#
# Usage:
#   ./08-r1-soak-check.sh                  # reuse/create the designated owner agent (kept)
#   ./08-r1-soak-check.sh --agent-id ID    # target a specific agent instead
#   ./08-r1-soak-check.sh --keep-agent     # (default behavior) keep the agent running
#   ./08-r1-soak-check.sh --ephemeral      # delete the agent on exit (old throwaway mode)
#   ./08-r1-soak-check.sh --relogin        # force a fresh zOS login (ignore cache)
#
# Requires an owner session. The script logs in to zOS with an email/password
# (prompted, or from SMOKE_TEST_EMAIL/SMOKE_TEST_PASSWORD) and caches the JWT,
# gitignored, in deploy/.smoke-session.jwt — so repeated soak runs reuse one
# login until it expires. A pre-minted SMOKE_TEST_TOKEN (env) or
# .secrets/SMOKE_TEST_TOKEN still takes precedence and skips the login.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

TEST_AGENT_ID=""
KEEP_AGENT=false
EPHEMERAL=false
while [[ $# -gt 0 ]]; do
    case "$1" in
        --agent-id) TEST_AGENT_ID="$2"; KEEP_AGENT=true; shift 2 ;;
        --keep-agent) KEEP_AGENT=true; shift ;;
        --ephemeral) EPHEMERAL=true; shift ;;
        --relogin) export SMOKE_FORCE_LOGIN=1; shift ;;
        *) echo "Unknown option: $1"; echo "Usage: $0 [--agent-id ID] [--keep-agent] [--ephemeral] [--relogin]"; exit 1 ;;
    esac
done

step_banner "08" "R1 soak check (repeatable)" "ops-admin"

require_cmds aws kubectl jq curl
require_aws_auth
ensure_kubectl_context

ensure_smoke_test_token \
    || step_fail "owner session required: log in when prompted, set SMOKE_TEST_EMAIL/SMOKE_TEST_PASSWORD, or provide SMOKE_TEST_TOKEN / .secrets/SMOKE_TEST_TOKEN"

declare -a CHECK_NAMES=()
declare -a CHECK_RESULTS=()
declare -a CHECK_NOTES=()

record() { # record <name> <PASS|FAIL|SKIP> <note>
    CHECK_NAMES+=("$1")
    CHECK_RESULTS+=("$2")
    CHECK_NOTES+=("${3:-}")
    case "$2" in
        PASS) log_ok "$1 ${3:-}" ;;
        SKIP) log_warn "$1 skipped ${3:-}" ;;
        *)    log_err "$1 ${3:-}" ;;
    esac
}

# The designated owner agent persists by default (it is reused across runs and
# was created/kept by step 07). Only --ephemeral deletes it on exit.
cleanup() {
    if [[ "${EPHEMERAL}" == "true" && -n "${TEST_AGENT_ID}" ]]; then
        gw_user_api DELETE "/v1/agents/${TEST_AGENT_ID}" >/dev/null 2>&1 || true
    fi
    gw_stop_port_forward
}
trap cleanup EXIT

# Thin wrappers over the shared _lib.sh helpers, bound to this run's
# TEST_AGENT_ID. wait_test_agent_running now logs progress and bails early when
# the agent flips to error (see wait_agent_running in _lib.sh).
test_agent_pod() { agent_pod_name "${TEST_AGENT_ID}"; }
wait_test_agent_running() { wait_agent_running "${TEST_AGENT_ID}" "${1:-600}"; }

#------------------------------------------------------------------------------
# platform-health
#------------------------------------------------------------------------------

log_section "[1/8] platform-health"
HEALTH_OK=true
for d in aura-swarm-gateway aura-swarm-control aura-swarm-scheduler; do
    READY=$(kubectl get deployment "${d}" -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.status.readyReplicas}' 2>/dev/null || echo "0")
    [[ "${READY:-0}" -ge 1 ]] || { HEALTH_OK=false; log_detail "${d}: not ready"; }
done
if gw_start_port_forward && [[ "$(gw_internal_get /internal/health | jq -r '.status')" == "ok" ]]; then
    :
else
    HEALTH_OK=false
fi
if [[ "${HEALTH_OK}" == "true" ]]; then
    record "platform-health" PASS
else
    record "platform-health" FAIL "deployment or /internal/health unhealthy"
fi

#------------------------------------------------------------------------------
# fleet-read-only (no mutations: records + pod runtime classes)
#------------------------------------------------------------------------------

log_section "[2/8] fleet-read-only"
ALL_AGENTS=$(gw_internal_get "/internal/agents/all" 2>/dev/null || echo "[]")
PODS_JSON=$(mktemp)
snapshot_agent_pods "${PODS_JSON}"

TIERED=$(echo "${ALL_AGENTS}" | jq '[.[] | select(.spec.tier != null)] | length')
LEGACY=$(echo "${ALL_AGENTS}" | jq '[.[] | select(.spec.tier == null)] | length')
ERRORED=$(echo "${ALL_AGENTS}" | jq '[.[] | select(.status == "error")] | length')
BAD_PODS=$(jq '[.[] | select(.runtime_class != "kata-remote" and .runtime_class != "kata-fc")] | length' "${PODS_JSON}")
log_detail "Agents: ${TIERED} tiered (TEE), ${LEGACY} legacy, ${ERRORED} in error"
jq -r 'group_by(.runtime_class) | .[] | "pods on \(.[0].runtime_class): \(length)"' "${PODS_JSON}" \
    | while IFS= read -r line; do log_detail "${line}"; done
rm -f "${PODS_JSON}"
if [[ "${ERRORED}" == "0" && "${BAD_PODS}" == "0" ]]; then
    record "fleet-read-only" PASS "${TIERED} tiered / ${LEGACY} legacy"
else
    record "fleet-read-only" FAIL "${ERRORED} error-state agent(s), ${BAD_PODS} pod(s) on unexpected runtime class"
fi

#------------------------------------------------------------------------------
# test-agent (create on SNP, sealed)
#------------------------------------------------------------------------------

log_section "[3/8] test-agent"
# Reuse the designated owner agent by default (step 07 creates+keeps it); an
# explicit --agent-id overrides. ensure_owner_test_agent discovers it by name,
# creates+keeps one if missing, waits for Running+Ready (harness serving
# /health), and on failure dumps root-cause diagnostics (pod events +
# scheduler error_message + CAA health).
# The gateway port-forward from [1/8] is still up for the user-API calls.
AGENT_UP=false
if [[ -n "${TEST_AGENT_ID}" ]]; then
    if wait_agent_running "${TEST_AGENT_ID}" 600; then
        AGENT_UP=true
    else
        diagnose_test_agent "${TEST_AGENT_ID}"
    fi
elif ensure_owner_test_agent; then
    AGENT_UP=true
fi

if [[ -z "${TEST_AGENT_ID}" ]]; then
    record "test-agent" FAIL "agent creation failed"
elif [[ "${AGENT_UP}" == "true" ]]; then
    POD=$(test_agent_pod)
    RC=$(kubectl get pod "${POD}" -n "${K8S_NAMESPACE_AGENTS}" -o jsonpath='{.spec.runtimeClassName}')
    SEALED=$(kubectl get pod "${POD}" -n "${K8S_NAMESPACE_AGENTS}" -o json \
        | jq -r '[.spec.containers[0].env[]? | select(.name=="AURA_STATE_ENCRYPTION") | .value] | first // ""')
    # The harness must be serving (container Ready / readiness probe passing on
    # /health), not just the pod phase Running — otherwise every harness-proxied
    # check below (vault/logs/cron) fails against a pod that only looks alive.
    # wait_agent_running already gates on this; assert it here so the verdict is
    # explicit even when an externally-supplied --agent-id is reused.
    READY=$(kubectl get pod "${POD}" -n "${K8S_NAMESPACE_AGENTS}" \
        -o jsonpath='{.status.containerStatuses[0].ready}' 2>/dev/null || echo "")
    if [[ "${RC}" == "kata-remote" && "${SEALED}" == "sealed" && "${READY}" == "true" ]]; then
        record "test-agent" PASS "agent ${TEST_AGENT_ID} on kata-remote, sealed, harness Ready"
    else
        record "test-agent" FAIL "runtime_class=${RC} sealed=${SEALED} ready=${READY:-?}"
    fi
else
    record "test-agent" FAIL "agent ${TEST_AGENT_ID} never reached Running+Ready (diagnostics above)"
fi

#------------------------------------------------------------------------------
# vault (secrets round-trip on the test agent only)
#------------------------------------------------------------------------------

log_section "[4/8] vault"
if [[ -n "${TEST_AGENT_ID}" ]]; then
    VAULT_OK=true
    SECRET_VAL="soak-$(date +%s)"
    gw_user_api PUT "/v1/agents/${TEST_AGENT_ID}/secrets/soak_check" \
        "{\"value\": \"${SECRET_VAL}\"}" >/dev/null 2>&1 || VAULT_OK=false
    GOT=$(gw_user_api GET "/v1/agents/${TEST_AGENT_ID}/secrets/soak_check?reveal=true" 2>/dev/null \
        | jq -r '.value // empty' || echo "")
    [[ "${GOT}" == "${SECRET_VAL}" ]] || VAULT_OK=false
    LISTED=$(gw_user_api GET "/v1/agents/${TEST_AGENT_ID}/secrets" 2>/dev/null \
        | jq '[.. | strings | select(. == "soak_check")] | length' || echo "0")
    [[ "${LISTED}" != "0" ]] || VAULT_OK=false
    gw_user_api DELETE "/v1/agents/${TEST_AGENT_ID}/secrets/soak_check" >/dev/null 2>&1 || VAULT_OK=false
    if [[ "${VAULT_OK}" == "true" ]]; then
        record "vault" PASS "PUT/GET(reveal)/LIST/DELETE round-trip"
    else
        record "vault" FAIL "secrets round-trip failed (value match: '${GOT}' vs '${SECRET_VAL}')"
    fi
else
    record "vault" SKIP "no test agent"
fi

#------------------------------------------------------------------------------
# tier-change (standard -> pro -> standard, pod recreate each way)
#------------------------------------------------------------------------------

log_section "[5/8] tier-change"
if [[ -n "${TEST_AGENT_ID}" ]]; then
    TIER_OK=true
    # The observable, runtime-class-agnostic effect of a tier change is a pod
    # RECREATE that converges back to Running+Ready. For confidential agents the
    # tier's cpu/memory does NOT show up as a pod resource request — the peer-pods
    # webhook strips cpu/memory and swaps in the kata.peerpods.io/vm extended
    # resource (the real sizing lives in the off-cluster pod VM). So assert the
    # pod was recreated each way (name changes) rather than reading a stripped
    # cpu request, and only fall back to the cpu-request check for container
    # (dev-mode) pods that keep their literal requests.
    POD_BEFORE="$(test_agent_pod)"
    RC=$(kubectl get pod "${POD_BEFORE}" -n "${K8S_NAMESPACE_AGENTS}" \
        -o jsonpath='{.spec.runtimeClassName}' 2>/dev/null || echo "")
    gw_user_api POST "/v1/agents/${TEST_AGENT_ID}/tier" '{"tier": "pro"}' >/dev/null 2>&1 || TIER_OK=false
    if [[ "${TIER_OK}" == "true" ]] && wait_test_agent_running 600; then
        POD_PRO="$(test_agent_pod)"
        # Pod must have been recreated (new pod) for the pro tier.
        [[ -n "${POD_PRO}" && "${POD_PRO}" != "${POD_BEFORE}" ]] || TIER_OK=false
        if [[ "${RC}" != "kata-remote" ]]; then
            # Container (dev) pods keep literal requests; verify the bump landed.
            CPU=$(kubectl get pod "${POD_PRO}" -n "${K8S_NAMESPACE_AGENTS}" \
                -o jsonpath='{.spec.containers[0].resources.requests.cpu}' 2>/dev/null || echo "")
            [[ "${CPU}" == "2" || "${CPU}" == "2000m" ]] || TIER_OK=false
        fi
        gw_user_api POST "/v1/agents/${TEST_AGENT_ID}/tier" '{"tier": "standard"}' >/dev/null 2>&1 || TIER_OK=false
        if wait_test_agent_running 600; then
            POD_STD="$(test_agent_pod)"
            [[ -n "${POD_STD}" && "${POD_STD}" != "${POD_PRO}" ]] || TIER_OK=false
        else
            TIER_OK=false
        fi
    else
        TIER_OK=false
    fi
    if [[ "${TIER_OK}" == "true" ]]; then
        record "tier-change" PASS "standard -> pro -> standard, pod recreated + reconverged each way"
    else
        record "tier-change" FAIL "tier change or pod recreate did not converge"
    fi
else
    record "tier-change" SKIP "no test agent"
fi

#------------------------------------------------------------------------------
# cron-cycle (process registration + hibernate -> wake -> run)
#
# A process is created through the harness API on the test agent's pod
# (pod port-forward; the gateway only stores trigger metadata). Then the
# agent is hibernated and we wait for the ProcessCronService to wake it
# and fire the trigger (cron "* * * * *").
#------------------------------------------------------------------------------

log_section "[6/8] cron-cycle"
if [[ -n "${TEST_AGENT_ID}" ]] && [[ -n "$(test_agent_pod)" ]]; then
    CRON_OK=true
    POD=$(test_agent_pod)
    HARNESS_PORT="${AURA_HARNESS_PORT:-8080}"
    LOCAL_PORT=$((19080 + RANDOM % 1000))
    kubectl port-forward -n "${K8S_NAMESPACE_AGENTS}" "pod/${POD}" \
        "${LOCAL_PORT}:${HARNESS_PORT}" >/dev/null 2>&1 &
    PF_PID=$!
    sleep 3
    PROC_RESP=$(curl -fsS -X POST -H "Authorization: Bearer ${SMOKE_TEST_TOKEN}" \
        -H "Content-Type: application/json" \
        -d '{"name": "soak-cron", "cron": "* * * * *", "prompt": "echo soak", "enabled": true}' \
        "http://127.0.0.1:${LOCAL_PORT}/v1/processes" 2>/dev/null || echo "")
    PROC_ID=$(echo "${PROC_RESP}" | jq -r '.process_id // .id // empty' 2>/dev/null || echo "")
    kill "${PF_PID}" 2>/dev/null || true
    wait "${PF_PID}" 2>/dev/null || true

    if [[ -z "${PROC_ID}" ]]; then
        record "cron-cycle" FAIL "process creation via harness API failed (resp: ${PROC_RESP:0:120})"
    else
        # Trigger metadata must reach the gateway.
        REGISTERED=$(gw_user_api GET "/v1/agents/${TEST_AGENT_ID}/process-triggers" 2>/dev/null \
            | jq --arg id "${PROC_ID}" '[.. | objects | select(.process_id? == $id)] | length' || echo "0")
        [[ "${REGISTERED}" != "0" ]] || CRON_OK=false

        # Hibernate, then wait for the cron service to wake the agent.
        gw_user_api POST "/v1/agents/${TEST_AGENT_ID}/hibernate" >/dev/null 2>&1 || CRON_OK=false
        ELAPSED=0
        while [[ -n "$(test_agent_pod)" && ${ELAPSED} -le 180 ]]; do
            sleep 10; ELAPSED=$((ELAPSED + 10))
        done
        [[ -z "$(test_agent_pod)" ]] || CRON_OK=false

        WOKE=false
        ELAPSED=0
        while [[ ${ELAPSED} -le 300 ]]; do
            if [[ -n "$(test_agent_pod)" ]]; then WOKE=true; break; fi
            sleep 15; ELAPSED=$((ELAPSED + 15))
        done
        [[ "${WOKE}" == "true" ]] || CRON_OK=false

        LAST_RUN=$(gw_user_api GET "/v1/agents/${TEST_AGENT_ID}/process-triggers" 2>/dev/null \
            | jq -r --arg id "${PROC_ID}" '[.. | objects | select(.process_id? == $id) | .last_run_at] | first // empty' || echo "")

        if [[ "${CRON_OK}" == "true" && -n "${LAST_RUN}" ]]; then
            record "cron-cycle" PASS "registered, hibernated, cron-woke, last_run_at=${LAST_RUN}"
        elif [[ "${CRON_OK}" == "true" ]]; then
            record "cron-cycle" FAIL "agent woke but last_run_at not set for process ${PROC_ID}"
        else
            record "cron-cycle" FAIL "registration/hibernate/wake cycle did not complete (registered=${REGISTERED}, woke=${WOKE})"
        fi
    fi
else
    record "cron-cycle" SKIP "no running test agent"
fi

#------------------------------------------------------------------------------
# usage-api
#------------------------------------------------------------------------------

log_section "[7/8] usage-api"
if [[ -n "${TEST_AGENT_ID}" ]]; then
    AGENT_USAGE=$(gw_user_api GET "/v1/agents/${TEST_AGENT_ID}/usage" 2>/dev/null || echo "")
    USER_USAGE=$(gw_user_api GET "/v1/usage" 2>/dev/null || echo "")
    EVENTS=$(echo "${AGENT_USAGE}" | jq '[.. | objects | select(has("sku") or has("tier"))] | length' 2>/dev/null || echo "0")
    if [[ -n "${AGENT_USAGE}" && -n "${USER_USAGE}" && "${EVENTS}" != "0" ]]; then
        record "usage-api" PASS "agent + user usage respond; sku/tier data present"
    else
        record "usage-api" FAIL "usage endpoints empty or missing sku/tier data"
    fi
else
    record "usage-api" SKIP "no test agent"
fi

#------------------------------------------------------------------------------
# logs-api
#------------------------------------------------------------------------------

log_section "[8/8] logs-api"
if [[ -n "${TEST_AGENT_ID}" ]]; then
    LOGS=$(gw_user_api GET "/v1/agents/${TEST_AGENT_ID}/logs?tail=50" 2>/dev/null || echo "")
    LOG_COUNT=$(echo "${LOGS}" | jq '[.. | objects | select(has("source") or has("line") or has("message"))] | length' 2>/dev/null || echo "0")
    if [[ "${LOG_COUNT}" != "0" ]]; then
        record "logs-api" PASS "${LOG_COUNT} log entrie(s) returned"
    else
        record "logs-api" FAIL "logs endpoint returned no entries (resp: ${LOGS:0:120})"
    fi
else
    record "logs-api" SKIP "no test agent"
fi

#------------------------------------------------------------------------------
# Checklist + verdict
#------------------------------------------------------------------------------

log_section "R1 Soak Checklist"
FAILS=0
for i in "${!CHECK_NAMES[@]}"; do
    case "${CHECK_RESULTS[$i]}" in
        PASS) echo -e "  ${GREEN}PASS${NC}  ${CHECK_NAMES[$i]}  ${CHECK_NOTES[$i]}" ;;
        SKIP) echo -e "  ${YELLOW}SKIP${NC}  ${CHECK_NAMES[$i]}  ${CHECK_NOTES[$i]}" ;;
        *)    echo -e "  ${RED}FAIL${NC}  ${CHECK_NAMES[$i]}  ${CHECK_NOTES[$i]}"; FAILS=$((FAILS + 1)) ;;
    esac
done
echo ""

if [[ "${EPHEMERAL}" != "true" && -n "${TEST_AGENT_ID}" ]]; then
    log_info "Designated owner agent kept: ${TEST_AGENT_ID} (name '${SMOKE_AGENT_NAME}')"
    log_detail "Reused automatically next run; target explicitly with --agent-id ${TEST_AGENT_ID}, or --ephemeral to delete on exit."
    echo ""
fi

if [[ ${FAILS} -gt 0 ]]; then
    step_fail "${FAILS} soak check(s) failed"
fi
step_ok "09 (./09-efs-backup.sh) once the soak window is over — or re-run 08 during the soak"
