# Deploying on GCP

The bot ships a generic Terraform module ([`terraform/`](../terraform/README.md)) that runs it as a scheduled **Cloud Run Job** — `--run-mode once`, triggered by Cloud Scheduler on a cron interval. No server to keep alive, no continuous-loop process to babysit. This page is a narrative walkthrough of getting from nothing to a running deployment; [`terraform/README.md`](../terraform/README.md) is the authoritative reference for every variable, output, and design decision — read it first if anything here seems to conflict, that file wins.

## 1. Project setup

You need a GCP project with billing enabled and these APIs turned on:

```bash
gcloud services enable \
  run.googleapis.com \
  cloudscheduler.googleapis.com \
  artifactregistry.googleapis.com \
  secretmanager.googleapis.com \
  monitoring.googleapis.com \
  --project "$PROJECT_ID"
```

(`monitoring.googleapis.com` is only needed if you plan to turn on the module's optional alert policies — `enable_alerts`, `notification_channels`, `alert_absence_hours`; see [`terraform/README.md`](../terraform/README.md#variable-reference) / [`terraform/variables.tf`](../terraform/variables.tf).)

You'll also need a NEAR account, funded with the borrow-side assets of whatever markets you intend to serve, and that account's signer key. This is the same account/key you'd use for any other deployment style — nothing about it is GCP-specific.

## 2. Create secrets first

The Terraform module **reads** Secret Manager secrets by id; it never creates or populates one. Create every secret you'll reference before your first `terraform apply`:

```bash
gcloud secrets create liquidator-signer-key --project "$PROJECT_ID"
printf '%s' "$SIGNER_KEY" | gcloud secrets versions add liquidator-signer-key \
  --project "$PROJECT_ID" --data-file=-
```

Repeat for anything else you plan to pass through `secret_env` — typically an RPC API key (`NEAR_RPC_API_KEY`) and, if you want alerts beyond the bot's own Telegram notifier, nothing extra (those go through `notification_channels`, not a secret).

**Use `printf '%s'`, never `echo`.** `echo` appends a trailing newline, and a trailing newline in a signer key or an RPC API key breaks header/key auth in ways that are painful to debug — the request just fails, with no indication the newline is the cause.

## 3. Configure and apply

Copy the fictional-but-complete example and fill in your own values:

```bash
cp -r terraform/examples/basic my-liquidator-deploy
cd my-liquidator-deploy
cp terraform.tfvars.example terraform.tfvars
```

Edit `terraform.tfvars` (`project_id`, `region`, `image_tag` — pin to a released image tag like `0.2.0`, not `latest`; note this drops the `v` that the git release tag carries, see [`terraform/README.md`](../terraform/README.md#this-is-a-generic-module)), and edit `main.tf`'s `env` map (network, registry account ids, signer account id, strategy knobs — `MIN_PROFIT_BPS` etc.) and `secret_env` map (point at the secret ids you actually created in step 2) for your deployment. Then:

```bash
terraform init
terraform apply
```

This provisions: the Cloud Run Job itself, a Cloud Scheduler cron trigger, an Artifact Registry remote repository mirroring `ghcr.io` (Cloud Run can't pull from GHCR directly), two least-privilege service accounts (job runtime, scheduler invoker), and Secret Manager IAM bindings scoped to the secrets you named. See [`terraform/README.md`](../terraform/README.md#what-it-deploys) for the full list.

> **This path currently does not work against the upstream image.** The
> Artifact Registry mirror proxies `https://ghcr.io` anonymously — the module
> configures no `upstream_credentials` — and the published
> `ghcr.io/templar-protocol/templar-liquidator` package is private, because
> GHCR packages default to private and the organization restricts making them
> public. `terraform apply` succeeds; the first execution then fails to pull.
>
> Making this work today needs two module edits, not just a variable — worth
> knowing before you start:
>
> 1. `ghcr_mirror` is a `REMOTE_REPOSITORY` (`terraform/main.tf`), a
>    read-through proxy. You cannot push into it. Hosting your own image means
>    a separate `STANDARD` Artifact Registry repository, created outside this
>    module or added to it.
> 2. The image path is not a variable. `image_tag` is the only image-related
>    input; the repository and path are fixed in the `mirrored_image` local
>    (`terraform/main.tf`), which is also the place
>    [terraform/README.md](../terraform/README.md) points at for path changes.
>    Repoint that local at your own repository.
>
> The rest of the module — scheduler, service accounts, secret bindings — is
> unaffected, as is building and running from source outside GCP.

## 4. Verify the first execution

Either wait for the next scheduled tick, or trigger one manually — `my-liquidator-deploy` (from step 3) exports `scheduler_job_name`, so you don't need to guess the generated name:

```bash
gcloud scheduler jobs run "$(terraform output -raw scheduler_job_name)" --location <region> --project "$PROJECT_ID"
```

Then check **Cloud Run → Jobs → your job → Executions → Logs**. The example ships `env.DRY_RUN = "true"`, so this first run — and every run until you deliberately change it — scans markets and logs what it *would* liquidate without submitting anything on-chain. Confirm:

- The registry refresh finds the markets you expect.
- Scans complete without RPC errors (watch for rate-limiting if you're on a public RPC — see the README FAQ on private RPCs).
- Logged liquidation candidates (if any) and their sizing look sane against `MIN_PROFIT_BPS`.

Run several scheduled executions this way before touching `DRY_RUN` — a bad config (wrong registry, wrong strategy flags, an RPC that can't keep up) is much cheaper to find while every "liquidation" is simulated.

## 5. Flip `DRY_RUN`

Once you trust the configuration:

```hcl
# terraform.tfvars or main.tf's env map
DRY_RUN = "false"
```

```bash
terraform apply
```

This is deliberately a separate, explicit step — going live should never be a side effect of an unrelated Terraform change. See [`terraform/README.md`](../terraform/README.md#safety-simulation-is-the-default) for the exact matching rule (`DRY_RUN` must be the literal string `"false"`; anything else stays in simulation or fails closed at startup).

## The overlap rule

`task_timeout_seconds` **must stay below the schedule interval.** The Cloud Run Job runs with `max_retries = 0` on purpose — a failed cycle isn't retried in place, the next scheduled tick is the retry. If the timeout is allowed to reach (or exceed) the schedule interval, a slow cycle can still be running when the next scheduled execution fires, and you get two concurrent liquidation attempts against the same positions instead of one clean retry cadence.

Size the timeout by measurement, not guesswork: it needs to comfortably exceed `markets × (scan time per market + 5s inter-market delay)`, plus swap settlement time if `COLLATERAL_STRATEGY=swap-to-borrow`, plus slack for a rate-limited RPC (a single 429 can cost a 60s backoff on its own). Start from the defaults (`schedule = "*/10 * * * *"`, `task_timeout_seconds = 480`), watch how long your real executions take in the Cloud Run Jobs logs, and raise both the timeout and the schedule interval together if cycles start approaching the limit — a growing registry or a flakier RPC both push the required timeout up over time. See [`terraform/README.md#the-overlap-rule`](../terraform/README.md#the-overlap-rule) for the full reasoning.

## Upgrading and rollback

The module's `ref` (if you consume it via `source = "git::https://github.com/templar-protocol/templar-liquidator//terraform?ref=vX.Y.Z"`) and `image_tag` should move together — one version bump upgrades infrastructure and binary atomically, and rolling back is the same edit backwards.
