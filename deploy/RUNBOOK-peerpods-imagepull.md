# Runbook — Peer Pods image pull + pod-VM scheduling failures

Remediation for confidential (kata-remote / Peer Pods) agents that fail to start
because the **in-guest image pull cannot authenticate to the private ECR harness
repo** and/or the **pod-VM subnet is exhausted**. All commands are read-only
diagnosis first, then the fix — run them yourself (this runbook does not run
anything).

> Read [`peerpods-doctor.sh`](peerpods-doctor.sh) output first. After the
> diagnostic fixes shipped alongside this runbook, the doctor reports
> `caa-daemonset` (not the downstream `runtimeclass-overhead`) as the root layer
> and surfaces the ECR/subnet signatures in its `<node>/caa-log` layer.

The two failure signatures this runbook addresses, both seen in the CAA pod logs
(`kubectl -n confidential-containers-system logs <caa-pod> [--previous]`):

```
# (current) in-guest CDH cannot pull the private image
CreateContainer fails: ... [CDH] [ERROR]: Image Pull error: Failed to pull image
  909208902094.dkr.ecr.us-east-2.amazonaws.com/aura-swarm-dev-harness@sha256:...
  ... Not authorized: url https://...amazonaws.com/v2/aura-swarm-dev-harness/manifests/sha256:...

# (previous) pod VM could not even launch — subnet full
[adaptor/cloud] error starting instance: ... operation error EC2: RunInstances ...
  InsufficientFreeAddressesInSubnet: There are not enough free addresses in subnet 'subnet-...'
```

---

## A. Subnet exhaustion (`InsufficientFreeAddressesInSubnet`)

CAA launches one EC2 pod VM per confidential agent, each needing an IP in the
subnet named by `AWS_SUBNET_ID` in `peer-pods-cm`. If that points at an agent
`/25` (shared with the AWS VPC CNI, which also consumes IPs for every cluster
pod) it runs out of addresses. The fix is to point CAA at the dedicated, large,
untagged `/22` pod-VM subnet (`*-podvm-*`, `aura.swarm/pool=tee-podvm`).

### A1. Identify which subnet CAA is using

```bash
# subnet CAA hands to RunInstances
kubectl -n confidential-containers-system get cm peer-pods-cm \
  -o jsonpath='{.data.AWS_SUBNET_ID}{"\n"}'

# what terraform created (dedicated pod-VM subnets vs the small agent subnets)
cd deploy/terraform
terraform output podvm_subnet_ids
terraform output agent_subnet_ids
```

If `AWS_SUBNET_ID` matches an **agent** subnet (or `podvm_subnet_ids` is empty),
that is the bug.

### A2. Ensure the dedicated pod-VM subnets exist, then re-point CAA

```bash
# create the dedicated /22 pod-VM subnets if terraform output podvm_subnet_ids is empty
cd deploy/terraform
terraform apply   # network module creates the untagged *-podvm-* subnets

# re-run 03: it reads `tf_output podvm_subnet_ids` and sets AWS_SUBNET_ID to the
# first dedicated subnet (see 03-coco-operator.sh, CAA_PODVM_SUBNET_ID logic).
cd ..
./03-coco-operator.sh
```

To pin a specific subnet without terraform, set `CAA_PODVM_SUBNET_ID=subnet-...`
in `config.env` before re-running `./03-coco-operator.sh`.

### A3. Reclaim leaked pod VMs squatting addresses

A crash-looping CAA can leave orphaned pod VMs that still hold IPs. List them and
terminate any whose backing pod is gone:

```bash
# running pod VMs (CAA names them podvm-<pod-name>-<id>)
aws ec2 describe-instances --region us-east-2 \
  --filters "Name=tag:Name,Values=podvm-*" \
            "Name=instance-state-name,Values=pending,running" \
  --query 'Reservations[].Instances[].{id:InstanceId,name:Tags[?Key==`Name`]|[0].Value,launched:LaunchTime,subnet:SubnetId}' \
  --output table

# cross-check against live pods; terminate orphans (replace with the real ids)
aws ec2 terminate-instances --region us-east-2 --instance-ids i-xxxx i-yyyy
```

---

## B. Private ECR guest-pull auth (`[CDH] Image Pull error ... Not authorized`)

For Peer Pods the **workload image is pulled inside the pod VM** by the in-guest
CDH / image-rs (driver `image_guest_pull`), **not** on the worker. The worker
node-role's ECR permission is therefore irrelevant — the guest needs its own
registry credentials. Nothing in the deploy chain currently provisions them
(the `05` smoke test only ever pulls a public image with `credentials = []`), so
every pull of the private `aura-swarm-dev-harness` image returns `Not authorized`.

### Recommended: hand the guest an ECR auth file via the KBS, referenced by initdata

This reuses the existing Trustee/KBS plumbing (same writer-pod technique as
[`05-peer-pods-smoke-test.sh`](05-peer-pods-smoke-test.sh)) so the credential is
released only after attestation.

#### B1. Mint an ECR `auth.json` (docker config format)

```bash
REG=909208902094.dkr.ecr.us-east-2.amazonaws.com
TOKEN=$(aws ecr get-login-password --region us-east-2)
AUTH=$(printf 'AWS:%s' "$TOKEN" | base64 -w0)
cat > /tmp/auth.json <<EOF
{ "auths": { "$REG": { "auth": "$AUTH" } } }
EOF
```

#### B2. Store it as a KBS resource in the `kbs-repository` PVC

The KBS LocalFs backend stores each resource as a single flat file whose name is
the resource path with every `/` replaced by the literal 4-char string `\x2F`
(see the detailed note in [`05-peer-pods-smoke-test.sh`](05-peer-pods-smoke-test.sh)).
Write `default/credentials/auth.json` with a throwaway busybox pod that mounts the
PVC (same pattern the smoke test uses), e.g. on-disk name
`default\x2Fcredentials\x2Fauth.json` under `/opt/confidential-containers/kbs/repository`.

#### B3. Make agent pods carry initdata that references the credential

The guest needs `cc_init_data` whose `cdh.toml` points AA/CDH at the KBS pod IP
(reachable over the VPC from the pod-VM subnet) and enables authenticated image
pull from the stored credential, e.g.:

```toml
[image]
# image-rs fetches the registry auth file from the KBS as an attestation-gated resource
auth = true

[kbc]
name = 'cc_kbc'
url  = 'http://<KBS_POD_IP>:8080'
```

Pass it on the pod as the base64(gzip(TOML)) annotation
`io.katacontainers.config.hypervisor.cc_init_data` (the exact mechanics — KBS pod
IP discovery, the `aa.toml`/`cdh.toml` body, and the gzip|base64 encoding — are
in [`05-peer-pods-smoke-test.sh`](05-peer-pods-smoke-test.sh) lines ~423-466).

> Making this automatic for **every** confidential agent requires the scheduler
> to inject `cc_init_data` (it currently injects only
> `io.containerd.cri.runtime-handler`; see
> [`crates/aura-swarm-scheduler/src/pod.rs`](../crates/aura-swarm-scheduler/src/pod.rs)).
> That is a deliberate agent-pod-spec change — see Part D.

#### B4. Refresh the ECR token (it expires every 12h)

ECR tokens are valid for 12 hours, so a static auth file goes stale. Add a
`CronJob` (every ~6h) that re-runs B1 + B2 to rewrite the KBS credential
resource. Without it, agents created after the token expires fail with the same
`Not authorized`.

### Alternative: pod-VM IAM instance profile + ECR credential provider

Bake an ECR credential helper into a custom pod-VM AMI and attach an instance
profile with `ecr:GetAuthorizationToken` + pull permissions, so the guest mints
its own short-lived token (no KBS resource, no refresh CronJob). Heavier up front
(custom AMI build + IAM), but no 12h expiry to manage. Pin the new AMI via
`PODVM_AMI_ID` in `config.env`.

---

## C. Recover the unhealthy CAA pods and re-verify

After A and/or B, restart the `0/1` CAA pods so they re-initialize cleanly
(`maxSurge=0` means no `:8000` port collision):

```bash
# the not-Ready CAA pods (READY 0/1)
kubectl -n confidential-containers-system get pods -o wide | grep cloud-api-adaptor

kubectl -n confidential-containers-system delete pod <caa-pod-1> <caa-pod-2> <caa-pod-3>

# re-verify end to end
./peerpods-doctor.sh
./05-peer-pods-smoke-test.sh   # boots a billable pod VM; override SMOKE_* to exercise the private image
```

Expected after the fix: `caa-daemonset` 6/6, `runtimeclass-overhead` PASS, and
`<node>/caa-log` clean (no `Not authorized` / `InsufficientFreeAddressesInSubnet`).

---

## D. Optional follow-up — make ECR auth permanent

To stop doing B by hand for every agent, inject `cc_init_data` (cdh.toml + the
credential-resource reference, plus the KBS pod IP) into the confidential agent
pod spec in
[`crates/aura-swarm-scheduler/src/pod.rs`](../crates/aura-swarm-scheduler/src/pod.rs),
alongside the existing `io.containerd.cri.runtime-handler` annotation. This is a
behavior change to every confidential agent pod, so treat it as a separate,
reviewed change rather than an ad-hoc remediation.
