#!/bin/bash
# _helpers.sh - Shared helper functions for deploy scripts
# Source this from any deploy script after sourcing config.env and setting colors.

#------------------------------------------------------------------------------
# Read a feature flag value from the existing terraform.tfvars
#
# Usage: value=$(read_tfvar_flag "enable_eks" "false")
#------------------------------------------------------------------------------
read_tfvar_flag() {
    local flag_name="$1"
    local default_value="${2:-false}"
    local tfvars_file="${SCRIPT_DIR}/terraform/terraform.tfvars"

    if [[ -f "$tfvars_file" ]]; then
        local line
        line=$(grep "^${flag_name}[[:space:]]*=" "$tfvars_file" 2>/dev/null | head -1)
        if [[ -n "$line" ]]; then
            local value
            value=$(echo "$line" | awk -F= '{print $2}' | tr -d ' ')
            if [[ "$value" == "true" || "$value" == "false" ]]; then
                echo "$value"
                return
            fi
        fi
    fi
    echo "$default_value"
}

#------------------------------------------------------------------------------
# Analyze a terraform plan file for destructive changes and display a
# prominent warning listing exactly what will be destroyed and what data
# will be lost. Requires the plan file to already exist on disk.
#
# Usage: warn_about_destroys "plan_file.tfplan"
# Sets:  PLAN_HAS_DESTROYS=true  when the plan includes destroy actions
#        PLAN_HAS_DESTROYS=false  when the plan is safe
#------------------------------------------------------------------------------
warn_about_destroys() {
    local plan_file="$1"
    PLAN_HAS_DESTROYS=false

    local show_output
    show_output=$(terraform show -no-color "$plan_file" 2>/dev/null) || return 0

    local destroyed_lines
    destroyed_lines=$(echo "$show_output" | grep "will be destroyed" || true)

    if [[ -z "$destroyed_lines" ]]; then
        return 0
    fi

    PLAN_HAS_DESTROYS=true
    local destroy_count
    destroy_count=$(echo "$destroyed_lines" | wc -l | tr -d ' ')

    echo ""
    echo -e "${RED}================================================================${NC}"
    echo -e "${RED}  WARNING: This plan will DESTROY ${destroy_count} resource(s)${NC}"
    echo -e "${RED}================================================================${NC}"
    echo ""

    echo "$destroyed_lines" | while IFS= read -r line; do
        local resource
        resource=$(echo "$line" | sed 's/.*# //' | sed 's/ will be destroyed.*//')

        case "$resource" in
            *eks_node_group*)
                echo -e "  ${RED}X EKS Node Group${NC}  ${resource}"
                echo -e "    ${RED}-> All running workloads on these nodes will be terminated${NC}"
                ;;
            *eks_cluster*)
                echo -e "  ${RED}X EKS Cluster${NC}     ${resource}"
                echo -e "    ${RED}-> The entire Kubernetes cluster will be deleted${NC}"
                ;;
            *efs_file_system*)
                echo -e "  ${RED}X EFS Filesystem${NC}  ${resource}"
                echo -e "    ${RED}-> ALL STORED DATA WILL BE PERMANENTLY LOST${NC}"
                ;;
            *efs_mount_target*)
                echo -e "  ${RED}X EFS Mount Target${NC}  ${resource}"
                ;;
            *ecr_repository*)
                echo -e "  ${RED}X ECR Repository${NC}  ${resource}"
                echo -e "    ${RED}-> All container images in this repository will be deleted${NC}"
                ;;
            *security_group*)
                echo -e "  ${YELLOW}- Security Group${NC}       ${resource}"
                ;;
            *iam_role_policy*)
                echo -e "  ${YELLOW}- IAM Policy Attachment${NC} ${resource}"
                ;;
            *iam_role*)
                echo -e "  ${YELLOW}- IAM Role${NC}             ${resource}"
                ;;
            *)
                echo -e "  ${YELLOW}- ${resource}${NC}"
                ;;
        esac
    done

    echo ""
    echo -e "${YELLOW}If this is unexpected, answer 'no' below and verify that${NC}"
    echo -e "${YELLOW}terraform.tfvars has the correct feature flags.${NC}"
    echo ""
}

#------------------------------------------------------------------------------
# Prompt the user with a plan review, including destroy warnings when present.
# Exits the script if the user declines.
#
# Usage: confirm_plan "plan_file.tfplan"
#------------------------------------------------------------------------------
confirm_plan() {
    local plan_file="$1"

    warn_about_destroys "$plan_file"

    echo ""
    echo "=============================================="
    echo -e "${YELLOW}Review the plan above before proceeding${NC}"
    echo "=============================================="
    echo ""

    if [[ "${PLAN_HAS_DESTROYS}" == "true" ]]; then
        echo -e "${RED}Type 'destroy' to confirm destructive changes, or 'no' to abort.${NC}"
        read -p "Confirm: " confirm
        if [[ "$confirm" != "destroy" ]]; then
            echo "Aborted. No changes applied."
            exit 0
        fi

        # Re-plan with force_delete so ECR repos with images can actually be removed
        echo ""
        echo "Re-planning with force_delete enabled for clean teardown..."
        terraform plan -out="$plan_file" -var="ecr_force_delete=true"
    else
        read -p "Apply this plan? (yes/no) [no]: " confirm
        confirm=${confirm:-no}
        if [[ "$confirm" != "yes" ]]; then
            echo "Aborted. Run this script again when ready."
            exit 0
        fi
    fi
}
