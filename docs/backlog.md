# Backlog

Documented, not implemented. This is the roadmap of things a fork or a future release might tackle, roughly grouped by theme. None of these are required to run the bot as shipped — they're either scaling answers for higher-volume operation, or observability/DX gaps that this v1 knowingly left open.

## Correctness and robustness

**Enforce `LiquidationStrategy::max_liquidation_percentage()`.** It exists today purely for logging (`liquidator.rs`'s `run_liquidations()` logs it, nothing checks the sizing output against it) — see the doc comment on [`LiquidationStrategy::max_liquidation_percentage`](../src/liquidation_strategy.rs). Both built-in strategies happen to respect their own configured percentage internally, so this is currently an unenforced convention rather than a bug, but a third-party strategy implementation has no compiler- or runtime-level guarantee its `calculate_liquidation_amount` actually stays within what it claims via this method.

**Zero-check strategy output before submission.** [`Liquidator::liquidate`](../src/liquidator.rs) passes whatever `calculate_liquidation_amount` returns straight through to `execute_liquidation` — it never independently checks the repay/collateral amounts for zero. The trait's doc comment is explicit that returning `Some((U128(0), _))` instead of `None` would attempt a zero-amount on-chain liquidation, and both shipped strategies self-guard against this (each checks and returns `None` before ever constructing a zero amount). A new strategy implementation must uphold the same invariant itself — the caller doesn't do it for you.

**Split `LiquidationOutcome::Skipped`.** Today it covers two different situations that get folded into one bucket: a position that's genuinely healthy, and one that's liquidatable but the bot lacked the inventory (or hit the contract minimum) to act on it. `RoundSummary::candidates` explicitly excludes both from its count for exactly this reason — see the doc comment on the field. Splitting them would make "how much liquidatable debt did we have to skip due to insufficient inventory" a countable, alertable number instead of invisible.

**Per-account cooldown after failed liquidation attempts.** A position that fails repeatedly (e.g. a persistent `OfferTooLow` or `ExcessiveLiquidation` from price drift) currently gets re-attempted every scan cycle with no backoff. A cooldown after N consecutive failures for the same account would reduce wasted RPC/gas churn.

**Validate `SIGNER_KEY` before startup, don't panic on it.** A mismatched-but-well-formed key (parses as a valid `near_crypto::SecretKey` but doesn't match `SIGNER_ACCOUNT_ID` on-chain) currently panics deep in `LiquidatorService::new` (`src/service.rs`) with a raw "invalid signer secret key: ... Mismatched Keypair detected" message and a nonzero-but-unstructured exit, instead of failing with a clean, actionable error and exit code at config-parse time. Verified live against a real misconfigured key.

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

**`value_delimiter = ','` on `REGISTRY_ACCOUNT_IDS`.** Unlike `ALLOWED_COLLATERAL_ASSETS` / `IGNORED_COLLATERAL_ASSETS` / `IGNORED_MARKETS`, the `--registries` arg (`src/config.rs`) has no `value_delimiter`, so `REGISTRY_ACCOUNT_IDS=a.near,b.near` parses as one invalid `AccountId` instead of two registries — only the CLI flag's repeatable form (`--registries a.near --registries b.near`) currently expresses multiple registries. Adding the delimiter would make the env var consistent with the other list-typed settings.

**`--dry-run` accepting `0`/`1`/`yes`/`no`.** The env var form is intentionally strict — only the literal strings `true`/`false` parse, anything else aborts at startup (see [docs/configuration.md](configuration.md#safety-dry_run)). That strictness is a deliberate safety property and should stay; what's open is whether the *value space* itself should widen to include a few more unambiguous truthy/falsy spellings while keeping everything else fail-closed.

**Compose overlay for prod.** `docker-compose.prod.yml` today is a full second file with its own copy of most settings rather than a Compose override layered on top of `docker-compose.yml`. An overlay would shrink the surface that can drift between the two.

## Build and dependencies

**cargo-chef Docker layer caching.** The current [`Dockerfile`](../Dockerfile) copies `Cargo.toml`/`Cargo.lock`/`src` and runs `cargo build --release` in one layer; any source change invalidates that layer and triggers a full from-scratch dependency compile (there's no separate dependency-only layer to reuse). `cargo-chef` would cache the dependency-compilation layer independently of source changes.

**`reqwest` 0.11 → 0.12.** Tracked in [`deny.toml`](../deny.toml) as the real fix for the `rustls-pemfile` unmaintained-advisory allowance: `reqwest` 0.12 drops `rustls-pemfile` entirely. No functional motivation beyond that today.

## Architecture

**Library-first restructure (core crate + thin bin), if third-party demand appears.** Right now the crate is a `[lib]` + `[[bin]]` in one package, which is enough for "fork and configure." If forks start wanting to depend on the liquidation core as a library while swapping out the binary's CLI/wiring, splitting into a core crate + thin binary crate would be the next step — not worth doing speculatively ahead of that demand.
