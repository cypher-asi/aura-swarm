#------------------------------------------------------------------------------
# EKS Module - Kubernetes Cluster
#
# Creates EKS cluster with node groups configured for Kata Containers
# as specified in spec 05-scheduler.md.
#------------------------------------------------------------------------------

#------------------------------------------------------------------------------
# EKS Cluster IAM Role (created out-of-band by org-admin's ./01-iam.sh)
#
# All IAM (role/policy creation + grants) is consolidated under the org-admin
# step for separation of duties; ops-admin's terraform only READS the role
# (iam:GetRole) and PASSES it (scoped iam:PassRole) when creating the cluster.
#------------------------------------------------------------------------------

data "aws_iam_role" "cluster" {
  name = "${var.resource_prefix}-eks-cluster-role"
}

#------------------------------------------------------------------------------
# EKS Cluster Security Group
#------------------------------------------------------------------------------

resource "aws_security_group" "cluster" {
  name        = "${var.resource_prefix}-eks-cluster-sg"
  description = "Security group for EKS cluster"
  vpc_id      = var.vpc_id

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-eks-cluster-sg"
  })
}

#------------------------------------------------------------------------------
# EKS Cluster
#------------------------------------------------------------------------------

resource "aws_eks_cluster" "main" {
  name     = "${var.resource_prefix}-cluster"
  version  = var.eks_version
  role_arn = data.aws_iam_role.cluster.arn

  vpc_config {
    subnet_ids              = concat(var.private_subnet_ids, var.agent_subnet_ids)
    security_group_ids      = [aws_security_group.cluster.id]
    endpoint_private_access = true
    endpoint_public_access  = true
  }

  enabled_cluster_log_types = [
    "api",
    "audit",
    "authenticator",
    "controllerManager",
    "scheduler"
  ]

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-cluster"
  })
}

#------------------------------------------------------------------------------
# Node Group IAM Role (created out-of-band by org-admin's ./01-iam.sh)
#
# As with the cluster role, ops-admin's terraform only READS this role and
# PASSES it when creating the managed node group. The OIDC provider and the
# Peer Pods / CAA IRSA role also live in ./01-iam.sh (the CAA trust policy
# references the cluster OIDC issuer, which only exists post-cluster, so it is
# provisioned on a cluster-aware re-run of step 01 after this module applies).
#------------------------------------------------------------------------------

data "aws_iam_role" "node_group" {
  name = "${var.resource_prefix}-eks-node-role"
}

#------------------------------------------------------------------------------
# Peer Pods / Cloud API Adaptor (CAA) locals
#------------------------------------------------------------------------------

locals {
  # Worker <-> pod-VM ingress required by CAA: agent-protocol-forwarder (15150)
  # and vxlan (9000 tcp+udp). Ports are fixed by the CAA wire protocol and match
  # CAA_FORWARDER_PORT / CAA_VXLAN_PORT in config.env (consumed by the
  # 03-coco-operator CAA Helm install). Peer Pods is the only confidential
  # runtime, so these are always created.
  caa_ingress_rules = [
    {
      description = "Peer Pods agent-protocol-forwarder (worker to/from pod VM)"
      from_port   = 15150
      to_port     = 15150
      protocol    = "tcp"
    },
    {
      description = "Peer Pods vxlan tcp (worker to/from pod VM)"
      from_port   = 9000
      to_port     = 9000
      protocol    = "tcp"
    },
    {
      description = "Peer Pods vxlan udp (worker to/from pod VM)"
      from_port   = 9000
      to_port     = 9000
      protocol    = "udp"
    },
  ]
}

#------------------------------------------------------------------------------
# Node Security Group
#------------------------------------------------------------------------------

resource "aws_security_group" "node" {
  name        = "${var.resource_prefix}-eks-node-sg"
  description = "Security group for EKS worker nodes"
  vpc_id      = var.vpc_id

  # Allow nodes to communicate with each other
  ingress {
    description = "Node to node communication"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    self        = true
  }

  # Allow pods to communicate with the cluster API
  ingress {
    description     = "Cluster API to nodes"
    from_port       = 443
    to_port         = 443
    protocol        = "tcp"
    security_groups = [aws_security_group.cluster.id]
  }

  # Allow kubelet and other services
  ingress {
    description     = "Cluster API to kubelet"
    from_port       = 10250
    to_port         = 10250
    protocol        = "tcp"
    security_groups = [aws_security_group.cluster.id]
  }

  # Peer Pods / CAA: worker <-> pod-VM traffic (agent-protocol-forwarder + vxlan),
  # scoped to the VPC CIDR (pod VMs launch into the agent subnets).
  dynamic "ingress" {
    for_each = local.caa_ingress_rules
    content {
      description = ingress.value.description
      from_port   = ingress.value.from_port
      to_port     = ingress.value.to_port
      protocol    = ingress.value.protocol
      cidr_blocks = [var.vpc_cidr]
    }
  }

  egress {
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = merge(var.tags, {
    Name                                                   = "${var.resource_prefix}-eks-node-sg"
    "kubernetes.io/cluster/${var.resource_prefix}-cluster" = "owned"
  })
}

# Allow cluster to communicate with nodes
resource "aws_security_group_rule" "cluster_to_nodes" {
  type                     = "ingress"
  from_port                = 443
  to_port                  = 443
  protocol                 = "tcp"
  security_group_id        = aws_security_group.cluster.id
  source_security_group_id = aws_security_group.node.id
  description              = "Allow nodes to communicate with cluster"
}

#------------------------------------------------------------------------------
# EKS Managed Node Group - System
#
# Hosts the platform system components (gateway, scheduler, CoreDNS, EFS
# CSI, CoCo operator controller, Trustee): it is the only untainted pool.
#
# Hosts the platform system components AND the agent pods. Agents run as Peer
# Pods (kata-remote): the kata shim + agent-protocol-forwarder run here on the
# ordinary worker while the per-agent SEV-SNP workload runs off-cluster in an
# AWS-managed pod VM. There is no on-node confidential pool. Size this pool for
# system components plus per-agent shim overhead.
#------------------------------------------------------------------------------

resource "aws_eks_node_group" "main" {
  cluster_name    = aws_eks_cluster.main.name
  node_group_name = "${var.resource_prefix}-node-group"
  node_role_arn   = data.aws_iam_role.node_group.arn
  subnet_ids      = var.agent_subnet_ids

  instance_types = [var.node_instance_type]
  capacity_type  = "ON_DEMAND"

  # Pin the AL2023 AMI to a containerd-1.7.x release so kata-remote guest image
  # pull works (containerd 2.x breaks it — see var.node_ami_release_version).
  # null => let EKS pick the latest AMI for ami_type/eks_version.
  release_version = var.node_ami_release_version != "" ? var.node_ami_release_version : null

  disk_size = var.node_disk_size

  scaling_config {
    desired_size = var.node_desired_count
    min_size     = var.node_min_count
    max_size     = var.node_max_count
  }

  update_config {
    max_unavailable = 1
  }

  labels = {
    role = "system"
  }

  tags = merge(var.tags, {
    Name = "${var.resource_prefix}-node-group"
  })
}

#------------------------------------------------------------------------------
# EKS Addons
#------------------------------------------------------------------------------

resource "aws_eks_addon" "vpc_cni" {
  cluster_name = aws_eks_cluster.main.name
  addon_name   = "vpc-cni"

  resolve_conflicts_on_create = "OVERWRITE"
  resolve_conflicts_on_update = "PRESERVE"
}

resource "aws_eks_addon" "coredns" {
  cluster_name = aws_eks_cluster.main.name
  addon_name   = "coredns"

  resolve_conflicts_on_create = "OVERWRITE"
  resolve_conflicts_on_update = "PRESERVE"

  depends_on = [aws_eks_node_group.main]
}

resource "aws_eks_addon" "kube_proxy" {
  cluster_name = aws_eks_cluster.main.name
  addon_name   = "kube-proxy"

  resolve_conflicts_on_create = "OVERWRITE"
  resolve_conflicts_on_update = "PRESERVE"
}
