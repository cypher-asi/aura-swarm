variable "resource_prefix" {
  description = "Prefix for resource names"
  type        = string
}

variable "repositories" {
  description = "List of repository names to create"
  type        = list(string)
}

variable "image_retention_count" {
  description = "Number of images to retain in each repository"
  type        = number
  default     = 30
}

variable "tags" {
  description = "Common tags for all resources"
  type        = map(string)
  default     = {}
}
