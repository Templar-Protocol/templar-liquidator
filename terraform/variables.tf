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
  description = "templar-liquidator image tag to run, e.g. \"0.2.0\" (no leading \"v\" — docker/metadata-action strips it from the git tag when publishing). Matches a tag published to ghcr.io/templar-protocol/templar-liquidator. NOTE: that package is currently private and the mirror proxies it anonymously, so a released tag fails at image pull on the first execution — see the module README, step 3."
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

  validation {
    # Enforces the no-overlap invariant above for the common "every N
    # minutes" cron form (`*/N * * * *`) that this module's own default
    # schedule uses. A full cron parser is out of scope for a single
    # validation expression, so schedule shapes this regex doesn't
    # recognize (specific hours/days, comma/range lists, step values on
    # other fields, etc.) are NOT checked here — size task_timeout_seconds
    # against the actual interval by hand for those, using the same
    # "timeout < interval" rule.
    condition = (
      can(regex("^\\*/([0-9]+) \\* \\* \\* \\*$", var.schedule))
      ? var.task_timeout_seconds < tonumber(regex("^\\*/([0-9]+) \\* \\* \\* \\*$", var.schedule)[0]) * 60
      : true
    )
    error_message = "task_timeout_seconds must be less than the schedule interval in seconds, so a slow run can never still be executing when the next scheduled tick fires (max_retries = 0 makes that tick the retry, not a rerun). This check only recognizes the \"*/N * * * *\" (every N minutes) cron form — for any other schedule shape, verify the invariant by hand and treat this validation passing as inconclusive, not as proof."
  }
}

variable "env" {
  description = "Plain (non-secret) environment variables passed to the container, e.g. NEAR_NETWORK, REGISTRY_ACCOUNT_IDS, SIGNER_ACCOUNT_ID, DRY_RUN. See the upstream .env.example for the full list of variables the binary reads. Secret-shaped values (signer keys, RPC/swap/notification tokens) belong in secret_env instead — see its validation below."
  type        = map(string)
  default     = {}

  validation {
    # Fails closed on the documented secret-shaped variable names so an
    # operator can't accidentally pass a real secret here: env values
    # become plaintext container env vars in the Cloud Run Job's config
    # (readable to anyone with read access to the job resource), instead
    # of a Secret Manager reference resolved at container start. Extend
    # this list if the binary starts reading another credential-shaped env
    # var upstream.
    condition = length([
      for k in keys(var.env) : k
      if contains(["SIGNER_KEY", "NEAR_RPC_API_KEY", "ONECLICK_API_TOKEN", "TELEGRAM_BOT_TOKEN"], k)
    ]) == 0
    error_message = "env must not contain secret-shaped keys (SIGNER_KEY, NEAR_RPC_API_KEY, ONECLICK_API_TOKEN, TELEGRAM_BOT_TOKEN). Pass these through secret_env instead, which mounts them from an existing Secret Manager secret rather than recording plaintext in the container's env config."
  }

  validation {
    # A key name check is not enough for NEAR_RPC_URL: the documented way to
    # authenticate an RPC endpoint is an `apiKey` query parameter on the URL
    # itself, so a credential can ride into plaintext env config inside a
    # variable whose name looks harmless. Pass the whole URL through
    # secret_env when it carries a credential, or supply the key separately
    # as the NEAR_RPC_API_KEY secret (the binary sends it as a header).
    condition = length([
      for k, v in var.env : k
      if length(regexall("(?i)(apikey|api_key|token|password|secret)=", v)) > 0
    ]) == 0
    error_message = "env values must not embed credentials as query parameters (e.g. NEAR_RPC_URL=https://rpc...?apiKey=...). Pass the URL through secret_env, or send the key as the NEAR_RPC_API_KEY secret instead."
  }

  validation {
    # A key present in both maps is ambiguous (which one wins depends on
    # provider/API ordering, not anything this module controls) and, if
    # env's copy is a real secret, defeats the check above by duplicating
    # the value in plaintext anyway.
    condition     = length(setintersection(keys(var.env), keys(var.secret_env))) == 0
    error_message = "env and secret_env must not share any keys. Keep each variable name in exactly one of the two maps."
  }
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
