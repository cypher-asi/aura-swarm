# Confidential agents via Peer Pods / Cloud API Adaptor (`kata-remote`)

Status: **implemented (sole confidential runtime)** · Owner: deploy ·
Architecture: confidential agents run as **Peer Pods / Cloud API Adaptor (CAA)**
— a per-agent AWS-managed SEV-SNP "pod VM" launched off-cluster — using the
`kata-remote` RuntimeClass on ordinary EKS workers.

> **Direction.** Peer Pods is the **only** confidential path. The on-node
> `kata-qemu-snp` / SEV-SNP metal alternative (and the old `CONFIDENTIAL_RUNTIME`
> switch, `snp_local` mode, confidential metal node group, and the SNP-host AMI
> plan) have been **removed** to streamline the codebase. Confidential agents
> derive `kata-remote` directly from `IsolationLevel::ConfidentialVM`; there is
> no runtime toggle. This moves the SNP-host burden to AWS (no kernel/KVM/OVMF
> ownership), makes confidential compute a **per-active-agent** cost, and scales
> elastically per pod. Sections below that still describe the switchable
> migration are **historical** — kept for the cost/attestation/EFS analysis.

---

## 1. Problem

The staged TEE rollout fails at **Step 05 (SNP attestation smoke test)** when a `kata-qemu-snp` pod tries to boot a confidential VM **on the node**:

```
failed to create shim task: This system doesn't support Confidential Computing (Guest Protection)
```

### Root cause (premise — verified)

`kata-qemu-snp` runs the confidential VM on the worker node, so the node's own hypervisor must launch AMD SEV-SNP guests. A **stock** `m6a.metal` EKS node cannot: the AL2023 AMI has no SNP-host kernel (needs Linux 6.11+), AWS Nitro needs a CCP-deferral workaround, and AWS's *managed* SEV-SNP is **guest-only** (per-pod), not a host capability. Fixing it on-node would mean owning a custom SNP-host AMI (Ubuntu + patched KVM/OVMF) and a host kernel forever — which is why we chose Peer Pods instead and removed the on-node path.

### Today's shape (verified, the baseline this plan changes)

- Every agent pod is built by `crates/aura-swarm-scheduler/src/pod.rs` (`build_pod_spec`) with `runtimeClassName: kata-qemu-snp`, derived from `IsolationLevel::ConfidentialVM.runtime_class()` in `crates/aura-swarm-store/src/types.rs`.
- Confidential pods carry `nodeSelector swarm.io/confidential-node=true` and a matching `NoSchedule` toleration (`CONFIDENTIAL_NODE_KEY` in `pod.rs`), so they land on the tainted `m6a.metal` managed node group (`terraform/modules/eks/main.tf` → `aws_eks_node_group.confidential`, default `m6a.metal` from `config.env` `CONFIDENTIAL_NODE_INSTANCE_TYPE`).
- The CoCo operator (`v0.17.0`, `deploy/03-coco-operator.sh`) installs the kata payload (`kata-deploy:3.23.0`) via the `CcRuntime` CR (`deploy/k8s/10-coco-ccruntime.yaml`); RuntimeClasses are owned by `deploy/k8s/09-runtime-class.yaml`.
- The harness fetches its per-agent state DEK from the Trustee KBS through the in-guest CDH after SNP attestation; the scheduler injects `AURA_KBS_URL`, `AURA_STATE_ENCRYPTION=sealed`, `AURA_STATE_KEY_ID` (`build_confidential_env_vars` in `pod.rs`). Sealed state lives on the `swarm-agent-state` EFS PVC, mounted per-agent by `subPath` (`build_state_volume` / `build_state_mount`).

### How Peer Pods avoids the SNP-host gap (premise)

CAA launches a **per-pod AWS-managed SEV-SNP EC2 "pod VM"** (e.g. `m6a.large`
with `DISABLECVM=false`), so the EKS worker nodes can be **ordinary instances**
— no SNP-host kernel anywhere on the cluster. The Kata shim and
`agent-protocol-forwarder` run on the normal worker; the workload, guest
kernel, and CDH run inside the AWS-owned SNP pod VM. The official path is the
CoCo **Cloud API Adaptor** Helm chart (`cloud-api-adaptor`), which creates a
`kata-remote` RuntimeClass. Reference:
`confidentialcontainers.org/docs/examples/aws-simple`. CAA configuration we
will need: `PODVM_AMI_ID`, `PODVM_INSTANCE_TYPE`, `DISABLECVM=false`,
`VXLAN_PORT=9000`, an AWS credentials/role secret, and security-group ports
**15150** (agent-protocol-forwarder) and **9000/tcp+udp** (vxlan).

---

## 2. Chosen approach (Option 3 from the AMI plan, promoted)

Run confidential agents as **Peer Pods**: the scheduler emits the
`kata-remote` RuntimeClass instead of `kata-qemu-snp`, the CAA daemonset (Helm)
replaces the CoCo-operator/CcRuntime install, and CAA boots an AWS-managed
SEV-SNP pod VM per agent. Workers become ordinary instances; the confidential
`m6a.metal` node group is zeroed/removed. Keep SNP-local as a **fallback**
behind a config switch (`CONFIDENTIAL_RUNTIME`, §4.2) so we can revert without
a code rollback.

### Alternatives considered


| Option                                                   | SNP host owned by | Confidential   | Cost model            | Notes                                                                         |
| -------------------------------------------------------- | ----------------- | -------------- | --------------------- | ----------------------------------------------------------------------------- |
| **3. Peer Pods / CAA (this plan)**                       | **AWS**           | ✅ (per-pod VM) | per-active-agent      | Larger runtime rework; biggest unknown is EFS/sealed-state in the pod VM (§6) |
| 1. SNP-host AMI on metal (rejected, removed)             | us                | ✅ (on node)    | fixed ~$6.1k/mo/metal | Smallest code change; ongoing AMI/kernel ownership                            |
| 2. `kata-qemu` on stock metal                            | —                 | ❌              | fixed                 | VM isolation only, no TEE/attestation — fails the v0.2.0 threat model         |
| 4. Hard cutover (no fallback switch)                     | AWS               | ✅              | per-active-agent      | Less config surface, but no quick revert if CAA/EFS blocks us                 |


---

## 3. Cost framing (rough — us-east-2, Linux On-Demand, ~730 hrs/mo)


| Item                        | Rate       | ~Monthly          | Notes                                           |
| --------------------------- | ---------- | ----------------- | ----------------------------------------------- |
| On-node `m6a.metal` (today) | ~$8.29/hr  | **~$6,100 fixed** | Paid regardless of agent count                  |
| Peer Pod `m6a.large`        | ~$0.095/hr | **~$69**          | per active agent; includes AWS +10% SEV-SNP fee |
| Peer Pod `m6a.xlarge`       | —          | ~$139             | per active agent                                |
| Peer Pod `m6a.2xlarge`      | —          | ~$277             | per active agent                                |
| System pool `m5.2xlarge`    | —          | ~$280 each        | **common to both** plans                        |


Break-even vs **one** metal node ≈ **87× `large` / 43× `xlarge` / 22×
`2xlarge`** always-on pod VMs. Below those counts Peer Pods is cheaper and
scales to zero when idle; above them (a large always-on fleet) metal wins on
unit cost — a reason to keep the fallback switch. These are planning numbers,
not a quote.

---

## 4. Work breakdown

### Phase 1 — Scheduler / runtime code (`crates/aura-swarm-scheduler`, `aura-swarm-store`)

The isolation→runtime-class mapping is the crux. Today
`IsolationLevel::ConfidentialVM.runtime_class()` hardcodes `Some("kata-qemu-snp")`
(`aura-swarm-store/src/types.rs`). We make the **confidential runtime class
configurable** rather than adding a new `IsolationLevel` (the store enum,
migrations, and tier specs all assume two levels; a third would ripple through
`migrations.rs` and every `match`).

- `**SchedulerConfig` (`types.rs`)**: add `confidential_runtime_class: String`
(default `"kata-qemu-snp"`) and a `confidential_runtime` mode (`snp_local` |
`peer_pods`), parsed in `from_env()` from `CONFIDENTIAL_RUNTIME` /
`CONFIDENTIAL_RUNTIME_CLASS` (§4.2). When `peer_pods`, the effective class is
`kata-remote`.
- `**pod.rs build_pod_spec`**: replace the `isolation.runtime_class()` call for
confidential pods with `config.confidential_runtime_class`. Container
(dev-mode) pods stay `None` (default runtime) — unchanged.
- **Drop the SNP node selector/toleration for Peer Pods.** Today
`confidential = isolation == ConfidentialVM` gates both the
`swarm.io/confidential-node` `node_selector` and the matching `toleration`.
Peer-pod workloads run **off-cluster** in the pod VM; the pod shim runs on a
normal worker, so these pods must **not** carry the selector/toleration (there
is no tainted metal pool to land on). Gate them on
`confidential_runtime == snp_local` instead of on `confidential`. Keep the
`ConfidentialVM` security context (root-in-guest) and **all** KBS/CDH env
wiring (`AURA_KBS_URL`, `AURA_STATE_ENCRYPTION`, `AURA_STATE_KEY_ID`,
`AURA_SWARM_INTERNAL_TOKEN`) unchanged — attestation still works via the CDH
**inside** the pod VM; only the VM's location moves.
- `**k8s.rs` `desired_runtime_class` / `pod_runtime_class_is_stale`**: these
already compare a pod's `runtimeClassName` against what `build_pod` would
produce. Point `desired_runtime_class` at the same configurable class so the
comparison stays truthful. **Rollout implication:** flipping the desired class
from `kata-qemu-snp` to `kata-remote` makes `pod_runtime_class_is_stale`
return true for **every** running agent, so the desired-state reconciler
(`run_desired_state_reconciler`, one-pod-per-pass pacing) will intentionally
**churn the whole fleet**, recreating each agent as a Peer Pod. That is the
desired behavior but must be a deliberate, monitored migration (capacity for N
new pod VMs, KBS load, EFS contention) — not a surprise side effect of a
config change. Each recreate boots a fresh EC2 pod VM and re-attests.
- **Tests**: update the `pod.rs` / `k8s.rs` unit tests that assert
`kata-qemu-snp`, the SNP node selector, and the toleration so they branch on
the configured runtime (add `peer_pods` cases asserting `kata-remote`, **no**
selector/toleration, KBS env still present).

### Phase 2 — Switchable config vs hard cutover (decision: **switchable**)

Keep SNP-local as a fallback. Add to `config.env`:

```bash
# snp_local = on-node kata-qemu-snp (legacy/fallback); peer_pods = CAA kata-remote
export CONFIDENTIAL_RUNTIME="${CONFIDENTIAL_RUNTIME:-peer_pods}"
```

- The scheduler reads `CONFIDENTIAL_RUNTIME` (and an optional
`CONFIDENTIAL_RUNTIME_CLASS` override) via `SchedulerConfig::from_env()`; the
deploy steps pass it into the scheduler Deployment env.
- This mirrors how `DEFAULT_ISOLATION` already toggles container vs
confidential. It lets us revert to metal+`kata-qemu-snp` by flipping one knob
and re-running the rollout (the fleet churns back), without a code rollback —
important while §6 unknowns are open.
- `DEFAULT_ISOLATION=confidential_vm` is unchanged; `CONFIDENTIAL_RUNTIME` only
selects **how** a confidential pod is realized.

### Phase 3 — Terraform / infra (`terraform/modules/eks`, root `main.tf`, `config.env`, `_lib.sh`)

- **Zero/remove the confidential metal node group.** Set
`CONFIDENTIAL_NODE_DESIRED_COUNT=0` (and min 0) in `config.env`, or drop
`aws_eks_node_group.confidential` from `modules/eks/main.tf` once Peer Pods is
proven. Workers become ordinary `m5.2xlarge` (the existing `main` system
pool); agent pod shims and `agent-protocol-forwarder` schedule there, so size
the pool for shim overhead, not for whole agents.
- **CAA IAM.** Add a node/IRSA policy granting CAA permission to manage pod VMs:
`ec2:RunInstances`, `ec2:TerminateInstances`, `ec2:DescribeInstances`,
`ec2:CreateTags`, `ec2:DescribeImages`, `ec2:DescribeSubnets`,
`ec2:DescribeSecurityGroups`, and `iam:PassRole` for the pod-VM instance
profile (if used). Attach to a dedicated IRSA role for the CAA service account
(preferred) or to `aws_iam_role.node_group`. Note the deploy-operator policy
(`deploy/iam/deploy-operator-policy.json`) may also need these EC2 actions for
the smoke test.
- **Security-group rules.** Add ingress to `aws_security_group.node`
(`modules/eks/main.tf`): **15150/tcp** (agent-protocol-forwarder, worker ↔ pod
VM) and **9000/tcp+udp** (vxlan). Pod VMs launch into the agent subnets (so
they can reach the EFS mount targets and the worker), so scope the rules to
the VPC/agent CIDRs.
- **Pod VM AMI (`PODVM_AMI_ID`).** Two sourcing options, decide in §6:
  1. **Prebuilt CoCo community pod-VM image** published for `us-east-2` (fastest
    to a working smoke test; pin the exact AMI id, provenance unverified).
  2. **Self-built** pod-VM image with `TEE_PLATFORM=amd` from the
    `cloud-api-adaptor` image-builder (full provenance/pinning control; more
     up-front work). Recommend (1) to prove the path, (2) for production.
- `**_lib.sh regenerate_tfvars`**: if we keep the node group as a zeroable
resource, the existing `confidential_node_*` tfvars still emit (now `0`). If
we add CAA-related tfvars (e.g. pod-VM AMI, instance type, extra SG ports),
add them to the heredoc alongside the existing `confidential_node_*` block.

### Phase 4 — Deploy scripts (numbered rollout `02`/`03`/`05`)

- `**02-snp-node-group.sh**`: with `CONFIDENTIAL_NODE_DESIRED_COUNT=0` this
becomes "ensure no confidential metal node group / normal workers ready". Keep
the EFS-replacement hazard guard (`plan_replaces_efs`) and the
`eks:DescribeUpdate` fallback — they protect the shared filesystem regardless
of runtime. The SNP-node Ready/taint verification at the tail is replaced by a
"no confidential node group present (or scaled to 0)" check.
- `**03-coco-operator.sh` → CAA Helm install.** Replace the CoCo-operator
kustomize apply + `CcRuntime` CR + `09-runtime-class.yaml`/`10-coco-ccruntime.yaml`
with `helm install cloud-api-adaptor` (pinned chart version), configured with
`PODVM_AMI_ID`, `PODVM_INSTANCE_TYPE`, `DISABLECVM=false`, `VXLAN_PORT=9000`,
the AWS creds/role secret, and subnet/SG ids. Verify: CAA daemonset Ready on
the workers and the `**kata-remote` RuntimeClass exists** (replacing the
`kata-qemu-snp` check). The `katacontainers.io/kata-runtime=true` node-label
wait is dropped (no per-node payload install).
- `**05-snp-smoke-test.sh` → `05-peer-pods-smoke-test.sh`.** Keep the scratch
KBS provisioning (write the random DEK into the `kbs-repository` PVC) and the
CDH fetch logic, but:
  - launch the throwaway pod with `runtimeClassName: kata-remote` and **no** SNP
  nodeSelector/toleration;
  - assert a real EC2 pod VM is **created** during the run
  (`aws ec2 describe-instances` filtered by the CAA pod-VM tag) and the
  in-guest CDH fetches the scratch DEK (attestation-gated release succeeds);
  - assert the pod VM is **terminated** after the pod is deleted (no leaked
  instances → no silent cost/quota leak). This last check is the key new
  assertion vs the on-node test.
- `**README.md` rollout table**: update steps 02/03/05 descriptions, the step-02
rollback note (scale CAA / pod-VM AMI vs node group), and add a "Peer Pods /
CAA" shared-assets note. (Per the task, `README.md` is not edited in this
change — flagged here as a Phase-4 follow-up.)

### Phase 5 — KBS / Trustee (`04-trustee-kbs.sh`, attestation policy)

- The Trustee KBS + AS and the `.secrets/kbs-admin.key` ↔ `kbs-auth-public-key`
flow are **unchanged** — the KBS still lives in `swarm-system`, still releases
the per-agent DEK after SNP attestation. The CDH now runs in the AWS pod VM
instead of an on-node guest, but the RCAR handshake and resource path
(`swarm/agents/{id}/state-key`) are identical.
- **Verify the attestation policy accepts peer-pod SNP evidence.** Confirm the
AS reference values / policy match the AWS-managed pod VM's launch
measurement and firmware (OVMF/IGVM) rather than our on-node guest stack.
Expect reference-value differences: the pod-VM image and firmware are
AWS/CoCo-built, so any pinned launch-measurement or `policy.rego` tuned to the
on-node stack will need re-baselining. Confirm KBS↔CAA version compatibility
(CDH/AS protocol) before cutover (§6).
- **Attestation on AWS managed SEV-SNP is confirmed available (verified 2026).**
  AWS supports requesting an SNP attestation report from the guest
  (`/dev/sev-guest`, `SNP_GET_REPORT` / `SNP_GET_EXT_REPORT`, `snpguest`); the
  report is signed by a **VLEK** that AMD issues for AWS and chains to the AMD
  root of trust, so it is hardware-rooted and third-party verifiable. Trustee's
  AS ships a native `snp` verifier driver and has been run against AWS SEV-SNP
  (`m6a.large`) with the KBS retrieving and verifying the report — so our
  CDH→KBS→per-agent-DEK→sealed-EFS flow works unchanged inside a managed-SNP pod
  VM. Refs: AWS "Attest an EC2 instance with AMD SEV-SNP"; confidential-containers
  trustee AS `snp` driver / issue #699.
  - **Caveat 1 — measurement reproducibility.** AWS does not publicly document
    how the launch digest is generated, so pinning the AS reference value to an
    exact expected measurement is hard; plan to baseline it empirically (verify
    VLEK→AMD-root authenticity, accept/observe the AWS measurement) rather than
    pre-computing it.
  - **Caveat 2 — only boot firmware is measured.** The SNP report covers initial
    firmware/vCPU state, **not** the kernel/initrd/cmdline/workload. Measuring
    those needs NitroTPM, and there is **no cryptographic link** between the SNP
    report and the NitroTPM quote — so workload measured-boot is weaker than the
    on-node kata model. Decide whether VM-genuineness attestation is sufficient
    for the v0.2.0 threat model, or whether to pin the pod-VM image another way.
  - **Constraints:** managed SEV-SNP is limited to M6a/C6a/R6a non-metal sizes,
    `us-east-2`/`eu-west-1` only, UEFI-boot AMI (AL2023/RHEL 9.3/SLES 15 SP4/
    Ubuntu 23.04+), plus the +10% hourly fee. We are in `us-east-2`.

---

## 5. Rollout order (operator, after Phases 1–5 land)

1. Land scheduler code (Phase 1–2) with `CONFIDENTIAL_RUNTIME=snp_local` so
  behavior is **unchanged** on deploy (no fleet churn yet).
2. Source `PODVM_AMI_ID` (prebuilt community image for `us-east-2` to start) and
  apply Phase-3 terraform (CAA IAM, SG ports 15150/9000); keep the metal node
   group until proven.
3. `./03-coco-operator.sh` (CAA Helm variant) → CAA daemonset Ready,
  `kata-remote` RuntimeClass exists.
4. `./05-peer-pods-smoke-test.sh` → pod VM created, attests, fetches scratch DEK
  via CDH, pod VM terminated → `STEP 05 OK`. **Do not proceed until green.**
5. Resolve the **EFS/sealed-state** question (§6) with a single real agent
  (sealed state read/written from inside a pod VM) before any fleet move.
6. Flip `CONFIDENTIAL_RUNTIME=peer_pods` and redeploy the scheduler. The
  desired-state reconciler churns the fleet one pod per pass onto `kata-remote`;
   monitor pod-VM count, KBS load, and EFS I/O.
7. Once stable, `./02-`* with `CONFIDENTIAL_NODE_DESIRED_COUNT=0` to retire the
  metal node group. Keep the fallback switch and the SNP-AMI plan on the shelf.

---

## 6. Open questions / unknowns (resolve before coding where noted)

1. **EFS sealed-state in an off-cluster pod VM — biggest open risk.** Today
  every agent pod mounts the `swarm-agent-state` EFS PVC at `/state` by
   `subPath {agent_id}` (`pod.rs build_state_volume`/`build_state_mount`), and
   the harness seals values under the DEK before writing there. With Peer Pods
   the workload runs in the **AWS pod VM**, not on the worker — so **how does the
   per-agent EFS subPath get into the pod VM?** Unknowns: (a) does CAA's volume
   handling mount the CSI/NFS volume on the worker and forward it to the pod VM,
   or must the pod VM mount EFS (NFS 2049) directly? (b) EFS mount targets today
   allow 2049 only from the **agent/private subnet CIDRs**
   (`modules/storage/main.tf`), so a pod VM must launch into an allowed subnet
   **and** the EFS SG must admit it; (c) does `subPath` semantics survive the
   peer-pod volume path? If CSI subPath mounts do **not** transparently work
   through CAA, we need an alternative (pod VM mounts EFS directly with a
   per-agent access point, or the harness talks to a state service) — a
   potentially large redesign. **This must be answered with a real pod VM before
   the fleet move (Rollout step 5).**
2. **CAA ↔ current KBS/Trustee compatibility.** Does the pod-VM CDH/attester
  version released with our chosen CAA chart speak the same AS/KBS protocol as
   our deployed Trustee? Pin compatible versions.
3. **Pod-VM image provenance & pinning.** Prebuilt community AMI (fast, weaker
  provenance) vs self-built `TEE_PLATFORM=amd` (full control). Production
   should pin a self-built, measured image; decide the build/ownership story.
4. **Attestation policy re-baseline.** *Attestation itself is confirmed working
   on AWS managed SEV-SNP (VLEK→AMD root; Trustee `snp` verifier) — see Phase 5.*
   The remaining question is the policy: AWS does not document how the launch
   digest is generated, so decide whether to (a) verify only VLEK/AMD-root
   authenticity + accept the observed AWS measurement, or (b) pin a specific
   measurement baselined empirically, and how to do so without bricking releases.
   Note the SNP report does not cover kernel/initrd/workload (Phase 5 caveat 2).
5. **Networking with the existing VPC/CNI.** vxlan (9000) and
  agent-protocol-forwarder (15150) between worker and pod VM across the agent
   subnets, with the VPC CNI in play — confirm no MTU/SG/NACL surprises and that
   pod VMs get addresses in an EFS-reachable subnet.
6. **Cost/quota guardrails.** Per-pod EC2 instances consume On-Demand vCPU quota
  and can leak if termination fails; the smoke test asserts termination, but we
   also want a reaper/alarm for orphaned pod VMs.

---

## 7. Risks


| Risk                                                                                                   | Mitigation                                                                                                                                                                                                  |
| ------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **EFS sealed state can't be mounted in the pod VM** (or `subPath` breaks) — could block the whole plan | Prove with one real agent at Rollout step 5 **before** fleet move; fallback `CONFIDENTIAL_RUNTIME=snp_local`; redesign options (direct NFS mount + per-agent access point, or state service) scoped in §6.1 |
| Whole-fleet churn when desired class flips to `kata-remote`                                            | Intentional; paced one-pod-per-pass by the reconciler; do it as a monitored migration with pod-VM/KBS/EFS capacity watch                                                                                    |
| Attestation policy rejects peer-pod SNP evidence                                                       | Re-baseline AS reference values/policy for the pod-VM measurement (Phase 5); validate in the smoke test before traffic                                                                                      |
| CAA ↔ KBS version mismatch                                                                             | Pin compatible CAA chart + Trustee versions; verify CDH/AS protocol in §6.2 before cutover                                                                                                                  |
| Orphaned pod VMs (termination failure) → cost/quota leak                                               | Smoke test asserts create **and** terminate; add an orphan reaper + CloudWatch alarm; per-pod On-Demand vCPU quota review                                                                                   |
| Pod-VM image provenance (community AMI)                                                                | Start on pinned community AMI for the smoke test; move to self-built `TEE_PLATFORM=amd` measured image for prod                                                                                             |
| Networking (vxlan/forwarder/EFS) across agent subnets                                                  | SG rules 15150 + 9000 scoped to VPC; launch pod VMs into EFS-reachable subnets; validate MTU/NACL early                                                                                                     |
| Untestable without AWS                                                                                 | Code paths are unit-tested with `peer_pods` cases; end-to-end is unverifiable until applied on AWS (same caveat as the AMI plan)                                                                            |


---

## 8. Effort

- **Code (Phases 1–2):** small–moderate — configurable confidential runtime
class + selector/toleration gating + tests in `aura-swarm-scheduler` /
`aura-swarm-store`. One sitting; unit-testable locally.
- **Infra/deploy (Phases 3–5):** moderate — CAA Helm install replacing the CoCo
operator, IAM/SG additions, pod-VM AMI sourcing, and a rewritten peer-pods
smoke test. Unverifiable until applied on AWS.
- **First working Peer Pod end-to-end:** few hours–couple days (CAA install +
AMI + networking + attestation policy re-baseline + smoke test).
- **The EFS/sealed-state question (§6.1) is the schedule risk:** if subPath EFS
does not pass through CAA cleanly, the redesign could dominate the effort —
resolve it first. This is a **larger migration than the AMI swap**; the
fallback switch is what makes it safe to attempt.

