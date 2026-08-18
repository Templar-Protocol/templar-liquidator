output "job_name" {
  description = "Name of the deployed Cloud Run job."
  value       = google_cloud_run_v2_job.liquidator.name
}

output "runtime_service_account" {
  description = "Email of the service account the Cloud Run job executes as."
  value       = google_service_account.job_runtime.email
}

output "image" {
  description = "Full mirrored image path the job pulls (Artifact Registry, not ghcr.io directly)."
  value       = local.mirrored_image
}

output "scheduler_job_name" {
  description = "Name of the Cloud Scheduler job that triggers each run."
  value       = google_cloud_scheduler_job.liquidator.name
}
