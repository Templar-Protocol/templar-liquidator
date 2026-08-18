locals {
  # Cloud Run cannot pull images directly from ghcr.io — it only pulls from
  # Artifact Registry (or GCR). So this module proxies the upstream GHCR
  # image through an Artifact Registry REMOTE_REPOSITORY, and the job runs
  # the mirrored copy instead of the ghcr.io reference.
  #
  # `registry_uri` below is a computed attribute the google provider derives
  # for us (confirmed against the provider schema: it resolves to
  # "<region>-docker.pkg.dev/<project>/<repository_id>"), so it always
  # matches whatever host/path convention that provider version uses instead
  # of us hand-guessing it.
  #
  # The path segment appended after it — "templar-protocol/templar-liquidator"
  # — mirrors how Artifact Registry remote repositories preserve a custom
  # upstream's own path (the same way a Docker Hub remote repo preserves
  # "library/postgres"): pulling the mirror at
  # "<registry_uri>/templar-protocol/templar-liquidator:<tag>" proxies
  # "ghcr.io/templar-protocol/templar-liquidator:<tag>" unchanged. Confirm
  # this on your first deploy — see the README's "Verify the image path"
  # section for the exact command.
  mirrored_image = "${google_artifact_registry_repository.ghcr_mirror.registry_uri}/templar-protocol/templar-liquidator:${var.image_tag}"
}

# Artifact Registry remote repository proxying ghcr.io. Cloud Run cannot pull
# images from ghcr.io directly, so every pull for the job goes through this
# mirror instead of the upstream registry.
resource "google_artifact_registry_repository" "ghcr_mirror" {
  project       = var.project_id
  location      = var.region
  repository_id = "${var.name}-ghcr"
  format        = "DOCKER"
  mode          = "REMOTE_REPOSITORY"
  description   = "Remote mirror of ghcr.io, proxied because Cloud Run cannot pull GHCR images directly."

  remote_repository_config {
    description = "Proxies https://ghcr.io"
    docker_repository {
      custom_repository {
        uri = "https://ghcr.io"
      }
    }
  }
}

# Identity the Cloud Run job executes as. Least-privilege: only Artifact
# Registry read access (below) and access to the specific secrets named in
# var.secret_env.
resource "google_service_account" "job_runtime" {
  project      = var.project_id
  account_id   = "${var.name}-job"
  display_name = "Runtime identity for the ${var.name} Cloud Run job"
}

# Identity Cloud Scheduler uses to invoke the job. Kept separate from the
# job's own runtime identity so the invoker only ever holds run.invoker, not
# any of the job's own data-plane permissions.
resource "google_service_account" "scheduler_invoker" {
  project      = var.project_id
  account_id   = "${var.name}-invoker"
  display_name = "Cloud Scheduler invoker for the ${var.name} Cloud Run job"
}

# Grants the runtime service account read access to each referenced secret's
# `latest` version. This module never creates secret values — secrets are
# expected to already exist (see README's "Create the secrets first" step).
resource "google_secret_manager_secret_iam_member" "job_secret_access" {
  for_each = var.secret_env

  project   = var.project_id
  secret_id = each.value
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.job_runtime.email}"
}

# Lets the runtime service account pull the mirrored image from Artifact
# Registry.
resource "google_artifact_registry_repository_iam_member" "job_reader" {
  project    = var.project_id
  location   = var.region
  repository = google_artifact_registry_repository.ghcr_mirror.repository_id
  role       = "roles/artifactregistry.reader"
  member     = "serviceAccount:${google_service_account.job_runtime.email}"
}

# The scheduled unit of work: one registry-refresh + one liquidation cycle,
# then exit. `--run-mode once` is what makes this a bounded task rather than
# the bot's default continuous loop.
resource "google_cloud_run_v2_job" "liquidator" {
  project  = var.project_id
  name     = var.name
  location = var.region

  template {
    template {
      service_account = google_service_account.job_runtime.email
      timeout         = "${var.task_timeout_seconds}s"

      # The next scheduled tick is the retry: a failed cycle (including a
      # registry that yields zero markets — the bot treats that as failure
      # too) should surface promptly rather than be masked by an in-place
      # Cloud Run retry racing the following scheduled execution.
      max_retries = 0

      containers {
        image = local.mirrored_image
        args  = ["--run-mode", "once"]

        resources {
          limits = {
            cpu    = var.cpu
            memory = var.memory
          }
        }

        dynamic "env" {
          for_each = var.env
          content {
            name  = env.key
            value = env.value
          }
        }

        dynamic "env" {
          for_each = var.secret_env
          content {
            name = env.key
            value_source {
              secret_key_ref {
                secret  = env.value
                version = "latest"
              }
            }
          }
        }
      }
    }
  }
}

# Lets the invoker service account start executions of the job. Nobody else
# needs run.invoker on it.
resource "google_cloud_run_v2_job_iam_member" "scheduler_invoker" {
  project  = var.project_id
  location = var.region
  name     = google_cloud_run_v2_job.liquidator.name
  role     = "roles/run.invoker"
  member   = "serviceAccount:${google_service_account.scheduler_invoker.email}"
}

# Fires the job on var.schedule by POSTing to the Cloud Run Jobs "run" API,
# authenticated as the invoker service account.
#
# Before setting env.DRY_RUN = "false" (going live), manually trigger this
# job once and confirm in Cloud Run -> Jobs -> Executions -> Logs that:
#   1. exactly one execution was created (`gcloud scheduler jobs run
#      "$(terraform output -raw scheduler_job_name)" --location <region>`
#      followed by `gcloud run jobs executions list --job <job-name>`),
#   2. it pulled local.mirrored_image (not a stale or unmirrored tag), and
#   3. every secret_env reference resolved (a bad Secret Manager id fails
#      the execution at container start, not at `terraform apply`).
# See the README's "15-minute walkthrough" step 6.
resource "google_cloud_scheduler_job" "liquidator" {
  project  = var.project_id
  region   = var.region
  name     = "${var.name}-trigger"
  schedule = var.schedule

  http_target {
    http_method = "POST"
    uri         = "https://${var.region}-run.googleapis.com/apis/run.googleapis.com/v1/namespaces/${var.project_id}/jobs/${google_cloud_run_v2_job.liquidator.name}:run"

    oauth_token {
      service_account_email = google_service_account.scheduler_invoker.email
    }
  }
}

# Optional: fires when any task attempt in a job execution fails.
resource "google_monitoring_alert_policy" "job_failed" {
  count = var.enable_alerts ? 1 : 0

  project      = var.project_id
  display_name = "${var.name}: job execution failed"
  combiner     = "OR"

  notification_channels = var.notification_channels

  conditions {
    display_name = "Failed task attempts > 0"

    condition_threshold {
      filter          = "resource.type = \"cloud_run_job\" AND resource.labels.job_name = \"${google_cloud_run_v2_job.liquidator.name}\" AND metric.type = \"run.googleapis.com/job/completed_task_attempt_count\" AND metric.labels.result = \"failed\""
      comparison      = "COMPARISON_GT"
      threshold_value = 0
      duration        = "0s"

      aggregations {
        alignment_period   = "60s"
        per_series_aligner = "ALIGN_COUNT"
      }
    }
  }

  alert_strategy {
    auto_close = "86400s"
  }
}

# Optional: fires when no execution has succeeded in alert_absence_hours —
# the silent-stall class of failure (e.g. Scheduler itself paused, or every
# execution has been quietly failing to even report a completed attempt).
resource "google_monitoring_alert_policy" "job_absent" {
  count = var.enable_alerts ? 1 : 0

  project      = var.project_id
  display_name = "${var.name}: no successful execution in ${var.alert_absence_hours}h"
  combiner     = "OR"

  notification_channels = var.notification_channels

  conditions {
    display_name = "Absence of successful task attempts"

    condition_absent {
      filter   = "resource.type = \"cloud_run_job\" AND resource.labels.job_name = \"${google_cloud_run_v2_job.liquidator.name}\" AND metric.type = \"run.googleapis.com/job/completed_task_attempt_count\" AND metric.labels.result = \"succeeded\""
      duration = "${var.alert_absence_hours * 3600}s"

      aggregations {
        alignment_period   = "3600s"
        per_series_aligner = "ALIGN_COUNT"
      }
    }
  }

  alert_strategy {
    auto_close = "86400s"
  }
}
