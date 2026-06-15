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
registry credentials. `./03-coco-operator.sh` now provisions them (an ECR pull
secret linked to the agent `default` SA, plus a refresher CronJob); if that step
has not run, every pull of the private `aura-swarm-dev-harness` image returns
`Not authorized`.

### Implemented fix: a pull secret linked to the agent ServiceAccount

`./03-coco-operator.sh` now seeds this automatically (`ensure_agent_ecr_pull_secret`
in [`_lib.sh`](_lib.sh) + the refresher manifest
[`k8s/agent-ecr-pull-refresher.yaml`](k8s/agent-ecr-pull-refresher.yaml)). The
kata-agent forwards the pod's `imagePullSecrets` to the in-guest image-rs, and the
scheduler sets no `serviceAccountName`, so linking the secret to the `default` SA
in `swarm-agents` covers every agent pod with no scheduler change.

What step 03 does (and what to run by hand if you are not re-running 03):

```bash
REG=909208902094.dkr.ecr.us-east-2.amazonaws.com
NS=swarm-agents
SECRET=ecr-harness-pull

# 1. seed/refresh the dockerconfigjson secret (ECR token is valid 12h)
kubectl create secret docker-registry "$SECRET" -n "$NS" \
  --docker-server="$REG" --docker-username=AWS \
  --docker-password="$(aws ecr get-login-password --region us-east-2)" \
  --dry-run=client -o yaml | kubectl apply -f -

# 2. link it to the default SA the agent pods run as
kubectl patch serviceaccount default -n "$NS" \
  -p '{"imagePullSecrets":[{"name":"'"$SECRET"'"}]}'
```

#### Token refresh (12h expiry)

ECR tokens expire after 12 hours, so step 03 also installs the
`ecr-pull-refresher` CronJob (default `0 */6 * * *`, override with
`AGENT_ECR_PULL_REFRESH_SCHEDULE`). It authenticates to AWS via the **worker node
instance role over IMDS** (`hostNetwork: true`; the node role already carries
`AmazonEC2ContainerRegistryReadOnly`), so there is no IRSA role and no static key.
Verify it:

```bash
kubectl -n swarm-agents get cronjob ecr-pull-refresher
kubectl -n swarm-agents create job --from=cronjob/ecr-pull-refresher ecr-refresh-test  # manual tick
```

### Alternative paths (not used here)

- **KBS-provided auth file via initdata.** Store the `auth.json` as an
  attestation-gated KBS resource and pass `cc_init_data` (`cdh.toml`) on each pod.
  Heavier (needs the scheduler to emit `cc_init_data`), but releases the
  credential only after attestation. See
  [`05-peer-pods-smoke-test.sh`](05-peer-pods-smoke-test.sh) lines ~423-466 for the
  initdata mechanics.
- **Pod-VM IAM instance profile + ECR credential provider.** Bake an ECR
  credential helper into a custom pod-VM AMI and attach an instance profile with
  `ecr:GetAuthorizationToken`, so the guest mints its own token (no secret, no
  refresh CronJob). No 12h expiry to manage, but requires a custom AMI; pin it via
  `PODVM_AMI_ID`.

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

## D. How this is wired permanently

The fix in Part B is built into the deploy flow, so steady-state needs no manual
steps:

- [`_lib.sh`](_lib.sh) `ensure_agent_ecr_pull_secret` seeds the secret + SA link.
- [`k8s/agent-ecr-pull-refresher.yaml`](k8s/agent-ecr-pull-refresher.yaml) refreshes
  the 12h token via the worker node role (IMDS).
- [`03-coco-operator.sh`](03-coco-operator.sh) calls both, so re-running step 03
  (the doctor's standard remediation) re-establishes everything.
- [`peerpods-doctor.sh`](peerpods-doctor.sh) `agent-pull-secret` layer flags a
  missing/unlinked secret proactively.
- Knobs: `AGENT_ECR_PULL_SECRET_NAME`, `AGENT_ECR_PULL_REFRESH_SCHEDULE` in
  [`config.env`](config.env).

An alternative, attestation-gated path (KBS auth file via `cc_init_data`, which
would need the scheduler to emit the annotation in
[`crates/aura-swarm-scheduler/src/pod.rs`](../crates/aura-swarm-scheduler/src/pod.rs))
remains possible if you later want the credential released only post-attestation.
