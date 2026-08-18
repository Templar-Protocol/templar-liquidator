# Complete, fictional example deployment. Copy this directory, fill in
# terraform.tfvars from terraform.tfvars.example, and apply — see the
# module README for the full walkthrough.

terraform {
  required_version = ">= 1.9"
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = ">= 6.0"
    }
  }
}

variable "project_id" {
  description = "GCP project id to deploy this example into."
  type        = string
}

variable "region" {
  description = "GCP region for all resources."
  type        = string
  default     = "us-central1"
}

variable "image_tag" {
  description = "templar-liquidator release tag to run, e.g. \"v0.2.0\"."
  type        = string
}

provider "google" {
  project = var.project_id
  region  = var.region
}

module "liquidator" {
  source = "../.."

  project_id = var.project_id
  region     = var.region
  name       = "example-liquidator"
  image_tag  = var.image_tag

  schedule             = "*/10 * * * *"
  task_timeout_seconds = 480

  env = {
    NEAR_NETWORK         = "mainnet"
    NEAR_RPC_URL         = "https://free.rpc.fastnear.com"
    REGISTRY_ACCOUNT_IDS = "v1.tmplr.near"
    SIGNER_ACCOUNT_ID    = "example-liquidator.near"
    MIN_PROFIT_BPS       = "50"

    # Safety default: the bot simulates unless DRY_RUN is exactly "false".
    # Leave this as "true" for your first several scheduled executions,
    # confirm the scans and (simulated) liquidations look right in Cloud Run
    # Jobs logs, then flip it to "false" as a deliberate, separate change.
    DRY_RUN = "true"
  }

  # Env var name -> id of an EXISTING Secret Manager secret. This module
  # only grants read access to these secrets' `latest` version — create them
  # first (see README "Create the secrets first").
  secret_env = {
    SIGNER_KEY       = "example-liquidator-signer-key"
    NEAR_RPC_API_KEY = "example-liquidator-rpc-api-key"
  }

  enable_alerts         = true
  notification_channels = ["projects/example-project/notificationChannels/1234567890"]
  alert_absence_hours   = 3
}

output "image" {
  value = module.liquidator.image
}

output "job_name" {
  value = module.liquidator.job_name
}
