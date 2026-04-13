# Deploy Workflows

## Normal Redeploy
Run `./08-deploy-k8s.sh` for a normal redeploy. This path updates Kubernetes manifests in place and preserves stored swarm data.

- `AgentId` records, sessions, and user data remain on the gateway/control PVCs.
- Per-agent harness state remains on the shared `swarm-agent-state` PVC under each agent's stable `agent_id` subdirectory.
- Running `swarm-agent` pods are disposable. The scheduler reconciler recreates missing pods and rolling-replaces stale-image pods against the same logical agent IDs.

## Destructive Path
`./08-deploy-k8s.sh --reset-data` is the only deploy-script path that wipes stored swarm data.

- It deletes `aura-swarm-gateway-data` and `aura-swarm-control-data`.
- That removes stored agent records, sessions, and user data before the deploy recreates the PVCs.
- Do not use `--reset-data` when you want to preserve existing swarms.

## Redeploy Proof
Run `./10-verify-redeploy.sh` to capture pre/post redeploy evidence.

- It snapshots active agent IDs from the gateway before and after redeploy.
- It compares `swarm-agent` pod labels before and after redeploy.
- It waits for harness digest convergence and then runs `./09-verify.sh`.

Use `./10-verify-redeploy.sh --recreate-agents` when you want the proof run to force immediate agent pod recycling instead of waiting for the scheduler to roll stale pods one at a time.
