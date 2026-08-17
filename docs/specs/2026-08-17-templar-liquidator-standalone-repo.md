# Templar Liquidator — Standalone Public Repo

**Date:** 2026-08-17
**Status:** Approved design, pre-implementation
**Destination:** this spec ships as part of the first commit of
`Templar-Protocol/templar-liquidator` (`docs/specs/` there). It lives
uncommitted in templar-backend only while the new repo is being stood up.

## 1. Goal

Move the liquidation bot from `Templar-Protocol/contracts/service/liquidator`
(public monorepo, GPL-3.0) into the already-created public repo
`Templar-Protocol/templar-liquidator`, as a standalone, fork-and-configure
Rust application that:

- works out of the box (`docker compose up` in dry-run with only an RPC URL),
- is the reference liquidator for Templar — and, given the absence of any
  credible public NEAR liquidator, for NEAR generally,
- is deployable by us to a dedicated GCP project via a generic, public
  Terraform module, with all Templar-specific configuration kept private,
- serves both large orgs and individuals as an adoption starting point.

Competitor research (18 public liquidator repos across EVM/Solana/NEAR/
Stellar/TON; see §14) grounds the polish scope.

## 2. Decision ledger

| # | Decision | Choice |
|---|---|---|
| 1 | Git history | Fresh start; initial commit records `Migrated from Templar-Protocol/contracts@<sha>` |
| 2 | Adoption model | Fork & configure (runnable app; extension points stay in-tree traits) |
| 3 | Code structure | Lift & polish in place — current module layout unchanged |
| 4 | Our runtime | Cloud Run **Job** + Cloud Scheduler (~10 min), not an always-on service |
| 5 | Old location | `contracts/service/liquidator` stays until new repo ships (tag + proven deploy), then deletion PR + pointer README |
| 6 | Infra home (public) | Generic variable-driven Terraform module in the public repo |
| 7 | Infra home (ours) | New private repo `templar-liquidator-infra`, operator-applied |
| 8 | Move scope | Migration + polish pass (small code delta, big docs delta) |
| 9 | License | GPL-3.0 (matches contracts repo; linked crates are GPL anyway) |
| 10 | Image publishing | GHCR on release tags; our side mirrors via AR remote repository |

## 3. Migration mechanics

- Source: contracts repo commit `8f8fe1d1a057756f71438abc75d7b3c688b282f0`
  (release `chore: release (#580)`) — verified to be the last commit touching
  `service/liquidator` on `dev`, so source rev and dependency pin coincide.
  Re-verify at implementation time; if the path has newer commits, move both
  together to the newest release commit covering them.
- New repo: `main` is the only long-lived branch (already default); feature
  branches → PRs → `main`; releases are tags. Crate version continues from
  `0.1.4` (set by release #580) → first standalone release is `v0.2.0`.
- The existing placeholder `README.md` in the new repo is replaced.
- Nothing in the new repo may reference private Templar repos, internal
  tickets, or our GCP project layout.

## 4. Dependency strategy

- The workspace-inherited `templar-*` crates become **git dependencies on the
  public contracts repo, all pinned to ONE uniform release rev** (same
  pattern and rationale as templar-backend's `services/blockchain-gateway`:
  mixed revs produce duplicate checkouts whose types don't unify):
  `templar-common` (features `rpc`), `templar-gateway-client` (`clap`),
  `templar-gateway-core`, `templar-gateway-methods-spec`,
  `templar-gateway-types`, `templar-proxy-oracle-near-common`; dev-deps
  `templar-gateway-testing`, `test-utils` from the same rev.
- All other `workspace = true` deps get explicit versions materialized from
  the contracts workspace (`near-api 0.8.6`, `near-account-id 2.6`,
  `clap 4.6` +derive+env, `async-trait 0.1.89`, `futures 0.3.31`,
  `hex 0.4.3` +serde, etc. — read exact set from the workspace manifest at
  migration time).
- `Cargo.lock` is committed (application, not library).
- `rust-toolchain.toml` pins a recent stable — default to the toolchain
  templar-backend's blockchain-gateway builds with (Rust 1.97) — the wasm
  target and 1.86 pin of the contracts workspace do not apply here; verify
  the pinned crates compile on the chosen toolchain.

## 5. Run modes and safety defaults

- **Loop mode (default today, unchanged):** the existing
  `select!`-over-intervals loop (`LIQUIDATION_SCAN_INTERVAL`, default 600s;
  `REGISTRY_REFRESH_INTERVAL`).
- **New `--once` / `RUN_MODE=once`:** registry refresh → inventory refresh →
  optional batch collateral swap → one liquidation round → exit (non-zero on
  fatal init errors). This is what our Cloud Run Job runs, and equally the
  cron / K8s-CronJob path for third parties. Overlap protection is the
  platform's job (task timeout < schedule interval) — no new locking code.
- **Dry-run becomes the default.** Sending transactions requires explicit
  `DRY_RUN=false`. A public bot that moves money must be safe by default;
  no surveyed competitor does this.

## 6. v1 code delta (complete list — everything else is backlog)

1. Cargo standalone conversion (§4).
2. `--once` mode (§5).
3. Dry-run default flip (§5).
4. Optional ops surface: `HTTP_PORT` env (off by default) serving `/healthz`
   and Prometheus `/metrics` — counters/gauges: scans run, candidates found,
   liquidations attempted/succeeded/failed, profit USD, wallet balances, last
   successful scan timestamp. Zero surveyed competitors ship real metrics.
   Irrelevant in `--once` mode; recommended in loop mode.
5. Rustdoc on the extension seams (`SwapProvider`, `LiquidationStrategy`,
   notifier) so `cargo doc` works as the extension guide.
6. Whatever mechanical renames the standalone crate requires (binary name
   stays `liquidator`, crate `templar-liquidator`).

Code lands otherwise functionally unchanged and reviewable against the
contracts-repo baseline.

## 7. Repo layout

```
templar-liquidator/
├── src/                  # lifted as-is (scanner/strategy/executor/
│                         # profitability/inventory/swap/notifier/oracle/...)
├── tests/                # incl. liquidation_sandbox.rs
├── Cargo.toml / Cargo.lock / rust-toolchain.toml
├── Dockerfile / docker-compose.yml / .env.example
├── terraform/            # generic GCP module + examples/ + README
├── docs/                 # architecture.md, configuration.md, economics.md,
│                         # deploy-gcp.md, deploy-vm.md, TESTNET.md,
│                         # backlog.md, specs/ (this file)
├── .devcontainer/        # Rust toolchain + terraform + gh; lean
├── .github/workflows/    # ci.yml, release.yml
├── CLAUDE.md             # + .claude/ (public-safe, project-technical only)
├── README.md / LICENSE (GPL-3.0) / SECURITY.md / CONTRIBUTING.md
```

The old repo's VM scripts (`init-server.sh`, `deploy.sh`,
`setup-loki-grafana.sh`, run-*.sh) are reviewed during the move: keep what
supports the generic VM path (as `scripts/` + `docs/deploy-vm.md`), drop what
is Templar-ops-specific.

## 8. CI, release, image publishing

- **CI on PR:** `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test`,
  `cargo-deny` (advisories + licenses), Docker build.
- **Sandbox integration test:** `workflow_dispatch` + nightly schedule (needs
  NEAR sandbox binaries; too heavy per-PR).
- **Release:** tag `vX.Y.Z` → GitHub Release + GHCR image
  `ghcr.io/templar-protocol/templar-liquidator:<tag>`, so the fastest path is
  `docker run --env-file .env ghcr.io/...`.
- **No GCP credentials in the public repo, ever.** Fork PRs must not be able
  to reach anything of ours; the only publish credential is the repo-scoped
  `GITHUB_TOKEN` for GHCR.

## 9. Public Terraform module (`terraform/`)

Variable-driven, no Templar values baked in:

- Cloud Run Job running the GHCR image in `--once` mode.
- Cloud Scheduler trigger (default `*/10 * * * *`), job task timeout below
  the schedule interval (default ~8 min), retries 0 — the next tick is the
  retry.
- Artifact Registry **remote repository** mirroring `ghcr.io` (Cloud Run
  cannot pull GHCR directly).
- Service account with least privilege; Secret Manager secret *references*
  (module never creates secret values) for `SIGNER_KEY`, RPC API key,
  1-Click token, Telegram token.
- Optional log-based alert policies: job execution failed; no successful run
  in N hours (silent-stall class).
- `terraform/examples/` — a complete fictional deployment; README:
  "deploy your own on GCP in ~15 minutes."

## 10. Docs set

README order (what the best surveyed repos converge on): one-paragraph
what-it-is → docker compose quickstart (dry-run by default) → economics
section with a worked numeric example using Templar's actual liquidation
discount / close-factor numbers → numbered "how it works" pipeline with
source links → mermaid architecture diagram → config reference table (every
knob as env var ≡ CLI flag, side by side) → FAQ (RPC choice + FastNEAR
guidance, what inventory the operator must hold, rebalancing behavior) →
liability disclaimer.

`docs/TESTNET.md`: end-to-end walkthrough — create a position on testnet,
drive it underwater, watch the bot liquidate it.
`docs/backlog.md`: the documented roadmap (§13).
`CLAUDE.md`: orientation, commands, architecture map, conventions, gotchas
(MCR decimal-string vs 24-dec dual shape, yoctoNEAR vs gas units, oracle
staleness, single-rev pinning rule). Public-safe: no internal references.

## 11. Our deployment (private side)

New private repo `templar-liquidator-infra`, operator-applied (the Blend
separate-project precedent; all infra through Terraform, no manual gcloud):

```
bootstrap/          # once: GCP project, APIs, state bucket
envs/mainnet/       # backend.tf (GCS state), main.tf consuming
                    #   git::…/templar-liquidator//terraform?ref=vX.Y.Z
                    # terraform.tfvars: project, region, schedule,
                    #   image_tag, env map, notification channels
envs/testnet/       # optional, reduced params
README.md           # runbooks: bump version, rollback, rotate key,
                    # pause (Scheduler), fund inventory
```

- Module `ref` and `image_tag` move together — one version bump upgrades
  infra + binary atomically; rollback is the same edit backwards.
- Secrets seeded once outside TF via `printf '%s'` (no trailing newline);
  rotation = add a secret version, next execution picks it up.
- Runtime config: `RUN_MODE=once`, explicit `DRY_RUN=false`, registry
  account ids, FastNEAR RPC URL + key.
- Bot account: dedicated NEAR account holding borrow-asset inventory;
  funding levels + top-up procedure in the runbook.
- Alerting, two layers: the bot's Telegram notifier (executed/failed,
  scan-failure threshold) + module alert policies → our channels.

## 12. Testing

- Existing unit tests move as-is and must stay green.
- `liquidation_sandbox.rs` keeps working via dev-deps from the pinned rev;
  runs nightly/manual in CI (§8).
- Parity check during migration: `cargo test` green + a loop-mode dry-run
  against testnet/mainnet RPC producing a scan equivalent to the old bot's.
- New tests only where the v1 delta adds behavior (`--once` exit paths,
  dry-run default, metrics endpoint smoke).

## 13. Backlog (documented in-repo, not v1)

From competitor research, roughly ordered by leverage:

1. **Drill mode** — Gearbox-style optimistic simulation against
   near-sandbox: force positions underwater, liquidate all, JSON report;
   doubles as protocol security tooling.
2. Config-file support (`--config config.toml`) completing the
   CLI ≡ env ≡ file trinity.
3. Per-account cooldown after failed liquidation attempts.
4. Priority-queue account tracking with adaptive refresh (the scaling
   answer when scan-everyone stops being enough; Euler pattern).
5. Library-first restructure (core crate + thin bin) if third-party demand
   materializes.
6. Auto-rebalancing of inventory (until then: documented manual procedure).
7. Low-health digest notifications (Euler-style scheduled report).

## 14. Competitor research (condensed)

Full survey lives in the brainstorming session (18 repos, 2026-08-17).
Load-bearing findings:

- **No credible public NEAR liquidator exists** — Burrow's is archived JS
  configured by editing `run.sh` with an undocumented Postgres dependency.
- **Best-engineered references:** morpho-org/morpho-blue-liquidation-bot
  (modular fork-and-configure, 73 forks), Gearbox liquidator-v2 (config
  trinity, optimistic mode, GHCR quickstart), dYdX liquidator (worked
  economics example), Euler bot (pipeline docs, health priority queue),
  blend-capital/liquidation-bot (Rust/Artemis on a non-EVM sibling protocol).
- **Table stakes:** .env.example, one-command Docker run, min-profit knob,
  economics explainer, market scoping, private-RPC guidance, license +
  disclaimer, prebuilt image.
- **Differentiators nobody ships:** real Prometheus metrics, cloud Terraform,
  safe-by-default dry-run, sandbox drill mode.
- **Anti-patterns:** README-only launches, no license, config-by-editing-
  scripts, compile-time network selection, mandatory databases, manual last
  steps, protocol-monorepo coupling (the Compound/comet trap this migration
  escapes).

## 15. Sequencing

1. New repo: spec + skeleton + code lift; builds, unit tests green.
2. v1 code delta (§6). *(Switch to the new repo's devcontainer around here.)*
3. CI + release workflow + first GHCR image.
4. Terraform module + examples.
5. Docs set (§10).
6. Tag `v0.2.0`.
7. Private side: `templar-liquidator-infra` bootstrap + mainnet apply;
   proven scheduled runs.
8. Follow-up PR to contracts repo: delete `service/liquidator`, leave
   pointer README.

## 16. Out of scope

- Any change to the contracts repo other than the final deletion PR.
- The backend monorepo (templar-backend) is untouched by this project.
- New liquidation features beyond §6 (they live in §13).
- Custody changes: the bot keeps signing in-process with its operator's key
  (`templar_gateway_client::SigningClient`); it does not use the backend's
  blockchain-gateway service.
