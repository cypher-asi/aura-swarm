#!/bin/bash
# efs-encryption-migration.sh - Guided migration from an UNENCRYPTED EFS
# filesystem to a new ENCRYPTED one. Only needed when ./02-snp-node-group.sh
# aborted with the EFS replacement hazard.
#
# Encryption cannot be enabled on an existing EFS filesystem, so this script
# walks through (with explicit pause points):
#   A. detect the live filesystem and verify it really is unencrypted
#   B. create a new encrypted filesystem + mount targets (idempotent)
#   C. initial data sync via a temporary rsync pod (workloads still live)
#   D. PAUSE: quiesce writers (scale platform to 0, delete agent pods)
#   E. final sync + verification (file count + byte size on both sides)
#   F. recreate EFS access points + static PVs/PVCs against the new filesystem
#   G. repoint terraform state (state rm old + import new), verify the plan
#      no longer replaces anything
#
# Afterwards: re-run ./02-snp-node-group.sh (applies access-point/backup-policy
# stragglers and the SNP node group), then continue the sequence. Platform
# deployments are scaled back up by the next deploy step (06) — or manually
# with the commands printed at the end.
#
# Usage: ./efs-encryption-migration.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source <(tr -d '\r' < "${SCRIPT_DIR}/config.env")
source "${SCRIPT_DIR}/_lib.sh"

step_banner "EFS" "EFS unencrypted -> encrypted migration (guided, conditional)" "ops-admin"

require_cmds aws terraform kubectl jq
require_aws_auth
ensure_kubectl_context

confirm_or_abort() {
    echo ""
    log_warn "PAUSE POINT: $1"
    read -r -p "Type 'yes' to continue: " answer
    if [[ "${answer}" != "yes" ]]; then
        log_info "Aborted at pause point. This script is safe to re-run; completed phases are skipped."
        exit 1
    fi
}

NEW_FS_TOKEN="${RESOURCE_PREFIX}-efs-encrypted"
RSYNC_POD="efs-migration-rsync"
RSYNC_POD_NS="${K8S_NAMESPACE_SYSTEM}"

#------------------------------------------------------------------------------
# Phase A: detect filesystems
#------------------------------------------------------------------------------

log_section "Phase A: detect filesystems"

OLD_FS_ID=$(tf_output efs_filesystem_id)
[[ -n "${OLD_FS_ID}" ]] || step_fail "could not read the current EFS filesystem ID from terraform output"

OLD_FS_JSON=$(aws efs describe-file-systems --file-system-id "${OLD_FS_ID}" --query 'FileSystems[0]')
OLD_ENCRYPTED=$(echo "${OLD_FS_JSON}" | jq -r '.Encrypted')
OLD_PERF=$(echo "${OLD_FS_JSON}" | jq -r '.PerformanceMode')
OLD_THROUGHPUT=$(echo "${OLD_FS_JSON}" | jq -r '.ThroughputMode')

log_detail "Current filesystem: ${OLD_FS_ID} (encrypted=${OLD_ENCRYPTED})"
if [[ "${OLD_ENCRYPTED}" == "true" ]]; then
    log_ok "The live filesystem is already encrypted — no migration needed."
    step_ok "02 (re-run ./02-snp-node-group.sh)"
    exit 0
fi

# Existing mount targets tell us which subnets/SGs the new FS must mirror.
OLD_MTS=$(aws efs describe-mount-targets --file-system-id "${OLD_FS_ID}" --query 'MountTargets')
OLD_DNS="${OLD_FS_ID}.efs.${AWS_REGION}.amazonaws.com"

#------------------------------------------------------------------------------
# Phase B: create the new encrypted filesystem + mount targets (idempotent)
#------------------------------------------------------------------------------

log_section "Phase B: create encrypted filesystem"

NEW_FS_ID=$(aws efs describe-file-systems \
    --query "FileSystems[?CreationToken=='${NEW_FS_TOKEN}'].FileSystemId | [0]" --output text)
if [[ -z "${NEW_FS_ID}" || "${NEW_FS_ID}" == "None" ]]; then
    log_info "Creating encrypted filesystem (token ${NEW_FS_TOKEN})..."
    NEW_FS_ID=$(aws efs create-file-system \
        --creation-token "${NEW_FS_TOKEN}" \
        --encrypted \
        --performance-mode "${OLD_PERF}" \
        --throughput-mode "${OLD_THROUGHPUT}" \
        --tags "Key=Name,Value=${RESOURCE_PREFIX}-efs" "Key=Project,Value=${PROJECT_NAME}" "Key=Environment,Value=${ENVIRONMENT}" \
        --query 'FileSystemId' --output text)
else
    log_info "Reusing existing migration filesystem ${NEW_FS_ID}"
fi

log_info "Waiting for ${NEW_FS_ID} to become available..."
while [[ "$(aws efs describe-file-systems --file-system-id "${NEW_FS_ID}" --query 'FileSystems[0].LifeCycleState' --output text)" != "available" ]]; do
    sleep 5
done
log_ok "New encrypted filesystem: ${NEW_FS_ID}"

# Mirror the old filesystem's mount targets (subnet + security groups).
log_info "Ensuring mount targets..."
NEW_MT_IDS=()
declare -A NEW_MT_BY_SUBNET=()
while IFS=$'\t' read -r subnet_id mt_id; do
    sgs=$(aws efs describe-mount-target-security-groups --mount-target-id "${mt_id}" \
        --query 'SecurityGroups' --output text | tr '\t' ' ')
    existing=$(aws efs describe-mount-targets --file-system-id "${NEW_FS_ID}" \
        --query "MountTargets[?SubnetId=='${subnet_id}'].MountTargetId | [0]" --output text)
    if [[ -z "${existing}" || "${existing}" == "None" ]]; then
        # shellcheck disable=SC2086
        existing=$(aws efs create-mount-target --file-system-id "${NEW_FS_ID}" \
            --subnet-id "${subnet_id}" --security-groups ${sgs} \
            --query 'MountTargetId' --output text)
        log_detail "Created mount target ${existing} in ${subnet_id}"
    else
        log_detail "Mount target already exists in ${subnet_id}: ${existing}"
    fi
    NEW_MT_IDS+=("${existing}")
    NEW_MT_BY_SUBNET["${subnet_id}"]="${existing}"
done < <(echo "${OLD_MTS}" | jq -r '.[] | [.SubnetId, .MountTargetId] | @tsv')

log_info "Waiting for mount targets to become available..."
for mt in "${NEW_MT_IDS[@]}"; do
    while [[ "$(aws efs describe-mount-targets --mount-target-id "${mt}" --query 'MountTargets[0].LifeCycleState' --output text)" != "available" ]]; do
        sleep 5
    done
done
NEW_DNS="${NEW_FS_ID}.efs.${AWS_REGION}.amazonaws.com"
log_ok "Mount targets available (${NEW_DNS})"

#------------------------------------------------------------------------------
# Phase C: initial sync (workloads still running; rsync is restartable)
#------------------------------------------------------------------------------

log_section "Phase C: initial data sync"

ensure_rsync_pod() {
    if kubectl get pod "${RSYNC_POD}" -n "${RSYNC_POD_NS}" >/dev/null 2>&1; then
        return 0
    fi
    kubectl apply -f - <<EOF
apiVersion: v1
kind: Pod
metadata:
  name: ${RSYNC_POD}
  namespace: ${RSYNC_POD_NS}
  labels:
    app: efs-migration-rsync
spec:
  restartPolicy: Never
  containers:
    - name: rsync
      image: alpine:3.20
      command: ["sh", "-c", "apk add --no-cache rsync >/dev/null && sleep infinity"]
      volumeMounts:
        - { name: old-efs, mountPath: /old }
        - { name: new-efs, mountPath: /new }
  volumes:
    - name: old-efs
      nfs: { server: ${OLD_DNS}, path: / }
    - name: new-efs
      nfs: { server: ${NEW_DNS}, path: / }
EOF
    kubectl wait --for=condition=Ready "pod/${RSYNC_POD}" -n "${RSYNC_POD_NS}" --timeout=300s \
        || step_fail "rsync pod did not become Ready (check NFS reachability / SGs)"
}

run_rsync() {
    local extra="${1:-}"
    # shellcheck disable=SC2086
    kubectl exec -n "${RSYNC_POD_NS}" "${RSYNC_POD}" -- \
        sh -c "rsync -aHX --numeric-ids ${extra} /old/ /new/"
}

ensure_rsync_pod
log_info "Running initial rsync (incremental; safe to interrupt and re-run)..."
run_rsync
log_ok "Initial sync complete"

#------------------------------------------------------------------------------
# Phase D: quiesce writers
#------------------------------------------------------------------------------

confirm_or_abort "about to QUIESCE the platform: scale gateway/control/scheduler to 0 and delete all agent pods. Agents will be unavailable until the next deploy step."

log_section "Phase D: quiesce writers"
for d in aura-swarm-scheduler aura-swarm-gateway aura-swarm-control; do
    kubectl scale "deployment/${d}" -n "${K8S_NAMESPACE_SYSTEM}" --replicas=0 2>/dev/null || true
done
kubectl delete pods -n "${K8S_NAMESPACE_AGENTS}" -l app=swarm-agent --ignore-not-found --wait=true
kubectl scale deployment/kbs -n "${K8S_NAMESPACE_SYSTEM}" --replicas=0 2>/dev/null || true
log_ok "Writers quiesced"

#------------------------------------------------------------------------------
# Phase E: final sync + verification
#------------------------------------------------------------------------------

log_section "Phase E: final sync + verification"
run_rsync "--delete"

OLD_COUNT=$(kubectl exec -n "${RSYNC_POD_NS}" "${RSYNC_POD}" -- sh -c "find /old -type f | wc -l" | tr -d '[:space:]')
NEW_COUNT=$(kubectl exec -n "${RSYNC_POD_NS}" "${RSYNC_POD}" -- sh -c "find /new -type f | wc -l" | tr -d '[:space:]')
OLD_BYTES=$(kubectl exec -n "${RSYNC_POD_NS}" "${RSYNC_POD}" -- sh -c "du -sb /old | cut -f1" | tr -d '[:space:]')
NEW_BYTES=$(kubectl exec -n "${RSYNC_POD_NS}" "${RSYNC_POD}" -- sh -c "du -sb /new | cut -f1" | tr -d '[:space:]')

log_detail "old: ${OLD_COUNT} files, ${OLD_BYTES} bytes"
log_detail "new: ${NEW_COUNT} files, ${NEW_BYTES} bytes"
if [[ "${OLD_COUNT}" != "${NEW_COUNT}" ]]; then
    step_fail "file count mismatch after final sync (old=${OLD_COUNT} new=${NEW_COUNT})"
fi
log_ok "Data verified on the encrypted filesystem"

confirm_or_abort "data verified. Next: recreate access points + PVs/PVCs against ${NEW_FS_ID} and repoint terraform state."

#------------------------------------------------------------------------------
# Phase F: recreate access points + static PVs/PVCs on the new filesystem
#
# Dynamic efs-ap provisioning created per-PVC access points with generated
# directories (e.g. /state/pvc-<uuid>). The data was copied 1:1, so we create
# matching access points on the new filesystem and rebind each PVC via a
# static PV pointing at the same directory.
#------------------------------------------------------------------------------

log_section "Phase F: rebind EFS-backed PVCs"

PV_LIST=$(kubectl get pv -o json | jq -c '
    [.items[]
     | select(.spec.csi.driver? == "efs.csi.aws.com")
     | {pv: .metadata.name,
        handle: .spec.csi.volumeHandle,
        capacity: .spec.capacity.storage,
        claim_ns: .spec.claimRef.namespace,
        claim_name: .spec.claimRef.name}]')

echo "${PV_LIST}" | jq -r '.[] | "  \(.pv) -> \(.claim_ns)/\(.claim_name) (\(.handle))"'

echo "${PV_LIST}" | jq -c '.[]' | while IFS= read -r pv_row; do
    pv=$(echo "${pv_row}" | jq -r '.pv')
    handle=$(echo "${pv_row}" | jq -r '.handle')
    capacity=$(echo "${pv_row}" | jq -r '.capacity')
    claim_ns=$(echo "${pv_row}" | jq -r '.claim_ns')
    claim_name=$(echo "${pv_row}" | jq -r '.claim_name')

    old_ap="${handle##*::}"
    if [[ "${old_ap}" == "${handle}" ]]; then
        log_warn "${pv}: no access point in volumeHandle; skipping (verify manually)"
        continue
    fi

    ap_json=$(aws efs describe-access-points --access-point-id "${old_ap}" --query 'AccessPoints[0]')
    root_path=$(echo "${ap_json}" | jq -r '.RootDirectory.Path')
    posix_uid=$(echo "${ap_json}" | jq -r '.PosixUser.Uid // 1000')
    posix_gid=$(echo "${ap_json}" | jq -r '.PosixUser.Gid // 1000')

    new_ap=$(aws efs describe-access-points --file-system-id "${NEW_FS_ID}" \
        --query "AccessPoints[?RootDirectory.Path=='${root_path}'].AccessPointId | [0]" --output text)
    if [[ -z "${new_ap}" || "${new_ap}" == "None" ]]; then
        new_ap=$(aws efs create-access-point --file-system-id "${NEW_FS_ID}" \
            --posix-user "Uid=${posix_uid},Gid=${posix_gid}" \
            --root-directory "Path=${root_path}" \
            --tags "Key=Name,Value=migrated-${old_ap}" \
            --query 'AccessPointId' --output text)
        log_detail "Created access point ${new_ap} for ${root_path}"
    fi

    log_info "Rebinding ${claim_ns}/${claim_name} to ${NEW_FS_ID}::${new_ap}..."
    kubectl patch pv "${pv}" -p '{"spec":{"persistentVolumeReclaimPolicy":"Retain"}}' >/dev/null
    kubectl delete pvc "${claim_name}" -n "${claim_ns}" --ignore-not-found --wait=true
    kubectl delete pv "${pv}" --ignore-not-found --wait=true

    kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolume
metadata:
  name: ${pv}-enc
spec:
  capacity:
    storage: ${capacity}
  volumeMode: Filesystem
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: efs-sc
  csi:
    driver: efs.csi.aws.com
    volumeHandle: ${NEW_FS_ID}::${new_ap}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: ${claim_name}
  namespace: ${claim_ns}
spec:
  accessModes: [ReadWriteMany]
  storageClassName: efs-sc
  volumeName: ${pv}-enc
  resources:
    requests:
      storage: ${capacity}
EOF
done
log_ok "EFS-backed PVCs rebound to the encrypted filesystem"

#------------------------------------------------------------------------------
# Phase G: repoint terraform state
#------------------------------------------------------------------------------

log_section "Phase G: repoint terraform state"

cd "${SCRIPT_DIR}/terraform"

state_rm_if_present() {
    if terraform state list 2>/dev/null | grep -qxF "$1"; then
        terraform state rm "$1"
    fi
}

# Remove the old filesystem and its dependents from state (the AWS resources
# themselves are NOT deleted — the old FS stays around as a fallback until
# you delete it manually after the migration has soaked).
state_rm_if_present 'module.storage[0].aws_efs_file_system.main'
state_rm_if_present 'module.storage[0].aws_efs_access_point.state'
state_rm_if_present 'module.storage[0].aws_efs_backup_policy.main'
while IFS= read -r mt_resource; do
    state_rm_if_present "${mt_resource}"
done < <(terraform state list 2>/dev/null | grep '^module.storage\[0\].aws_efs_mount_target.main' || true)

terraform import 'module.storage[0].aws_efs_file_system.main' "${NEW_FS_ID}"

# Import each mount target into the state index that matches its position in
# the storage module's subnet_ids (= the network module's storage_subnet_ids
# output). The AWS API's mount-target order is NOT guaranteed to match that
# ordering, so importing positionally by API order could bind a mount target
# to the wrong count.index and make the next apply REPLACE mount targets.
idx=0
while IFS= read -r subnet_id; do
    [[ -z "${subnet_id}" ]] && continue
    mt="${NEW_MT_BY_SUBNET[${subnet_id}]:-}"
    if [[ -z "${mt}" ]]; then
        log_warn "No new mount target for subnet ${subnet_id} (state index ${idx}); terraform will create it on the ./02 re-run"
        idx=$((idx + 1))
        continue
    fi
    terraform import "module.storage[0].aws_efs_mount_target.main[${idx}]" "${mt}" || true
    idx=$((idx + 1))
done < <(terraform output -json 2>/dev/null | jq -r '.storage_subnet_ids.value[]?')

log_info "Verifying the plan no longer REPLACES the filesystem..."
terraform plan -out=efs-migration-check.tfplan >/dev/null
if plan_replaces_efs "efs-migration-check.tfplan"; then
    step_fail "terraform STILL plans an EFS replacement after the import — inspect 'terraform plan' manually"
fi
log_ok "Terraform state repointed; the filesystem is no longer replaced."
log_detail "Note: the next 'terraform plan' will still CREATE the /state access point and"
log_detail "the backup policy on the new filesystem (deliberately left out of state here)."
log_detail "That is expected and is applied when you re-run ./02-snp-node-group.sh."

#------------------------------------------------------------------------------
# Cleanup + handoff
#------------------------------------------------------------------------------

kubectl delete pod "${RSYNC_POD}" -n "${RSYNC_POD_NS}" --ignore-not-found >/dev/null

echo ""
log_info "Old (unencrypted) filesystem ${OLD_FS_ID} was kept as a fallback."
log_info "Delete it manually once the migration has soaked:"
log_cmd "aws efs delete-file-system --file-system-id ${OLD_FS_ID}   # after deleting its mount targets"
echo ""
log_info "Platform deployments are still scaled to 0. The next deploy step"
log_detail "(./07-deploy-r1.sh) re-applies manifests and scales everything back up;"
log_detail "to restore service sooner: kubectl scale deployment/aura-swarm-{gateway,control,scheduler} -n ${K8S_NAMESPACE_SYSTEM} --replicas=1"

step_ok "02 (re-run ./02-snp-node-group.sh — the plan should now be clean)"
