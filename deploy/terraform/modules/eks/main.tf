#------------------------------------------------------------------------------
# EKS Module - Kubernetes Cluster
#
# Creates EKS cluster with node groups configured for Kata Containers
# as specified in spec 05-scheduler.md.
#------------------------------------------------------------------------------

#------------------------------------------------------------------------------
# EKS Cluster IAM Role
#------------------------------------------------------------------------------

data "aws_iam_policy_document" "eks_assume_role" {
  statement {
    effect = "Allow"
    principals {
      type        = "Service"
      identifiers = ["eks.amazonaws.com"]
    }
    actions = ["sts:AssumeRole"]
  }
}

resource "aws_iam_role" "cluster" {
  name               = "${var.resource_prefix}-eks-cluster-role"
  assume_role_policy = data.aws_iam_policy_document.eks_assume_role.json

  tags = var.tags
}

resource "aws_iam_role_policy_attachment" "cluster_policy" {
  policy_arn = "arn:aws:iam::aws:policy/AmazonEKSClusterPolicy"
  role       = aws_iam_role.cluster.name
}

resource "aws_iam_role_policy_attachment" "cluster_vpc_resource_controller" {
  policy_arn = "arn:aws:iam::aws:policy/AmazonEKSVPCResourceController"
  role       = aws_iam_role.cluster.name
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
  role_arn = aws_iam_role.cluster.arn

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

  depends_on = [
    aws_iam_role_policy_attachment.cluster_policy,
    aws_iam_role_policy_attachment.cluster_vpc_resource_controller,
  ]
}

#------------------------------------------------------------------------------
# OIDC Provider for IRSA (IAM Roles for Service Accounts)
#------------------------------------------------------------------------------

data "tls_certificate" "cluster" {
  url = aws_eks_cluster.main.identity[0].oidc[0].issuer
}

resource "aws_iam_openid_connect_provider" "cluster" {
  client_id_list  = ["sts.amazonaws.com"]
  thumbprint_list = [data.tls_certificate.cluster.certificates[0].sha1_fingerprint]
  url             = aws_eks_cluster.main.identity[0].oidc[0].issuer

  tags = var.tags
}

#------------------------------------------------------------------------------
# Node Group IAM Role
#------------------------------------------------------------------------------

data "aws_iam_policy_document" "node_assume_role" {
  statement {
    effect = "Allow"
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
    actions = ["sts:AssumeRole"]
  }
}

resource "aws_iam_role" "node_group" {
  name               = "${var.resource_prefix}-eks-node-role"
  assume_role_policy = data.aws_iam_policy_document.node_assume_role.json

  tags = var.tags
}

resource "aws_iam_role_policy_attachment" "node_worker_policy" {
  policy_arn = "arn:aws:iam::aws:policy/AmazonEKSWorkerNodePolicy"
  role       = aws_iam_role.node_group.name
}

resource "aws_iam_role_policy_attachment" "node_cni_policy" {
  policy_arn = "arn:aws:iam::aws:policy/AmazonEKS_CNI_Policy"
  role       = aws_iam_role.node_group.name
}

resource "aws_iam_role_policy_attachment" "node_ecr_read" {
  policy_arn = "arn:aws:iam::aws:policy/AmazonEC2ContainerRegistryReadOnly"
  role       = aws_iam_role.node_group.name
}

# SSM for node management
resource "aws_iam_role_policy_attachment" "node_ssm" {
  policy_arn = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
  role       = aws_iam_role.node_group.name
}

#------------------------------------------------------------------------------
# Peer Pods / Cloud API Adaptor (CAA) locals
#------------------------------------------------------------------------------

locals {
  # OIDC issuer host (no scheme) for IRSA trust policies.
  oidc_provider_url = replace(aws_eks_cluster.main.identity[0].oidc[0].issuer, "https://", "")

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
  node_role_arn   = aws_iam_role.node_group.arn
  subnet_ids      = var.agent_subnet_ids

  instance_types = [var.node_instance_type]
  capacity_type  = "ON_DEMAND"

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

  depends_on = [
    aws_iam_role_policy_attachment.node_worker_policy,
    aws_iam_role_policy_attachment.node_cni_policy,
    aws_iam_role_policy_attachment.node_ecr_read,
  ]
}

#------------------------------------------------------------------------------
# Peer Pods / Cloud API Adaptor (CAA) IAM (IRSA)
#
# Dedicated IRSA role bound to the CAA service account
# (cloud-api-adaptor / confidential-containers-system) via the cluster OIDC
# provider. Grants CAA permission to create/terminate the per-agent AWS-managed
# SEV-SNP "pod VM" EC2 instances. Peer Pods is the only confidential runtime,
# so this is always created.
#------------------------------------------------------------------------------

data "aws_iam_policy_document" "caa_assume_role" {
  statement {
    effect  = "Allow"
    actions = ["sts:AssumeRoleWithWebIdentity"]

    principals {
      type        = "Federated"
      identifiers = [aws_iam_openid_connect_provider.cluster.arn]
    }

    condition {
      test     = "StringEquals"
      variable = "${local.oidc_provider_url}:sub"
      values   = ["system:serviceaccount:confidential-containers-system:cloud-api-adaptor"]
    }

    condition {
      test     = "StringEquals"
      variable = "${local.oidc_provider_url}:aud"
      values   = ["sts.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "caa" {
  name               = "${var.resource_prefix}-caa-role"
  assume_role_policy = data.aws_iam_policy_document.caa_assume_role.json

  tags = var.tags
}

data "aws_iam_policy_document" "caa" {
  # Per-pod VM lifecycle + discovery. RunInstances/TerminateInstances act on
  # ephemeral pod VMs whose ids are not known ahead of time, so resource is "*"
  # (CAA tags every pod VM; the smoke test asserts create + terminate).
  #
  # The write actions are explicit. For reads, CAA calls a range of ec2:Describe*
  # operations (DescribeInstances, DescribeImages, DescribeInstanceTypes,
  # DescribeSubnets, DescribeSecurityGroups, DescribeVpcs, DescribeKeyPairs,
  # DescribeLaunchTemplates, ...) — granting ec2:Describe* avoids per-call
  # whack-a-mole. All ec2:Describe* are read-only and cannot be resource-scoped
  # (they require "*"), so enumerating them individually buys no extra safety.
  statement {
    sid    = "PodVMLifecycle"
    effect = "Allow"
    actions = [
      "ec2:RunInstances",
      "ec2:TerminateInstances",
      "ec2:CreateTags",
      "ec2:Describe*",
    ]
    resources = ["*"]
  }

  # iam:PassRole is only needed if pod VMs are launched with an instance profile.
  # Scoped as tightly as practical to the documented pod-VM instance-role name
  # pattern (account wildcard since the account id is not available in-module).
  # Tighten to the exact role ARN once a pod-VM instance role exists.
  statement {
    sid       = "PassPodVMInstanceRole"
    effect    = "Allow"
    actions   = ["iam:PassRole"]
    resources = ["arn:aws:iam::*:role/${var.resource_prefix}-podvm-*"]
  }
}

resource "aws_iam_role_policy" "caa" {
  name   = "${var.resource_prefix}-caa-policy"
  role   = aws_iam_role.caa.id
  policy = data.aws_iam_policy_document.caa.json
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
