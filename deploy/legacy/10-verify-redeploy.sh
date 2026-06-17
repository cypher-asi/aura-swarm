#!/bin/bash
# 10-verify-redeploy.sh - Prove that a normal redeploy preserves active swarms
#
# Usage:
#   ./10-verify-redeploy.sh
#   ./10-verify-redeploy.sh --recreate-agents
#   ./10-verify-redeploy.sh --skip-deploy
#
# This script:
# - snapshots active agent IDs from the gateway before and after redeploy
# - snapshots swarm-agent pods, labels, and harness digests
# - runs a normal deploy (unless --skip-deploy is set)
# - waits for harness digest convergence
# - runs 09-verify.sh for the final cluster health check

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

RUN_DEPLOY=true
DEPLOY_ARGS=()
POLL_SECS="${REDEPLOY_VERIFY_POLL_SECS:-15}"
KEEP_EVIDENCE="${REDEPLOY_VERIFY_KEEP_EVIDENCE:-false}"
# R2 (Swarm TEE migration) convergence checks: all agent pods on
# kata-qemu-snp, none on kata-fc, gateway schema_version == 2. Set to
# "true" only during the transition window before the R2 rollout
# (MIGRATION_RECREATE_LEGACY_PODS) has completed.
SKIP_R2_CHECKS="${REDEPLOY_VERIFY_SKIP_R2_CHECKS:-false}"

usage() {
    echo "Usage: $0 [--skip-deploy] [--recreate-agents|--no-recreate-agents]"
    echo ""
    echo "Options:"
    echo "  --skip-deploy         Only run the pre/post proof checks"
    echo "  --recreate-agents     Forward to 08-deploy-k8s.sh for immediate pod recycling"
    echo "  --no-recreate-agents  Forward to 08-deploy-k8s.sh (default)"
    echo ""
    echo "This script refuses --reset-data because it would invalidate the"
    echo "swarm-persistence proof."
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-deploy)
            RUN_DEPLOY=false
            ;;
        --recreate-agents|--no-recreate-agents)
            DEPLOY_ARGS+=("$1")
            ;;
        --reset-data)
            echo -e "${RED}✗${NC} --reset-data is destructive and cannot be used with redeploy verification."
            exit 1
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            usage
            exit 1
            ;;
    esac
    shift
done

# Tolerate Windows CRLF line endings in config.env when running under Bash.
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_redeploy_verify.sh"

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo -e "${RED}✗${NC} Missing required command: $1"
        exit 1
    fi
}

for cmd in kubectl jq curl base64; do
    require_command "$cmd"
done

TMP_DIR=$(mktemp -d)
PORT_FORWARD_PID=""
PORT_FORWARD_PORT=""
PORT_FORWARD_LOG="${TMP_DIR}/port-forward.log"

stop_port_forward() {
    if [[ -n "${PORT_FORWARD_PID}" ]] && kill -0 "${PORT_FORWARD_PID}" 2>/dev/null; then
        kill "${PORT_FORWARD_PID}" 2>/dev/null || true
        wait "${PORT_FORWARD_PID}" 2>/dev/null || true
    fi
    PORT_FORWARD_PID=""
    PORT_FORWARD_PORT=""
}

cleanup() {
    local exit_code="${1:-0}"
    stop_port_forward
    if [[ "${KEEP_EVIDENCE}" == "true" || "${exit_code}" -ne 0 ]]; then
        echo ""
        echo -e "${CYAN}Evidence directory:${NC} ${TMP_DIR}"
    else
        rm -rf "${TMP_DIR}"
    fi
}

trap 'cleanup $?' EXIT

start_port_forward() {
    local attempt
    for attempt in 1 2 3 4 5; do
        stop_port_forward
        PORT_FORWARD_PORT="$((18080 + RANDOM % 1000))"
        : > "${PORT_FORWARD_LOG}"
        kubectl port-forward -n "${K8S_NAMESPACE_SYSTEM}" svc/aura-swarm-gateway "${PORT_FORWARD_PORT}:8080" \
            >"${PORT_FORWARD_LOG}" 2>&1 &
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
    if [[ -s "${PORT_FORWARD_LOG}" ]]; then
        echo "Port-forward log:"
        cat "${PORT_FORWARD_LOG}"
    fi
    return 1
}

snapshot_active_agents() {
    local output_path="$1"
    redeploy_snapshot_active_agents "${output_path}" "${PORT_FORWARD_LOG}"
}

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

get_expected_harness_digest() {
    local configured_image
    configured_image=$(kubectl get configmap aura-swarm-config -n "${K8S_NAMESPACE_SYSTEM}" \
        -o jsonpath='{.data.AURA_HARNESS_IMAGE}' 2>/dev/null || echo "")
    if [[ "${configured_image}" =~ sha256:[a-f0-9]{64} ]]; then
        echo "${BASH_REMATCH[0]}"
    fi
}

count_active_agents_without_pods() {
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

wait_for_digest_convergence() {
    local active_agents_path="$1"
    local output_path="$2"

    local active_count
    active_count=$(jq 'length' "${active_agents_path}")

    if [[ "${active_count}" == "0" ]]; then
        snapshot_agent_pods "${output_path}"
        echo -e "${GREEN}✓${NC} No active agents before/after redeploy; skipping digest convergence wait."
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

    local expected_digest
    expected_digest=$(get_expected_harness_digest)
    local elapsed=0

    echo ""
    echo "Waiting for swarm-agent pods to converge to the configured harness image..."
    if [[ -n "${expected_digest}" ]]; then
        echo "  Expected digest: ${expected_digest}"
    else
        echo -e "  ${YELLOW}No pinned harness digest found in ConfigMap; verifying single-digest convergence only.${NC}"
    fi
    echo "  Timeout: ${timeout_secs}s (scheduler replaces at most one stale pod roughly every 30s)"

    while [[ "${elapsed}" -le "${timeout_secs}" ]]; do
        snapshot_agent_pods "${output_path}"

        local missing_pods
        missing_pods=$(count_active_agents_without_pods "${active_agents_path}" "${output_path}")

        local pod_count
        pod_count=$(jq 'length' "${output_path}")

        local missing_digest_count
        missing_digest_count=$(jq '[.[] | select(.digest == "")] | length' "${output_path}")

        local unique_digest_count
        unique_digest_count=$(jq '[.[] | select(.digest != "") | .digest] | unique | length' "${output_path}")

        local converged=false
        if [[ "${missing_pods}" == "0" && "${missing_digest_count}" == "0" ]]; then
            if [[ -n "${expected_digest}" ]]; then
                local mismatched_digests
                mismatched_digests=$(jq --arg digest "${expected_digest}" \
                    '[.[] | select(.digest != $digest)] | length' "${output_path}")
                if [[ "${mismatched_digests}" == "0" ]]; then
                    converged=true
                fi
            elif [[ "${unique_digest_count}" == "1" ]]; then
                converged=true
            fi
        fi

        if [[ "${converged}" == "true" ]]; then
            echo -e "${GREEN}✓${NC} Swarm-agent pods converged after ${elapsed}s"
            return 0
        fi

        echo "  [${elapsed}s] active_without_pod=${missing_pods} pod_count=${pod_count} missing_digest=${missing_digest_count} unique_digests=${unique_digest_count}"

        sleep "${POLL_SECS}"
        elapsed=$((elapsed + POLL_SECS))
    done

    echo -e "${RED}✗${NC} Timed out waiting for harness digest convergence."
    jq -r '.[] | "  \(.agent_id)  \(.pod_name)  \(.digest // "<missing>")"' "${output_path}" || true
    return 1
}

compare_active_agents() {
    local pre_active_path="$1"
    local post_active_path="$2"
    local pre_pods_path="$3"
    local post_pods_path="$4"

    local pre_count
    local post_count
    pre_count=$(jq 'length' "${pre_active_path}")
    post_count=$(jq 'length' "${post_active_path}")

    echo ""
    echo -e "${CYAN}Active Swarm Identity Check${NC}"
    echo "  Pre-redeploy active agents:  ${pre_count}"
    echo "  Post-redeploy active agents: ${post_count}"

    if [[ "${pre_count}" == "0" ]]; then
        echo -e "${GREEN}✓${NC} No active swarms existed before redeploy, so there were no active IDs to preserve."
        return 0
    fi

    declare -A POST_ACTIVE=()
    while IFS=$'\t' read -r agent_id user_id name; do
        [[ -z "${agent_id}" ]] && continue
        POST_ACTIVE["${agent_id}"]="${user_id}|${name}"
    done < <(jq -r '.[] | [.agent_id, .user_id, .name] | @tsv' "${post_active_path}")

    local missing=0
    local metadata_changed=0
    local replaced_pods=0

    while IFS=$'\t' read -r agent_id user_id name; do
        [[ -z "${agent_id}" ]] && continue

        if [[ -z "${POST_ACTIVE[${agent_id}]+x}" ]]; then
            echo -e "${RED}✗${NC} Missing active agent after redeploy: ${agent_id}"
            missing=$((missing + 1))
            continue
        fi

        IFS='|' read -r post_user post_name <<< "${POST_ACTIVE[${agent_id}]}"
        if [[ "${post_user}" != "${user_id}" || "${post_name}" != "${name}" ]]; then
            echo -e "${YELLOW}⚠${NC} Agent metadata changed for ${agent_id}"
            echo "    pre:  user=${user_id} name=${name}"
            echo "    post: user=${post_user} name=${post_name}"
            metadata_changed=$((metadata_changed + 1))
        fi

        local pre_pods
        local post_pods
        pre_pods=$(jq -r --arg id "${agent_id}" \
            '[.[] | select(.agent_id == $id) | .pod_name] | sort | join(",")' "${pre_pods_path}")
        post_pods=$(jq -r --arg id "${agent_id}" \
            '[.[] | select(.agent_id == $id) | .pod_name] | sort | join(",")' "${post_pods_path}")

        if [[ -z "${post_pods}" ]]; then
            echo -e "${RED}✗${NC} No swarm-agent pod found after redeploy for ${agent_id}"
            missing=$((missing + 1))
            continue
        fi

        if [[ "${pre_pods}" != "${post_pods}" ]]; then
            replaced_pods=$((replaced_pods + 1))
        fi
    done < <(jq -r '.[] | [.agent_id, .user_id, .name] | @tsv' "${pre_active_path}")

    if [[ "${missing}" != "0" ]]; then
        return 1
    fi

    echo -e "${GREEN}✓${NC} Preserved all ${pre_count} pre-existing active AgentIds across redeploy"
    echo "  Active agents with new pod names: ${replaced_pods}"
    if [[ "${metadata_changed}" != "0" ]]; then
        echo -e "  ${YELLOW}Metadata changes detected:${NC} ${metadata_changed}"
    fi
}

# R2 (Swarm TEE migration) convergence proof:
# - the gateway reports store schema_version 2 (v1 -> v2 migration ran),
# - every swarm-agent pod runs on runtimeClassName kata-qemu-snp,
# - no swarm-agent pod is left on the legacy kata-fc runtime class.
verify_r2_convergence() {
    echo ""
    echo -e "${CYAN}R2 Migration Convergence Check${NC}"

    if [[ "${SKIP_R2_CHECKS}" == "true" ]]; then
        echo -e "${YELLOW}⚠${NC} Skipping R2 checks (REDEPLOY_VERIFY_SKIP_R2_CHECKS=true)."
        echo "    (R3: the reconciler always replaces runtime-class-stale pods; skip only mid-rollout.)"
        return 0
    fi

    local failures=0

    # 1) Store schema version via the gateway internal health endpoint.
    start_port_forward
    local schema_version
    schema_version=$(redeploy_internal_get "/internal/health" | jq -r '.schema_version // "null"')
    stop_port_forward
    if [[ "${schema_version}" == "2" ]]; then
        echo -e "${GREEN}✓${NC} Gateway reports store schema_version 2 (records migrated to tiered/sealed)"
    else
        echo -e "${RED}✗${NC} Gateway reports schema_version=${schema_version} (expected 2)."
        echo "    The v1 -> v2 startup migration has not completed; check gateway logs."
        failures=$((failures + 1))
    fi

    # 2) Runtime classes of every swarm-agent pod.
    local runtime_classes_json="${TMP_DIR}/pod-runtime-classes.json"
    kubectl get pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent -o json \
        | jq '
            [
                .items[] | {
                    agent_id: (.metadata.labels["swarm.io/agent-id"] // ""),
                    pod_name: (.metadata.name // ""),
                    runtime_class: (.spec.runtimeClassName // "<none>")
                }
            ] | sort_by(.agent_id, .pod_name)
        ' > "${runtime_classes_json}"

    local pod_count snp_count legacy_pods other_pods
    pod_count=$(jq 'length' "${runtime_classes_json}")
    snp_count=$(jq '[.[] | select(.runtime_class == "kata-qemu-snp")] | length' "${runtime_classes_json}")
    legacy_pods=$(jq -r '.[] | select(.runtime_class == "kata-fc") | "  \(.agent_id)  \(.pod_name)"' "${runtime_classes_json}")
    other_pods=$(jq -r '.[] | select(.runtime_class != "kata-qemu-snp" and .runtime_class != "kata-fc") | "  \(.agent_id)  \(.pod_name)  \(.runtime_class)"' "${runtime_classes_json}")

    if [[ -n "${legacy_pods}" ]]; then
        echo -e "${RED}✗${NC} Pods still on the legacy kata-fc runtime class:"
        printf '%s\n' "${legacy_pods}"
        echo "    The reconciler replaces runtime-class-stale pods one per ~30s pass; wait for the rolling recreation."
        failures=$((failures + 1))
    fi
    if [[ -n "${other_pods}" ]]; then
        echo -e "${RED}✗${NC} Pods on an unexpected runtime class:"
        printf '%s\n' "${other_pods}"
        failures=$((failures + 1))
    fi
    if [[ "${pod_count}" == "0" ]]; then
        echo -e "${GREEN}✓${NC} No swarm-agent pods running (nothing to check for runtime class)"
    elif [[ "${snp_count}" == "${pod_count}" ]]; then
        echo -e "${GREEN}✓${NC} All ${pod_count} swarm-agent pod(s) on runtimeClassName kata-qemu-snp"
    fi

    if [[ "${failures}" != "0" ]]; then
        echo ""
        echo "R2 migration convergence check failed (${failures} issue(s))."
        return 1
    fi
}

echo "=============================================="
echo "  AURA Swarm - Redeploy Verification"
echo "=============================================="
echo ""
echo "Deploy cluster: ${EKS_CLUSTER_NAME}"
echo "Run deploy: ${RUN_DEPLOY}"
if [[ ${#DEPLOY_ARGS[@]} -eq 0 ]]; then
    echo "Deploy args: (none)"
else
    echo "Deploy args: ${DEPLOY_ARGS[*]}"
fi
echo ""

PRE_ACTIVE_JSON="${TMP_DIR}/pre-active-agents.json"
POST_ACTIVE_JSON="${TMP_DIR}/post-active-agents.json"
PRE_ALL_JSON="${TMP_DIR}/pre-all-agents.json"
POST_ALL_JSON="${TMP_DIR}/post-all-agents.json"
PRE_PODS_JSON="${TMP_DIR}/pre-agent-pods.json"
POST_PODS_JSON="${TMP_DIR}/post-agent-pods.json"
PRE_AGENT_SCOPE="all"

echo "Capturing pre-redeploy snapshots..."
snapshot_active_agents "${PRE_ACTIVE_JSON}"
if redeploy_snapshot_all_agents "${PRE_ALL_JSON}" "${PORT_FORWARD_LOG}"; then
    PRE_AGENT_SCOPE="all"
else
    echo -e "${YELLOW}⚠${NC} Existing gateway does not expose /internal/agents/all."
    echo "    Falling back to active AgentId preservation for this first upgraded deploy."
    cp "${PRE_ACTIVE_JSON}" "${PRE_ALL_JSON}"
    PRE_AGENT_SCOPE="active"
fi
snapshot_agent_pods "${PRE_PODS_JSON}"
echo -e "${GREEN}✓${NC} Captured pre-redeploy persisted agents, active agents, and pod state"

if [[ "${RUN_DEPLOY}" == "true" ]]; then
    echo ""
    echo "Running normal deploy..."
    "${SCRIPT_DIR}/08-deploy-k8s.sh" "${DEPLOY_ARGS[@]}"
fi

echo ""
echo "Capturing post-redeploy snapshots..."
snapshot_active_agents "${POST_ACTIVE_JSON}"
redeploy_snapshot_all_agents "${POST_ALL_JSON}" "${PORT_FORWARD_LOG}"

redeploy_wait_for_digest_convergence "${POST_ACTIVE_JSON}" "${POST_ALL_JSON}" "${POST_PODS_JSON}"

echo ""
echo "Running final deployment health verification..."
"${SCRIPT_DIR}/09-verify.sh"

if [[ "${PRE_AGENT_SCOPE}" == "all" ]]; then
    redeploy_compare_all_agent_ids "${PRE_ALL_JSON}" "${POST_ALL_JSON}"
else
    redeploy_verify_agent_ids_still_present "${PRE_ALL_JSON}" "${POST_ALL_JSON}" "Active AgentId"
fi
compare_active_agents "${PRE_ACTIVE_JSON}" "${POST_ACTIVE_JSON}" "${PRE_PODS_JSON}" "${POST_PODS_JSON}"

verify_r2_convergence

EXPECTED_DIGEST=$(get_expected_harness_digest || true)
FINAL_DIGESTS=$(jq -r '[.[] | select(.digest != "") | .digest] | unique | join(", ")' "${POST_PODS_JSON}")

echo ""
echo -e "${GREEN}=============================================="
echo "  Redeploy verification passed"
echo "==============================================${NC}"
if [[ -n "${EXPECTED_DIGEST}" ]]; then
    echo "Configured harness digest: ${EXPECTED_DIGEST}"
fi
if [[ -n "${FINAL_DIGESTS}" ]]; then
    echo "Observed swarm-agent digests: ${FINAL_DIGESTS}"
fi
echo ""
echo "This run verified that pre-existing active AgentIds survived redeploy and"
echo "that swarm-agent pods converged to the configured harness image."
