#!/bin/bash
# enable-gateway-persistence.sh - Enable persistent RocksDB storage for gateway
#
# This script:
# 1. Creates a PVC for gateway data using existing EFS
# 2. Updates the gateway deployment to use the PVC instead of emptyDir
# 3. Restarts the gateway to pick up the change
#
# After running this, gateway data will survive pod restarts.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${SCRIPT_DIR}/config.env"

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

echo "=============================================="
echo "  Aura Swarm - Enable Gateway Persistence"
echo "=============================================="
echo ""

#------------------------------------------------------------------------------
# Create PVC for gateway data
#------------------------------------------------------------------------------

echo "Creating PersistentVolumeClaim for gateway..."

kubectl apply -f - <<EOF
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: aura-swarm-gateway-data
  namespace: ${K8S_NAMESPACE_SYSTEM}
spec:
  accessModes:
    - ReadWriteMany
  storageClassName: efs-sc
  resources:
    requests:
      storage: 10Gi
EOF

echo -e "${GREEN}✓${NC} PVC created"
echo ""

#------------------------------------------------------------------------------
# Patch gateway deployment to use PVC
#------------------------------------------------------------------------------

echo "Patching gateway deployment to use persistent storage..."

# Patch the volumes section to use PVC instead of emptyDir
kubectl patch deployment aura-swarm-gateway -n "${K8S_NAMESPACE_SYSTEM}" --type='json' -p='[
  {
    "op": "replace",
    "path": "/spec/template/spec/volumes/0",
    "value": {
      "name": "data",
      "persistentVolumeClaim": {
        "claimName": "aura-swarm-gateway-data"
      }
    }
  }
]'

echo -e "${GREEN}✓${NC} Deployment patched"
echo ""

#------------------------------------------------------------------------------
# Wait for rollout
#------------------------------------------------------------------------------

echo "Waiting for gateway to restart with persistent storage..."
kubectl rollout status deployment/aura-swarm-gateway -n "${K8S_NAMESPACE_SYSTEM}" --timeout=120s

echo ""

#------------------------------------------------------------------------------
# Verify
#------------------------------------------------------------------------------

echo "Verifying PVC is bound..."
PVC_STATUS=$(kubectl get pvc aura-swarm-gateway-data -n "${K8S_NAMESPACE_SYSTEM}" -o jsonpath='{.status.phase}')

if [[ "$PVC_STATUS" == "Bound" ]]; then
    echo -e "${GREEN}✓${NC} PVC is bound"
else
    echo -e "${YELLOW}⚠${NC} PVC status: ${PVC_STATUS}"
fi

echo ""
echo "Checking gateway pod volume mounts..."
kubectl get pod -n "${K8S_NAMESPACE_SYSTEM}" -l app=aura-swarm-gateway -o jsonpath='{.items[0].spec.volumes[0]}' | jq .

echo ""
echo -e "${GREEN}=============================================="
echo "  Gateway persistence enabled!"
echo "==============================================${NC}"
echo ""
echo "Agent data will now survive pod restarts."
echo ""
echo -e "${YELLOW}NOTE:${NC} This patch is temporary. To make it permanent,"
echo "update deploy/k8s/05-gateway.yaml to use the PVC."
