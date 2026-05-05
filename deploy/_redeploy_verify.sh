#!/bin/bash
# Shared strict redeploy verification helpers.
#
# Source after config.env. Callers should set -euo pipefail.

RED="${RED:-\033[0;31m}"
GREEN="${GREEN:-\033[0;32m}"
YELLOW="${YELLOW:-\033[1;33m}"
CYAN="${CYAN:-\033[0;36m}"
NC="${NC:-\033[0m}"

PORT_FORWARD_PID="${PORT_FORWARD_PID:-}"
PORT_FORWARD_PORT="${PORT_FORWARD_PORT:-}"
PORT_FORWARD_LOG="${PORT_FORWARD_LOG:-}"

redeploy_require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo -e "${RED}✗${NC} Missing required command: $1"
        exit 1
    fi
}

redeploy_require_tools() {
    local cmd
    for cmd in kubectl jq curl base64; do
        redeploy_require_command "$cmd"
    done
}

redeploy_internal_token() {
    if [[ -n "${INTERNAL_TOKEN:-}" ]]; then
        printf '%s' "${INTERNAL_TOKEN}"
        return 0
    fi

    local encoded
    encoded=$(kubectl get secret aura-swarm-secrets -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.data.INTERNAL_TOKEN}' 2>/dev/null || true)
    if [[ -z "${encoded}" ]]; then
        return 1
    fi

    printf '%s' "${encoded}" | base64 --decode
}

redeploy_internal_get() {
    local endpoint="$1"
    local token

    if ! token="$(redeploy_internal_token)"; then
        echo -e "${RED}✗${NC} Missing INTERNAL_TOKEN for gateway internal API."
        echo "    Create .secrets/INTERNAL_TOKEN or ensure the swarm-system/aura-swarm-secrets secret exists."
        return 1
    fi

    curl -fsS -H "Authorization: Bearer ${token}" \
        "http://127.0.0.1:${PORT_FORWARD_PORT}${endpoint}"
}

redeploy_stop_port_forward() {
    if [[ -n "${PORT_FORWARD_PID}" ]] && kill -0 "${PORT_FORWARD_PID}" 2>/dev/null; then
        kill "${PORT_FORWARD_PID}" 2>/dev/null || true
        wait "${PORT_FORWARD_PID}" 2>/dev/null || true
    fi
    PORT_FORWARD_PID=""
    PORT_FORWARD_PORT=""
}

redeploy_start_port_forward() {
    local log_path="$1"
    local attempt

    for attempt in 1 2 3 4 5; do
        redeploy_stop_port_forward
        PORT_FORWARD_PORT="$((18080 + RANDOM % 1000))"
        : > "${log_path}"
        kubectl port-forward -n "${K8S_NAMESPACE_SYSTEM}" svc/aura-swarm-gateway "${PORT_FORWARD_PORT}:8080" \
            >"${log_path}" 2>&1 &
        PORT_FORWARD_PID=$!

        for _ in $(seq 1 20); do
            if redeploy_internal_get "/internal/health" >/dev/null 2>&1; then
                return 0
            fi
            if ! kill -0 "${PORT_FORWARD_PID}" 2>/dev/null; then
                break
            fi
            sleep 1
        done
    done

    echo -e "${RED}✗${NC} Failed to port-forward aura-swarm-gateway after multiple attempts."
    if [[ -s "${log_path}" ]]; then
        echo "Port-forward log:"
        sed 's/^/  /' "${log_path}" || true
    fi
    return 1
}

redeploy_snapshot_internal_agents() {
    local endpoint="$1"
    local output_path="$2"
    local log_path="$3"
    local rc

    redeploy_start_port_forward "${log_path}"
    set +e
    redeploy_internal_get "${endpoint}" \
        | jq -S 'sort_by(.agent_id)' > "${output_path}"
    rc=$?
    set -e
    redeploy_stop_port_forward
    return "${rc}"
}

redeploy_snapshot_all_agents() {
    redeploy_snapshot_internal_agents "/internal/agents/all" "$1" "$2"
}

redeploy_snapshot_active_agents() {
    redeploy_snapshot_internal_agents "/internal/agents/active" "$1" "$2"
}

redeploy_snapshot_agent_pods() {
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
                    image: (.spec.containers[0].image // ""),
                    image_id: (.status.containerStatuses[0].imageID // ""),
                    digest: (
                        (.status.containerStatuses[0].imageID // "") as $image_id
                        | if ($image_id | test("sha256:[a-f0-9]{64}")) then
                            ($image_id | capture("(?<digest>sha256:[a-f0-9]{64})").digest)
                          else
                            ""
                          end
                    )
                }
            ] | sort_by(.agent_id, .pod_name)
        ' > "${output_path}"
}

redeploy_expected_harness_digest() {
    local configured_image
    configured_image=$(kubectl get configmap aura-swarm-config -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.data.AURA_HARNESS_IMAGE}' 2>/dev/null || echo "")
    if [[ "${configured_image}" =~ sha256:[a-f0-9]{64} ]]; then
        echo "${BASH_REMATCH[0]}"
    fi
}

redeploy_count_active_agents_without_pods() {
    local active_agents_path="$1"
    local pod_snapshot_path="$2"
    local missing=0
    local agent_id

    while IFS= read -r agent_id; do
        [[ -z "${agent_id}" ]] && continue
        if [[ "$(jq --arg id "${agent_id}" '[.[] | select(.agent_id == $id)] | length' "${pod_snapshot_path}")" == "0" ]]; then
            missing=$((missing + 1))
        fi
    done < <(jq -r '.[].agent_id' "${active_agents_path}")

    echo "${missing}"
}

redeploy_compare_all_agent_ids() {
    local pre_agents_path="$1"
    local post_agents_path="$2"

    local pre_count post_count missing unexpected metadata_changed error_regressions active_regressions
    pre_count=$(jq 'length' "${pre_agents_path}")
    post_count=$(jq 'length' "${post_agents_path}")

    echo ""
    echo -e "${CYAN}Persisted Machine Identity Check${NC}"
    echo "  Pre-redeploy persisted agents:  ${pre_count}"
    echo "  Post-redeploy persisted agents: ${post_count}"

    missing=$(jq -nr --slurpfile pre "${pre_agents_path}" --slurpfile post "${post_agents_path}" '
        ([$pre[0][].agent_id] - [$post[0][].agent_id])[]?
    ')
    unexpected=$(jq -nr --slurpfile pre "${pre_agents_path}" --slurpfile post "${post_agents_path}" '
        ([$post[0][].agent_id] - [$pre[0][].agent_id])[]?
    ')
    metadata_changed=$(jq -nr --slurpfile pre "${pre_agents_path}" --slurpfile post "${post_agents_path}" '
        ($post[0] | map({key: .agent_id, value: {user_id, name}}) | from_entries) as $post_by_id
        | $pre[0][]
        | select($post_by_id[.agent_id] != null)
        | select($post_by_id[.agent_id].user_id != .user_id or $post_by_id[.agent_id].name != .name)
        | .agent_id
    ')
    error_regressions=$(jq -nr --slurpfile pre "${pre_agents_path}" --slurpfile post "${post_agents_path}" '
        ($post[0] | map({key: .agent_id, value: .status}) | from_entries) as $post_status
        | $pre[0][]
        | select($post_status[.agent_id] != null)
        | select(.status != "error" and $post_status[.agent_id] == "error")
        | "\(.agent_id) \(.status) -> error"
    ')
    active_regressions=$(jq -nr --slurpfile pre "${pre_agents_path}" --slurpfile post "${post_agents_path}" '
        def active: . == "provisioning" or . == "running" or . == "idle";
        ($post[0] | map({key: .agent_id, value: .status}) | from_entries) as $post_status
        | $pre[0][]
        | select($post_status[.agent_id] != null)
        | select(.status | active)
        | select(($post_status[.agent_id] | active) | not)
        | "\(.agent_id) \(.status) -> \($post_status[.agent_id])"
    ')

    if [[ -n "${missing}" ]]; then
        echo -e "${RED}✗${NC} Missing persisted AgentIds after redeploy:"
        printf '%s\n' "${missing}" | sed 's/^/    /'
    fi

    if [[ -n "${unexpected}" ]]; then
        echo -e "${RED}✗${NC} Unexpected new persisted AgentIds appeared during redeploy:"
        printf '%s\n' "${unexpected}" | sed 's/^/    /'
    fi

    if [[ -n "${metadata_changed}" ]]; then
        echo -e "${YELLOW}⚠${NC} Agent metadata changed for:"
        printf '%s\n' "${metadata_changed}" | sed 's/^/    /'
    fi

    if [[ -n "${error_regressions}" ]]; then
        echo -e "${RED}✗${NC} Existing AgentIds regressed to Error during redeploy:"
        printf '%s\n' "${error_regressions}" | sed 's/^/    /'
    fi

    if [[ -n "${active_regressions}" ]]; then
        echo -e "${RED}✗${NC} Existing active AgentIds stopped being active during redeploy:"
        printf '%s\n' "${active_regressions}" | sed 's/^/    /'
    fi

    if [[ -n "${missing}" || -n "${unexpected}" || -n "${error_regressions}" || -n "${active_regressions}" ]]; then
        echo ""
        echo "Redeploy failed because persisted machine identity or lifecycle state changed unsafely."
        return 1
    fi

    echo -e "${GREEN}✓${NC} Preserved the exact set of ${pre_count} persisted AgentIds"
}

redeploy_verify_agent_ids_still_present() {
    local pre_agents_path="$1"
    local post_agents_path="$2"
    local label="${3:-AgentIds}"

    local pre_count missing error_regressions active_regressions
    pre_count=$(jq 'length' "${pre_agents_path}")
    missing=$(jq -nr --slurpfile pre "${pre_agents_path}" --slurpfile post "${post_agents_path}" '
        ([$pre[0][].agent_id] - [$post[0][].agent_id])[]?
    ')
    error_regressions=$(jq -nr --slurpfile pre "${pre_agents_path}" --slurpfile post "${post_agents_path}" '
        ($post[0] | map({key: .agent_id, value: .status}) | from_entries) as $post_status
        | $pre[0][]
        | select($post_status[.agent_id] != null)
        | select($post_status[.agent_id] == "error")
        | "\(.agent_id) \(.status) -> error"
    ')
    active_regressions=$(jq -nr --slurpfile pre "${pre_agents_path}" --slurpfile post "${post_agents_path}" '
        def active: . == "provisioning" or . == "running" or . == "idle";
        ($post[0] | map({key: .agent_id, value: .status}) | from_entries) as $post_status
        | $pre[0][]
        | select($post_status[.agent_id] != null)
        | select(($post_status[.agent_id] | active) | not)
        | "\(.agent_id) \(.status) -> \($post_status[.agent_id])"
    ')

    echo ""
    echo -e "${CYAN}${label} Preservation Check${NC}"
    echo "  Pre-redeploy IDs checked: ${pre_count}"

    if [[ -n "${missing}" ]]; then
        echo -e "${RED}✗${NC} Missing AgentIds after redeploy:"
        printf '%s\n' "${missing}" | sed 's/^/    /'
    fi

    if [[ -n "${error_regressions}" ]]; then
        echo -e "${RED}✗${NC} Pre-redeploy AgentIds regressed to Error:"
        printf '%s\n' "${error_regressions}" | sed 's/^/    /'
    fi

    if [[ -n "${active_regressions}" ]]; then
        echo -e "${RED}✗${NC} Pre-redeploy AgentIds stopped being active:"
        printf '%s\n' "${active_regressions}" | sed 's/^/    /'
    fi

    if [[ -n "${missing}" || -n "${error_regressions}" || -n "${active_regressions}" ]]; then
        return 1
    fi

    echo -e "${GREEN}✓${NC} All ${pre_count} pre-redeploy AgentId(s) are still present"
}

redeploy_verify_pods_belong_to_persisted_agents() {
    local all_agents_path="$1"
    local pod_snapshot_path="$2"

    local unknown_pods
    unknown_pods=$(jq -r --slurpfile agents "${all_agents_path}" '
        ([$agents[0][].agent_id]) as $agent_ids
        | .[]
        | select(.agent_id == "" or (.agent_id as $id | $agent_ids | index($id) | not))
        | "\(.pod_name) agent_id=\(.agent_id // "<missing>")"
    ' "${pod_snapshot_path}")

    if [[ -n "${unknown_pods}" ]]; then
        echo -e "${RED}✗${NC} Found swarm-agent pods that do not map to persisted AgentIds:"
        printf '%s\n' "${unknown_pods}" | sed 's/^/    /'
        return 1
    fi

    echo -e "${GREEN}✓${NC} Every swarm-agent pod maps to a persisted AgentId"
}

redeploy_wait_for_digest_convergence() {
    local active_agents_path="$1"
    local all_agents_path="$2"
    local output_path="$3"

    local active_count
    active_count=$(jq 'length' "${active_agents_path}")

    if [[ "${active_count}" == "0" ]]; then
        redeploy_snapshot_agent_pods "${output_path}"
        redeploy_verify_pods_belong_to_persisted_agents "${all_agents_path}" "${output_path}"
        echo -e "${GREEN}✓${NC} No active agents; skipping harness digest convergence wait."
        return 0
    fi

    local timeout_secs
    if [[ -n "${REDEPLOY_VERIFY_TIMEOUT_SECS:-}" ]]; then
        timeout_secs="${REDEPLOY_VERIFY_TIMEOUT_SECS}"
    else
        timeout_secs=$((active_count * 35))
        if [[ "${timeout_secs}" -lt 120 ]]; then
            timeout_secs=120
        fi
    fi

    local poll_secs="${REDEPLOY_VERIFY_POLL_SECS:-15}"
    local expected_digest
    expected_digest=$(redeploy_expected_harness_digest)

    if [[ -z "${expected_digest}" ]]; then
        echo -e "${RED}✗${NC} ConfigMap AURA_HARNESS_IMAGE is not pinned to a sha256 digest."
        echo "    Redeploy cannot prove harness migration from a mutable tag."
        return 1
    fi

    local elapsed=0

    echo ""
    echo "Waiting for swarm-agent pods to converge to the configured harness image..."
    echo "  Expected digest: ${expected_digest}"
    echo "  Timeout: ${timeout_secs}s (scheduler replaces at most one stale pod roughly every 30s)"

    while [[ "${elapsed}" -le "${timeout_secs}" ]]; do
        redeploy_snapshot_agent_pods "${output_path}"

        local missing_pods pod_count missing_digest_count mismatched_digests
        missing_pods=$(redeploy_count_active_agents_without_pods "${active_agents_path}" "${output_path}")
        pod_count=$(jq 'length' "${output_path}")
        missing_digest_count=$(jq '[.[] | select(.digest == "")] | length' "${output_path}")
        mismatched_digests=$(jq --arg digest "${expected_digest}" \
            '[.[] | select(.digest != $digest)] | length' "${output_path}")

        if [[ "${missing_pods}" == "0" && "${missing_digest_count}" == "0" && "${mismatched_digests}" == "0" ]]; then
            redeploy_verify_pods_belong_to_persisted_agents "${all_agents_path}" "${output_path}"
            echo -e "${GREEN}✓${NC} Swarm-agent pods converged after ${elapsed}s"
            return 0
        fi

        echo "  [${elapsed}s] active_without_pod=${missing_pods} pod_count=${pod_count} missing_digest=${missing_digest_count} mismatched_digest=${mismatched_digests}"
        sleep "${poll_secs}"
        elapsed=$((elapsed + poll_secs))
    done

    echo -e "${RED}✗${NC} Timed out waiting for harness digest convergence."
    jq -r '.[] | "  \(.agent_id)  \(.pod_name)  \(.phase) ready=\(.ready) digest=\(.digest // "<missing>")"' "${output_path}" || true
    echo ""
    echo "Troubleshooting commands:"
    echo "  kubectl logs -n ${K8S_NAMESPACE_SYSTEM} deploy/aura-swarm-scheduler --tail=80"
    echo "  kubectl get pods -n ${K8S_NAMESPACE_AGENTS} -l app=swarm-agent -o wide"
    echo "  kubectl describe pods -n ${K8S_NAMESPACE_AGENTS} -l app=swarm-agent"
    return 1
}
