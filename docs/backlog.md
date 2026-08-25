# Backlog

Documented, not implemented. This is the roadmap of things a fork or a future release might tackle, roughly grouped by theme. None of these are required to run the bot as shipped — they're either scaling answers for higher-volume operation, or observability/DX gaps that this v1 knowingly left open.

## Correctness and robustness

**Per-account cooldown after failed liquidation attempts.** A position that fails repeatedly (e.g. a persistent `OfferTooLow` or `ExcessiveLiquidation` from price drift) currently gets re-attempted every scan cycle with no backoff. A cooldown after N consecutive failures for the same account would reduce wasted RPC/gas churn.

## Scaling

**Priority-queue account tracking with adaptive refresh.** The current model scans every position in every configured market on every cycle. At meaningful scale (many markets, many positions per market) that stops being cheap; a priority queue that refreshes near-threshold positions more often than healthy ones — the pattern used by more mature liquidation bots on other chains — is the documented answer if scan-everyone ever becomes the bottleneck.

**Auto-rebalancing of inventory.** `COLLATERAL_STRATEGY=swap-to-borrow` rebalances what the bot *receives* from liquidations, but nothing proactively tops up a depleted borrow-asset balance from elsewhere. Until this exists, rebalancing across assets is a manual, documented operator procedure (see [docs/economics.md](economics.md#inventory-sizing)).

**Drill / simulation mode.** A Gearbox-style mode that runs against a `near-sandbox` instance, forces positions underwater, liquidates all of them, and produces a JSON report — useful both as a pre-deployment smoke test for a fork's configuration and as protocol-side security tooling (stress-testing a market's liquidation parameters without touching real funds or real chain state).

## Operability

**Low-health digest notifications.** Beyond per-liquidation and per-failure alerts, a scheduled digest of positions approaching (but not yet at) the liquidation threshold — an early-warning signal, not an action trigger.

**Per-asset wallet-balance gauges in `/metrics`.** Deferred from the initial metrics work because it needs labelled series (one gauge per asset, not a fixed field). The renderer now supports labels — `templar_liquidator_inventory_reserved_raw{asset=…}` is the first labelled family — so what remains is the balance gauge itself: a snapshot of tracked balances published from the inventory refresh path, alongside the reserved gauge in [`metrics.rs`](../src/metrics.rs).

**Add a real liveness endpoint.** `/healthz` is already correctly a pure readiness check — it 503s until at least one market has scanned cleanly recently, and never reports "process is up" on its own (see the module doc for [`http.rs`](../src/http.rs)). That's exactly why it's unsafe to wire to a liveness/restart probe: a bot stuck on a persistent RPC problem would restart-loop forever without fixing anything. What's actually missing is a *separate* liveness signal an orchestrator could use for that purpose — conventionally named `/livez` (not `/readyz`, which in the usual k8s-style split names the readiness check, the role `/healthz` already fills here) — so "restart me, the process is wedged" and "I'm up but not ready" stay distinguishable.

**Once-mode cycle-timeout wrapper.** `RUN_MODE=once` relies entirely on the external scheduler's own timeout to bound a stuck cycle. A wrapper timeout inside the binary itself would fail a stuck cycle deterministically rather than depending on the orchestrator to notice.

## Configuration and interface

**Config-file support (`--config config.toml`).** Would complete the CLI ≡ env ≡ file trinity — right now every setting is CLI-flag-or-env-var only, with no way to check a full configuration into version control short of a `.env` file (which mixes secrets with non-secret settings).

**`--dry-run` accepting `0`/`1`/`yes`/`no`.** The env var form is intentionally strict — only the literal strings `true`/`false` parse, anything else aborts at startup (see [docs/configuration.md](configuration.md#safety-dry_run)). That strictness is a deliberate safety property and should stay; what's open is whether the *value space* itself should widen to include a few more unambiguous truthy/falsy spellings while keeping everything else fail-closed.

**Compose overlay for prod.** `docker-compose.prod.yml` today is a full second file with its own copy of most settings rather than a Compose override layered on top of `docker-compose.yml`. An overlay would shrink the surface that can drift between the two.

## Build and dependencies

**cargo-chef Docker layer caching.** The current [`Dockerfile`](../Dockerfile) copies `Cargo.toml`/`Cargo.lock`/`src` and runs `cargo build --release` in one layer; any source change invalidates that layer and triggers a full from-scratch dependency compile (there's no separate dependency-only layer to reuse). `cargo-chef` would cache the dependency-compilation layer independently of source changes.

## Architecture

**Library-first restructure (core crate + thin bin), if third-party demand appears.** Right now the crate is a `[lib]` + `[[bin]]` in one package, which is enough for "fork and configure." If forks start wanting to depend on the liquidation core as a library while swapping out the binary's CLI/wiring, splitting into a core crate + thin binary crate would be the next step — not worth doing speculatively ahead of that demand.
