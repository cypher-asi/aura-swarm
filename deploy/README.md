# Deploy Workflows

## Confidential (SEV-SNP) Infrastructure

R1 runs dual-mode: legacy agents stay on the kata-fc pool while every new agent is a confidential SEV-SNP VM. The confidential infra installs in this order:

1. **SNP node group (Terraform).** `./02-deploy-network.sh` regenerates `terraform.tfvars` including the confidential node group settings (`CONFIDENTIAL_NODE_*` in `config.env`, default `m6a.metal`, desired 1, min 0, max 3); `./04-deploy-eks.sh` applies it. Nodes come up labeled `swarm.io/confidential-node=true` and tainted `swarm.io/confidential-node=true:NoSchedule`, alongside the legacy pool. EFS encryption is mandatory and hardcoded in the storage module.
2. **CoCo operator.** `./05-configure-eks.sh` installs the Confidential Containers operator (pinned via `COCO_OPERATOR_VERSION`) into `confidential-containers-system` and waits for the controller.
3. **CcRuntime + RuntimeClass.** `./08-deploy-k8s.sh` applies `k8s/10-coco-ccruntime.yaml` (operator then installs the kata-qemu-snp runtime onto the SNP nodes) and `k8s/09-runtime-class.yaml` (RuntimeClasses `kata-fc`, `kata-qemu`, `kata-qemu-snp`; the SNP class carries pod overhead and the confidential-node selector). The CcRuntime apply is skipped with a warning if the operator CRD is missing.
4. **Trustee (KBS + Attestation Service).** `./08-deploy-k8s.sh` applies `k8s/11-trustee.yaml`: the KBS deployment (built-in AS), the `kbs` service on port 8080 (matches the scheduler default `AURA_KBS_URL=http://kbs.swarm-system.svc.cluster.local:8080`), a persistent repository PVC, and network policies allowing agent pods to reach the KBS. The script also generates the KBS admin keypair: private key in `.secrets/kbs-admin.key` (keep it safe; used with `kbs-client`), public key in the `kbs-auth-public-key` secret.

Quick checks after install:

- `kubectl get nodes -l swarm.io/confidential-node=true` — SNP nodes joined.
- `kubectl get ccruntime ccruntime -o jsonpath='{.status}'` — runtime install progress.
- `kubectl get pods -n swarm-system -l app=kbs` — KBS running.

KBS attestation policies/reference values and per-agent DEK provisioning are configured in the attestation/DEK lifecycle phase, not by these scripts.

## R2 Migration Runbook (fleet move to tiers + sealed storage)

R2 is the one-time release that moves every legacy agent onto the new architecture: agent records gain a tier + sealed storage (store schema v1 → v2), pods rolling-recreate from `kata-fc` onto `kata-qemu-snp`, and the harness encrypt-in-place migrates each agent's plaintext state on its first sealed boot. Run the steps in order:

1. **Rollback point: EFS backup.** AWS Backup for the swarm EFS file system is already operational (EFS encryption and backups are enforced by the terraform storage module). Before deploying R2, take an explicit on-demand recovery point of the `swarm-agent-state` file system and note its ARN, and snapshot the gateway/control PVCs' contents if extra paranoia is warranted. This backup is the rollback path: the harness keeps a transient `*.plaintext-backup` directory only for the duration of each agent's encrypt-in-place swap, so the EFS recovery point is the durable pre-R2 state.
2. **Deploy the R2 gateway.** A normal `./08-deploy-k8s.sh` redeploy. On startup the gateway runs the store migration automatically (`schema_version` v1 → v2 in the `meta` CF): every legacy agent record is rewritten to the nearest tier (cpu ≤ 500m → small, ≤ 1000m → standard, else pro), `confidential_vm` isolation, and sealed storage under `swarm/agents/{id}/state-key`. The pass is idempotent and crash-safe (the version bump lands last). The gateway then backfills missing state DEKs in the KBS for migrated agents — put-if-absent, never overwriting an existing DEK. Check the gateway logs for `Store schema migration complete` and `DEK backfill pass complete`; failures of the backfill retry on the next restart.
3. **Enable the rolling pod migration.** Add `MIGRATION_RECREATE_LEGACY_PODS: "true"` to the `aura-swarm-config` ConfigMap (the scheduler deployment reads it via an optional `configMapKeyRef`) and restart the scheduler. The desired-state reconciler then treats any pod whose `runtimeClassName` mismatches its (now confidential) spec as stale and replaces **one pod per ~30s pass**, so the fleet drains off `kata-fc` gradually. On each recreated pod's first sealed boot the harness performs the atomic, resumable encrypt-in-place state migration (copy → fsync → swap → `.aura-sealed` marker → delete plaintext backup). Hibernating/stopped agents migrate on their next wake/start.
4. **Verify convergence.** Run `./10-verify-redeploy.sh` (or `--skip-deploy` for checks only). Beyond the usual identity/digest proofs it now asserts the R2 end state: gateway reports `schema_version: 2` on `/internal/health`, every `swarm-agent` pod is on `kata-qemu-snp`, and no pod remains on `kata-fc`. While step 3 is still in flight, `REDEPLOY_VERIFY_SKIP_R2_CHECKS=true` skips these checks.
5. **Rollback (if needed).** Restore the EFS recovery point from step 1, redeploy the previous gateway/scheduler images, and unset `MIGRATION_RECREATE_LEGACY_PODS`. A v1 database restored from backup will simply re-run the migration on the next R2 gateway start.

After the fleet converges, R3 removes the legacy code paths (`MicroVM`, optional tier, plaintext state) and retires the `kata-fc` RuntimeClass and old node group.

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
