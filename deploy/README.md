# Deploy Workflows

## Normal Redeploy
Run `./08-deploy-k8s.sh` for a normal redeploy. This path updates Kubernetes manifests in place and preserves stored swarm data.

- `AgentId` records, sessions, and user data remain on the gateway/control PVCs.
- Per-agent harness state remains on the shared `swarm-agent-state` PVC under each agent's stable `agent_id` subdirectory.
- Running `swarm-agent` pods are disposable. The scheduler reconciler recreates missing pods and rolling-replaces stale-image pods against the same logical agent IDs.
- The harness image must be pinned to an immutable `@sha256` digest. Run `./07-build-images.sh --harness` before redeploying a new harness.
- Internal gateway APIs require a service bearer token. Create `.secrets/INTERNAL_TOKEN` before deploying; gateway, scheduler, and redeploy verification use the same token for `/internal/*`.
- The deploy fails non-zero if platform rollouts fail, persisted `AgentId`s disappear, unexpected persisted `AgentId`s appear, or active agent pods do not converge to the configured harness digest.

## Destructive Path
`./08-deploy-k8s.sh --reset-data` is the only deploy-script path that wipes stored swarm data.

- It deletes `aura-swarm-gateway-data` and `aura-swarm-control-data`.
- That removes stored agent records, sessions, and user data before the deploy recreates the PVCs.
- Do not use `--reset-data` when you want to preserve existing swarms.

## Redeploy Proof
Run `./10-verify-redeploy.sh` to capture pre/post redeploy evidence.

- It snapshots persisted agent IDs from the gateway before and after redeploy.
- It compares `swarm-agent` pod labels before and after redeploy.
- It waits for harness digest convergence and then runs `./09-verify.sh`.

The first deploy that introduces the `/internal/agents/all` endpoint may fall back to active-ID preservation for its pre-snapshot if the old gateway does not expose that endpoint yet. Subsequent redeploys verify the exact full persisted `AgentId` set, including hibernating, stopped, and error agents.

Use `./10-verify-redeploy.sh --recreate-agents` when you want the proof run to force immediate agent pod recycling instead of waiting for the scheduler to roll stale pods one at a time.
