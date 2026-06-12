#!/bin/bash
# Diagnostic script for aura-swarm gateway
# Usage: ./diagnose.sh [GATEWAY_URL]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env" 2>/dev/null || true

GATEWAY_URL="${1:-http://ab6d2375031e74ce1976fdf62ea951a4-e757483aaffba396.elb.us-east-2.amazonaws.com}"
NAMESPACE="${K8S_NAMESPACE_SYSTEM:-swarm-system}"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

FAILURES=0

pass() { echo -e "${GREEN}✓${NC} $*"; }
fail() { echo -e "${RED}✗${NC} $*"; FAILURES=$((FAILURES + 1)); }
warn() { echo -e "${YELLOW}⚠${NC} $*"; }
info() { echo -e "${CYAN}→${NC} $*"; }
section() { echo -e "\n${CYAN}═══ $* ═══${NC}"; }

# ─────────────────────────────────────────────────────────────────────────────
section "1. Gateway health check"
# ─────────────────────────────────────────────────────────────────────────────
HEALTH=$(curl -sf "${GATEWAY_URL}/health" 2>&1) && {
    pass "GET /health → ${HEALTH}"
} || {
    fail "GET /health unreachable"
    echo "    Gateway is not responding. Check pod status and LoadBalancer."
}

# ─────────────────────────────────────────────────────────────────────────────
section "2. Agents endpoint (unauthenticated)"
# ─────────────────────────────────────────────────────────────────────────────
RESP=$(curl -s -w '\n__HTTP_CODE__%{http_code}' "${GATEWAY_URL}/v1/agents" 2>&1)
HTTP_CODE=$(echo "$RESP" | grep '__HTTP_CODE__' | sed 's/__HTTP_CODE__//')
BODY=$(echo "$RESP" | grep -v '__HTTP_CODE__')
info "HTTP ${HTTP_CODE}"
echo "    Body: ${BODY}"

if [ "$HTTP_CODE" = "401" ]; then
    pass "Unauthenticated request correctly rejected with 401"
elif [ "$HTTP_CODE" = "500" ]; then
    fail "500 even without auth — problem is NOT token-related"
    warn "Server-side error (RocksDB, config, or middleware panic)"
else
    warn "Unexpected status ${HTTP_CODE}"
fi

# ─────────────────────────────────────────────────────────────────────────────
section "3. Agents endpoint (with stored token)"
# ─────────────────────────────────────────────────────────────────────────────
CRED_FILE=""
if [ -n "${LOCALAPPDATA:-}" ]; then
    CRED_FILE="${LOCALAPPDATA}/aura-swarm/credentials.json"
elif [ -d "${HOME}/Library/Application Support" ]; then
    CRED_FILE="${HOME}/Library/Application Support/aura-swarm/credentials.json"
else
    CRED_FILE="${HOME}/.local/share/aura-swarm/credentials.json"
fi

TOKEN=""
if [ -f "$CRED_FILE" ]; then
    TOKEN=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1]))['access_token'])" "$CRED_FILE" 2>/dev/null || \
            python  -c "import json,sys; print(json.load(open(sys.argv[1]))['access_token'])" "$CRED_FILE" 2>/dev/null || true)
fi

if [ -n "$TOKEN" ]; then
    pass "Found stored token at ${CRED_FILE}"
    info "Token preview: ${TOKEN:0:20}..."

    RESP=$(curl -s -w '\n__HTTP_CODE__%{http_code}' \
        -H "Authorization: Bearer ${TOKEN}" \
        "${GATEWAY_URL}/v1/agents" 2>&1)
    HTTP_CODE=$(echo "$RESP" | grep '__HTTP_CODE__' | sed 's/__HTTP_CODE__//')
    BODY=$(echo "$RESP" | grep -v '__HTTP_CODE__')
    info "HTTP ${HTTP_CODE}"
    echo "    Body: ${BODY}"

    case "$HTTP_CODE" in
        200) pass "Agents endpoint working!" ;;
        401) fail "Token rejected — re-run: aswarm login" ;;
        402) fail "Insufficient credits or billing account not found" ;;
        500) fail "500 with valid auth — server-side issue (see logs below)" ;;
        *)   warn "Unexpected status ${HTTP_CODE}" ;;
    esac
else
    warn "No stored credentials found at ${CRED_FILE}"
    info "Run: aswarm login"
fi

# ─────────────────────────────────────────────────────────────────────────────
section "4. Kubernetes pod status"
# ─────────────────────────────────────────────────────────────────────────────
if command -v kubectl &>/dev/null; then
    echo ""
    info "Pods in ${NAMESPACE}:"
    kubectl get pods -n "${NAMESPACE}" -o wide 2>&1 | sed 's/^/    /'
    echo ""

    GATEWAY_POD=$(kubectl get pods -n "${NAMESPACE}" -l app=aura-swarm-gateway -o jsonpath='{.items[0].metadata.name}' 2>/dev/null || true)

    if [ -n "$GATEWAY_POD" ]; then
        info "Gateway pod: ${GATEWAY_POD}"

        PHASE=$(kubectl get pod -n "${NAMESPACE}" "$GATEWAY_POD" -o jsonpath='{.status.phase}' 2>/dev/null || true)
        READY=$(kubectl get pod -n "${NAMESPACE}" "$GATEWAY_POD" -o jsonpath='{.status.containerStatuses[0].ready}' 2>/dev/null || true)
        RESTARTS=$(kubectl get pod -n "${NAMESPACE}" "$GATEWAY_POD" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null || true)

        [ "$PHASE" = "Running" ] && pass "Phase: Running" || fail "Phase: ${PHASE}"
        [ "$READY" = "true" ] && pass "Ready: true" || fail "Ready: ${READY}"
        [ "${RESTARTS:-0}" -lt 3 ] && pass "Restarts: ${RESTARTS}" || warn "Restarts: ${RESTARTS} (possible crash loop)"
    else
        fail "No gateway pod found in ${NAMESPACE}"
    fi
else
    warn "kubectl not available — skipping pod checks"
fi

# ─────────────────────────────────────────────────────────────────────────────
section "5. Gateway pod logs (last 80 lines)"
# ─────────────────────────────────────────────────────────────────────────────
if command -v kubectl &>/dev/null && [ -n "${GATEWAY_POD:-}" ]; then
    echo ""
    kubectl logs -n "${NAMESPACE}" "$GATEWAY_POD" --tail=80 2>&1 | sed 's/^/    /'
    echo ""

    info "Grepping for errors..."
    ERR_LINES=$(kubectl logs -n "${NAMESPACE}" "$GATEWAY_POD" --tail=200 2>&1 | grep -iE 'error|panic|500|internal|failed' | grep -iv 'INFO' || true)
    if [ -n "$ERR_LINES" ]; then
        fail "Error lines found:"
        echo "$ERR_LINES" | sed 's/^/    /'
    else
        pass "No obvious errors in recent logs"
    fi
else
    warn "Skipping log collection (no kubectl or no pod)"
fi

# ─────────────────────────────────────────────────────────────────────────────
section "6. zOS connectivity from gateway pod"
# ─────────────────────────────────────────────────────────────────────────────
if command -v kubectl &>/dev/null && [ -n "${GATEWAY_POD:-}" ]; then
    info "Testing if gateway can reach zOS user-info API..."

    ZOS_TEST=$(kubectl exec -n "${NAMESPACE}" "$GATEWAY_POD" -- \
        sh -c 'if command -v curl >/dev/null 2>&1; then
            curl -sf -o /dev/null -w "%{http_code}" --connect-timeout 5 https://zosapi.zero.tech/api/users/current 2>&1
        else
            echo "NO_CURL"
        fi' 2>&1) || ZOS_TEST="EXEC_FAILED"

    case "$ZOS_TEST" in
        401)   pass "zOS reachable (returned 401 — expected without token)" ;;
        200)   pass "zOS reachable (returned 200)" ;;
        NO_CURL)
            info "No curl in container — testing via ephemeral pod..."
            ZOS_TEST2=$(kubectl run -n "${NAMESPACE}" diag-curl-$$ --rm -i --restart=Never \
                --image=curlimages/curl:latest -- \
                curl -sf -o /dev/null -w "%{http_code}" --connect-timeout 5 \
                https://zosapi.zero.tech/api/users/current 2>&1 | tail -1) || ZOS_TEST2="FAILED"
            case "$ZOS_TEST2" in
                401|200) pass "zOS reachable from cluster (HTTP ${ZOS_TEST2})" ;;
                *)       fail "Cannot reach zOS from cluster: ${ZOS_TEST2}" ;;
            esac
            ;;
        EXEC_FAILED) warn "Could not exec into pod (readOnlyRootFilesystem)" ;;
        *)     fail "zOS unreachable or unexpected response: ${ZOS_TEST}" ;;
    esac
else
    warn "Skipping connectivity test"
fi

# ─────────────────────────────────────────────────────────────────────────────
section "7. Gateway environment & config"
# ─────────────────────────────────────────────────────────────────────────────
if command -v kubectl &>/dev/null && [ -n "${GATEWAY_POD:-}" ]; then
    info "ConfigMap values:"
    kubectl get configmap -n "${NAMESPACE}" aura-swarm-config -o yaml 2>&1 | grep -E '^\s+(SCHEDULER_URL|GATEWAY_URL|Z_BILLING|RUST_LOG|DEFAULT_ISOLATION|AUTH)' | sed 's/^/    /'
    echo ""

    info "Container env (non-secret):"
    kubectl get pod -n "${NAMESPACE}" "$GATEWAY_POD" -o jsonpath='{range .spec.containers[0].env[*]}{.name}={.value}{"\n"}{end}' 2>/dev/null | grep -v 'API_KEY' | sed 's/^/    /'
fi

# ─────────────────────────────────────────────────────────────────────────────
section "8. Services & endpoints"
# ─────────────────────────────────────────────────────────────────────────────
if command -v kubectl &>/dev/null; then
    info "Services:"
    kubectl get svc -n "${NAMESPACE}" 2>&1 | sed 's/^/    /'
    echo ""
    info "Endpoints for gateway:"
    kubectl get endpoints -n "${NAMESPACE}" aura-swarm-gateway 2>&1 | sed 's/^/    /'
fi

# ─────────────────────────────────────────────────────────────────────────────
section "Summary"
# ─────────────────────────────────────────────────────────────────────────────
echo ""
if [ "$FAILURES" -eq 0 ]; then
    pass "All checks passed! (${FAILURES} failures)"
else
    fail "${FAILURES} check(s) failed"
    echo ""
    info "Common causes:"
    echo "  1. Token expired or invalid — re-run: aswarm login"
    echo "  2. zOS unreachable from cluster — check DNS/egress/network policies"
    echo "  3. RocksDB permission error on /data/aura-swarm"
    echo "  4. zbilling fail-closed — Z_BILLING_FAIL_CLOSED=true but no valid API key"
fi
echo ""
