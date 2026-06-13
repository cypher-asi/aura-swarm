# Plan: SEV-SNP on AWS metal via a custom SNP-host AMI (Image Builder)

Status: **proposed** · Owner: deploy · Architecture: keep `kata-qemu-snp` on the
M6A/M7A **metal** confidential node group.

---

## 1. Problem

The staged TEE rollout fails at **Step 05 (SNP attestation smoke test)** when a
`kata-qemu-snp` pod tries to boot a confidential VM:

```
failed to create shim task: This system doesn't support Confidential Computing (Guest Protection)
```

### Root cause
`kata-qemu-snp` runs the confidential VM **on the node**, so the node's own
hypervisor must be able to launch AMD SEV-SNP guests. A **stock** `m6a.metal`
EKS node cannot:

- The default EKS AL2023 AMI has **no SNP-host kernel** (needs Linux 6.11+ with
  SNP host support).
- AWS Nitro requires a **CCP-deferral workaround** before SNP firmware init
  works on bare metal.
- AWS's *managed* SEV-SNP is **guest-only** (per-pod via Peer Pods), not a host
  capability for the node.

This is a platform capability gap, not a config/code bug.

### Confirmed viable
AWS publishes a validated reference that makes SNP host work on `m6a.metal`:
[`aws-samples/howto-runtime-attestation-on-aws`](https://github.com/aws-samples/howto-runtime-attestation-on-aws)
— Ubuntu EKS AMI + kernel 6.11.5 + AMDSEV-patched KVM/OVMF + the Nitro CCP
workaround. Success check: `snphost ok` → `[ PASS ]` and
`/sys/module/kvm_amd/parameters/sev_snp = Y`.

---

## 2. Chosen approach (Option 1)

Keep the architecture; replace **only** the confidential node group's AMI with an
SNP-host image built by the **AWS EC2 Image Builder pipeline** (automated, not a
hand-compiled kernel). The image is selected via a single config value
(`CONFIDENTIAL_NODE_AMI_ID`). Everything is a no-op until that value is set.

### Alternatives considered
| Option | Custom AMI | Confidential | Notes |
|---|---|---|---|
| **1. SNP-host AMI on metal (this plan)** | Yes (automated build) | ✅ | Keeps current architecture; ongoing AMI ownership |
| 2. `kata-qemu` on stock metal | No | ❌ | VM isolation only, no TEE/attestation |
| 3. Peer Pods / CAA | No | ✅ (not metal) | Larger runtime rework; AWS owns the SNP host |

---

## 3. Work breakdown

### Phase 0 — Build the AMI (operator, AWS account) — prerequisite
From `aws-samples/howto-runtime-attestation-on-aws`:
1. `cd cdk && npm install && cdk bootstrap`
2. `cdk deploy RuntimeAttestationImageBuilderStack…`
3. EC2 Image Builder console → run the **"Ubuntu SEV-SNP Host image on EKS"**
   pipeline (~20–40 min).
4. Capture the output **AMI ID** (EKS-worker variant, EKS 1.31, region `us-east-2`).

**Output:** `CONFIDENTIAL_NODE_AMI_ID=ami-…`

### Phase 1 — Terraform (code)
Files: `terraform/variables.tf`, `terraform/modules/eks/variables.tf`,
`terraform/main.tf`, `terraform/modules/eks/main.tf`,
new `terraform/modules/eks/snp-userdata.sh.tftpl`.

- Add `confidential_node_ami_id` variable (default `""`) to root + eks module;
  pass through the module call.
- New `aws_launch_template.confidential` (`count = ami_id != "" ? 1 : 0`):
  - `image_id = var.confidential_node_ami_id`
  - `block_device_mappings` → `/dev/sda1` (Ubuntu root), `confidential_node_disk_size`, gp3
  - `user_data` = Ubuntu **EKS bootstrap**
    (`/etc/eks/bootstrap.sh <cluster> --apiserver-endpoint … --b64-cluster-ca … --dns-cluster-ip …`)
    derived from the cluster endpoint / CA / service CIDR. The SNP kernel cmdline
    is baked into the AMI.
  - `metadata_options` (IMDSv2, hop limit 2) + instance tags.
- Gate `aws_eks_node_group.confidential` onto it:
  - `dynamic "launch_template"` block when the AMI is set.
  - `disk_size = local.custom ? null : var.confidential_node_disk_size` (disk moves to LT).
  - keep `instance_types` on the node group (LT omits instance type → no conflict).

**Safety:** with `confidential_node_ami_id = ""` the plan is byte-identical to today.

### Phase 2 — config + tfvars (code)
Files: `config.env`, `_lib.sh` (`regenerate_tfvars`).
- `CONFIDENTIAL_NODE_AMI_ID="${CONFIDENTIAL_NODE_AMI_ID:-}"`.
- Emit `confidential_node_ami_id = "…"` into `terraform.tfvars`.

### Phase 3 — SNP host verification (code)
Files: `_lib.sh` + step 02 (post node-join) and step 05 (pre-smoke).
- `verify_snp_host`: short privileged pod on the SNP node checks
  `/sys/module/kvm_amd/parameters/sev_snp == Y` and `/dev/sev` present.
- Fail fast with a clear message
  ("node lacks SEV-SNP host support — confirm CONFIDENTIAL_NODE_AMI_ID")
  instead of the cryptic step-05 shim error.

### Phase 4 — version alignment (decision + maybe pin)
- AWS validated **CoCo 0.11 / kata 3.11** with the 6.11 SNP-host kernel; we run
  **CoCo operator 0.17 / kata-deploy 3.23**.
- Try current pins first (newer kata generally supports SNP host). If the SNP
  shim misbehaves, fall back to the validated combo via the existing
  `COCO_OPERATOR_VERSION` + kata-deploy tag variables. Document both.

### Phase 5 — docs
File: `deploy/README.md`.
- "Confidential node AMI" section: the Image Builder runbook, EKS-worker AMI +
  region/EKS-version matching, version matrix, and the rollout order below.

---

## 4. Rollout order (operator, after Phases 1–5 land)
1. Build AMI (Phase 0) → set `CONFIDENTIAL_NODE_AMI_ID`.
2. `./deploy/02-snp-node-group.sh` → terraform replaces the confidential node
   group onto the LT/custom AMI (existing `DescribeUpdate` + bare-metal-quota
   handling apply), waits for the node, **verifies SNP host**.
3. `./deploy/03-coco-operator.sh` → kata install (self-reconciling).
4. `./deploy/05-snp-smoke-test.sh` → confidential VM boots, attests, fetches the
   DEK → `STEP 05 OK`.

---

## 5. Risks
| Risk | Mitigation |
|---|---|
| LT + Ubuntu-EKS bootstrap untestable without AWS | Gated by var (no impact until set); Phase 3 catches a bad join early |
| Node-group replacement churn (AMI type change) | Expected; quota/`DescribeUpdate` handling already covers replace |
| kata/CoCo ↔ kernel mismatch | Phase 4 fallback to AWS-validated 0.11/3.11 |
| Ongoing AMI maintenance (kernel CVEs, EKS upgrades) | Rebuild pipeline on upgrades; documented in README |
| Standard On-Demand vCPU quota (384) vs 192/metal | One metal fits; avoid 2 concurrent metals during replace, or raise `L-1216C47A` |

---

## 6. Effort
- **Code (Phases 1–5):** small–moderate, one sitting; **unverifiable until applied on AWS**.
- **First working SNP node end-to-end:** few hours–couple days (AMI build + LT/bootstrap debugging + re-run 02→05).
- **Steady state:** AMI-rebuild overhead added to each EKS/kernel upgrade.
