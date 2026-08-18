# templar-liquidator Terraform module

Deploy [templar-liquidator](https://github.com/templar-protocol/templar-liquidator)
on GCP as a scheduled job — no server to babysit, no continuous-loop process
to keep alive. This is a generic module: every deployment-specific value
(project, region, schedule, env vars, secrets, alerting) is a variable. It
has no knowledge of any particular deployment, including ours — third
parties are expected to consume it directly, the same way we do from our
own (private) infrastructure repo.

## What it deploys

- One **Cloud Run job** that runs the bot with `--run-mode once`: one
  registry refresh + one liquidation scan, then exit.
- One **Cloud Scheduler** cron trigger that invokes the job on a schedule
  (default every 10 minutes).
- One **Artifact Registry remote repository** that mirrors `ghcr.io` — Cloud
  Run cannot pull images from GHCR directly, so every pull goes through this
  proxy instead.
- Two **service accounts**: one the job runs as (least privilege: Artifact
  Registry read + access to the specific secrets you reference), one
  Scheduler uses to invoke the job (`roles/run.invoker` only).
- Secret Manager **IAM bindings** for secrets you name in `secret_env` — the
  module reads existing secrets, it never creates or populates one.
- Two **optional** Cloud Monitoring alert policies (`enable_alerts = true`):
  failed task attempts, and no successful execution in N hours.

At a 10-minute cadence this runs roughly 4,300 executions/month at the
default `cpu = "1"` / `memory = "512Mi"`. Cloud Run Jobs billing is
per-vCPU-second and per-GiB-second while a task is actually running, so cost
scales with how long each scan takes — typically a few cents to low dollars
per month, not a fixed monthly charge.

## Prerequisites

- A GCP project with billing enabled.
- These APIs enabled on it:
  - `run.googleapis.com`
  - `cloudscheduler.googleapis.com`
  - `artifactregistry.googleapis.com`
  - `secretmanager.googleapis.com`
  - `monitoring.googleapis.com` (only needed if `enable_alerts = true`)
- A NEAR account funded with the borrow-side assets the bot will liquidate
  with, and its key.

## Create the secrets first

This module references Secret Manager secrets by id — it does not create
them. Create each one before your first `terraform apply`:

```bash
gcloud secrets create liquidator-signer-key --project "$PROJECT_ID"

printf '%s' "$SIGNER_KEY" | gcloud secrets create liquidator-signer-key \
  --project "$PROJECT_ID" --data-file=-
```

The `printf '%s'` form is load-bearing, not stylistic: `echo` appends a
trailing newline, and a trailing newline in a signer key or an RPC API key
breaks header/key auth in ways that are easy to misdiagnose (the request
just fails). Never pipe `echo` into `gcloud secrets create` /
`gcloud secrets versions add` for this bot's secrets.

Do the same for any other secret you plan to reference in `secret_env`
(e.g. an RPC API key, a 1-Click token, a Telegram bot token).

## 15-minute walkthrough

1. Create the secrets you need (above).
2. Copy the example:
   ```bash
   cp -r terraform/examples/basic my-liquidator-deploy
   cd my-liquidator-deploy
   cp terraform.tfvars.example terraform.tfvars
   ```
3. Edit `terraform.tfvars`: set `project_id` and `image_tag` (a released
   tag, e.g. `v0.2.0`). Edit `main.tf`'s `secret_env` map to point at the
   secret ids you actually created, and adjust `env` (network, registry
   account ids, signer account id, strategy knobs) for your deployment.
4. ```bash
   terraform init
   terraform apply
   ```
5. Wait for the next scheduled tick (or trigger one manually —
   `gcloud scheduler jobs run <scheduler_job_name> --location <region>`) and
   check the execution in **Cloud Run → Jobs → your job → Executions →
   Logs**. `env.DRY_RUN` defaults to `"true"` in the example, so this first
   run simulates: it scans and logs what it would liquidate without
   submitting anything on-chain.
6. Once you've reviewed a few simulated runs and trust the configuration,
   flip `DRY_RUN` to `"false"` in `terraform.tfvars`/`main.tf` and
   `terraform apply` again. This is a deliberate, separate step — going
   live should never be a side effect of an unrelated change.

## The overlap rule

`task_timeout_seconds` **must stay below the schedule interval**. The job
is created with `max_retries = 0` on purpose: a failed cycle is not retried
in place, the next scheduled tick is the retry. If the timeout is allowed to
exceed (or even approach) the interval, a slow cycle can still be running
when the next scheduled execution starts, and you get two concurrent
liquidation attempts against the same positions instead of one clean retry
cadence.

Sizing the timeout is a measurement problem, not a guess: it needs to
comfortably exceed roughly
`markets × (scan time per market + 5s inter-market delay)`, plus time for
swap settlement if `COLLATERAL_STRATEGY=swap-to-borrow`, plus slack for a
rate-limited RPC — a single 429 can cost a 60s backoff on its own. Deploy
with the defaults (`schedule = "*/10 * * * *"`, `task_timeout_seconds = 480`),
watch how long your first several real executions actually take in the
Cloud Run Jobs logs, and raise both the timeout and the schedule interval
together if cycles start approaching the limit. A registry that grows (more
markets) or an RPC that gets flakier both push the required timeout up over
time — this isn't a set-once number.

## Safety: simulation is the default

The bot **simulates unless `DRY_RUN` is set to exactly `"false"`**. Any
other value — unset, blank, `"0"`, `"False"`, a quoted variant — is either
still a simulation or a startup failure, deliberately never a silent live
deploy. Going live is always an explicit, reviewable edit to `env.DRY_RUN`
in your Terraform config, never an implicit default.

## Verify the image path

The module derives the mirrored image location from the Artifact Registry
resource's own computed `registry_uri` attribute (not a hand-built string),
then appends the upstream's own path — `templar-protocol/templar-liquidator`
— the same way an Artifact Registry Docker Hub remote repo preserves
`library/postgres`. To confirm this resolves correctly for your project
after `terraform apply`:

```bash
terraform output image
gcloud artifacts docker images list "$(terraform output -raw image | cut -d: -f1)"
```

If Google changes the remote-repository path convention for a custom
upstream in a future provider version, `terraform output image` is the spot
to check first — the `mirrored_image` local in `main.tf` is where you'd
adjust it.

## Variable reference

See `variables.tf` for the full list with descriptions and defaults. The
ones worth calling out:

| Variable | Purpose |
|---|---|
| `env` | Plain env vars, e.g. `NEAR_NETWORK`, `REGISTRY_ACCOUNT_IDS`, `SIGNER_ACCOUNT_ID`, `DRY_RUN`, `MIN_PROFIT_BPS`. See the upstream `.env.example` for the full set the binary reads. |
| `secret_env` | Env var name → existing Secret Manager secret id, e.g. `SIGNER_KEY`, `NEAR_RPC_API_KEY`, `TELEGRAM_BOT_TOKEN`. |
| `enable_alerts` / `notification_channels` / `alert_absence_hours` | Optional Cloud Monitoring alerting on top of the bot's own Telegram notifier. |

## This is a generic module

Nothing in `terraform/` refers to any specific deployment — not ours, not
anyone else's. If you're standing up your own instance, consume this module
directly (`source = "git::https://github.com/templar-protocol/templar-liquidator//terraform?ref=vX.Y.Z"`
or a local path) from your own Terraform, the way `examples/basic` does.
Pin `ref` to a release tag and move it together with `image_tag` — one
version bump upgrades infrastructure and binary atomically, and rollback is
the same edit backwards.
