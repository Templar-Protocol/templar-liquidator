variable "project_id" {
  description = "GCP project id to deploy the liquidator job into."
  type        = string
}

variable "region" {
  description = "GCP region for every resource this module creates (Cloud Run job, Artifact Registry mirror, Cloud Scheduler job)."
  type        = string
  default     = "us-central1"
}

variable "name" {
  description = "Base name used to derive every resource name (job, service accounts, Artifact Registry repository, Scheduler job). Keep it short: it is a prefix for GCP service-account ids, which are capped at 30 characters."
  type        = string
  default     = "templar-liquidator"

  validation {
    condition     = can(regex("^[a-z]([a-z0-9-]{0,18}[a-z0-9])?$", var.name))
    error_message = "name must be lowercase alphanumeric with hyphens, start with a letter, and be at most 20 characters so derived service-account ids stay under GCP's 30-character limit."
  }
}

variable "image_tag" {
  description = "templar-liquidator release tag to run, e.g. \"v0.2.0\". Matches a tag published to ghcr.io/templar-protocol/templar-liquidator."
  type        = string
}

variable "schedule" {
  description = "Cloud Scheduler cron expression that triggers one liquidation cycle. Must leave enough headroom for task_timeout_seconds — see the README's overlap rule."
  type        = string
  default     = "*/10 * * * *"
}

variable "task_timeout_seconds" {
  description = "Cloud Run Job task timeout in seconds. MUST stay below the schedule interval so two executions never overlap — the job has max_retries = 0 by design, so the next scheduled tick is the retry, not an in-place rerun."
  type        = number
  default     = 480
}

variable "env" {
  description = "Plain (non-secret) environment variables passed to the container, e.g. NEAR_NETWORK, REGISTRY_ACCOUNT_IDS, SIGNER_ACCOUNT_ID, DRY_RUN. See the upstream .env.example for the full list of variables the binary reads."
  type        = map(string)
  default     = {}
}

variable "secret_env" {
  description = "Map of environment variable name to an EXISTING Secret Manager secret id (e.g. { SIGNER_KEY = \"liquidator-signer-key\" }). This module only references secrets and grants the runtime service account access to their `latest` version — it never creates or populates secret values. Create the secrets first (see README)."
  type        = map(string)
  default     = {}
}

variable "cpu" {
  description = "vCPU allocation for the job container, in Cloud Run's resource-limit string form (e.g. \"1\", \"2\")."
  type        = string
  default     = "1"
}

variable "memory" {
  description = "Memory allocation for the job container, in Cloud Run's resource-limit string form (e.g. \"512Mi\", \"1Gi\")."
  type        = string
  default     = "512Mi"
}

variable "enable_alerts" {
  description = "Whether to create the optional Cloud Monitoring alert policies (failed task attempts, and absence of a successful run). Requires notification_channels to actually notify anyone."
  type        = bool
  default     = false
}

variable "notification_channels" {
  description = "Notification channel resource names (e.g. \"projects/<project>/notificationChannels/<id>\") the alert policies should notify. Ignored when enable_alerts is false."
  type        = list(string)
  default     = []
}

variable "alert_absence_hours" {
  description = "Hours of no successful execution before the absence alert policy fires. Ignored when enable_alerts is false."
  type        = number
  default     = 3
}
