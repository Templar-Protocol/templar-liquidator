# templar-liquidator

## What this is

An inventory-based liquidation bot for Templar Protocol lending markets on NEAR.
It holds a pool of borrow-asset tokens, repays the debt of underwater positions
directly from that inventory, and receives their collateral at a discount in
return. Received collateral can optionally be swapped back into borrow assets
so the same inventory is available for the next round.

**This bot is NOT non-custodial.** Unlike the Templar smart contracts it
trades against, it holds a signer key (`SIGNER_ACCOUNT_ID` / `SIGNER_KEY`) and
submits transactions itself — that is the point of a liquidation bot. Treat the signer key with the
weight that implies: it moves real inventory, and the bot is expected to run
unsupervised. Dry-run is the default for exactly this reason (see Safety
invariants below).

This repo is published as a **public reference implementation**. The expected
way to adapt it to different parameters or a different venue is to fork and
configure via CLI flags / env vars, or — where configuration can't express the
change — extend one of the three seams called out in `src/liquidator.rs`'s
crate-level docs: `swap::SwapProvider`, `liquidation_strategy::LiquidationStrategy`,
or `notifier::Notifier`.

## Orientation commands

```bash
cargo test --lib --bins            # unit tests (what CI runs on every PR)
cargo clippy --all-targets -- -D warnings
cargo fmt --all
make help                          # Docker Compose lifecycle (build/start/stop/logs)
docker compose up                  # run the bot in dry-run mode via Docker
```

## Module map

One line per file in `src/`:

- `main.rs` — binary entry point: sets up tracing, parses `Args`, builds the
  service config, runs it.
- `liquidator.rs` — lib root. Crate-level docs describe the liquidation
  pipeline and the three extension seams; also owns the error taxonomy
  (`LiquidatorError`, `ErrorPhase`, `NotificationKind`, `LiquidationOutcome`,
  `CollateralStrategy`) and the top-level `Liquidator` driving `liquidate()`
  and `run_liquidations()`.
- `config.rs` — CLI argument parsing (`Args`, `clap`) and translation into
  `ServiceConfig`.
- `service.rs` — service lifecycle: `run()` is the long-lived loop
  (registry refresh + liquidation rounds on independent timers), `run_once()`
  performs exactly one registry refresh and one liquidation round, then drains
  pending notifications before returning.
- `scanner.rs` — fetches borrow positions per market and checks liquidation
  status, with pagination for large markets.
- `liquidation_strategy.rs` — `LiquidationStrategy` trait and the built-in
  percentage-of-inventory / fixed-USD-amount sizing policies.
- `profitability.rs` — gas-cost and collateral-value conversions; decides
  whether a sizing decision clears the configured profit margin.
- `executor.rs` — builds and submits the liquidation transaction, manages
  inventory debits/credits, and triggers the optional immediate collateral
  swap.
- `inventory.rs` — `InventoryManager`: tracks available balances across all
  markets and assets so liquidations only proceed when inventory covers them.
- `oracle.rs` — price fetching across oracle types the bot reads: Pyth
  (Hermes HTTP), LST feeds with transformers, and proxy-oracle feeds —
  composed off-chain from each feed's configured source (Hermes / RedStone
  API + transformer view calls) at scan time, with the proxy's on-chain
  cache as fallback. Execution-time pricing still goes through the on-chain
  push (`update_onchain_prices`).
- `redstone.rs` — RedStone public price API client (`api.redstone.finance`),
  used only for scan-side proxy price composition.
- `swap/mod.rs` — `SwapProvider` trait, the swap-provider extension seam.
- `swap/provider.rs` — `SwapProviderImpl`, a concrete enum wrapping the
  shipped provider for dynamic dispatch (the trait itself has generic
  methods and can't be a trait object); a fork's venue becomes a new
  variant here.
- `swap/oneclick.rs` — 1-Click API provider (NEAR Intents cross-chain swaps):
  quote → deposit → poll.
- `swap/retry.rs` — swap error classification (retryable vs. permanent) and
  the generic retry wrapper swaps run through.
- `notifier.rs` — Telegram notifications; the notification extension seam
  (no trait boundary yet — a fork adding another channel extends or replaces
  this type directly).
- `metrics.rs` — dependency-free Prometheus text-format counters/gauges,
  process-lifetime, exposed via `http.rs`.
- `http.rs` — optional `GET /healthz` (readiness, not liveness) and
  `GET /metrics`; only started when `HTTP_PORT` is set, never in
  `--run-mode once`.
- `format.rs` — human-readable asset ticker formatting for logs.
- `rpc.rs` — error taxonomy for the RPC/gateway boundary (`RpcError`,
  `AppError`) and NEP-330 contract-source-metadata shapes; the low-level
  blockchain plumbing itself lives in the in-process `templar-gateway-client`
  dependency, not here.

## Conventions

- `clippy::pedantic` is warn-level with `unwrap_used = "deny"` (see
  `Cargo.toml`'s `[lints.clippy]`); tests are exempted via
  `allow-unwrap-in-tests` / `allow-expect-in-tests` in `clippy.toml`, not by
  disabling the lint.
- Structured `tracing` logs, not `println!`/ad hoc formatting.
- Doc comments state constraints and invariants, not a narration of what
  changed — read `notifier.rs` and `http.rs` for the style.
- On-chain amounts use the big-number types from `templar-common`
  (`Decimal`, `U128`-wrapped fungible-asset amounts); `f64` is for display
  and USD-denominated config knobs only (e.g. `FIXED_LIQUIDATION_AMOUNT_USD`,
  `ProfitabilityCalculator::DEFAULT_GAS_COST_USD`).

## THE SINGLE-REV RULE

Every `templar-*` git dependency in `Cargo.toml` (`templar-common`,
`templar-gateway-client`, `templar-gateway-core`, `templar-gateway-methods-spec`,
`templar-gateway-oracle-updates-dispatch`, `templar-gateway-oracle-updates-spec`,
`templar-gateway-types`, `templar-proxy-oracle-near-common`, plus the
dev-dependencies `templar-gateway-testing` and `test-utils`) must stay pinned
to the **same** `rev`. If any one of them drifts to a different rev, Cargo
resolves two separate checkouts of the same upstream repo, and types that look
identical (e.g. two `MarketConfiguration`s) won't unify — the build fails with
confusing "expected X, found X" errors that don't look like a version problem.
Bump every `rev =` together in one change, never one alone.

## Safety invariants a change must not break

- **Dry-run is the default.** `DRY_RUN` (env) / `--dry-run` (flag) defaults to
  `true`. Live trading requires explicitly setting `DRY_RUN=false` — there is
  no other way to opt in. The flag accepts an optional value so it's usable
  from argv-only surfaces (bare `--dry-run` means true; `--dry-run=false` /
  `--dry-run false` opt out); the env var itself parses only the literal
  strings `true`/`false`, nothing else.
- **`--run-mode once` must drain notifications before returning.**
  `run_once()` explicitly drains the notifier after the liquidation cycle
  (success or error path) because the tokio runtime shutting down when `main`
  returns would otherwise cancel an in-flight Telegram POST.
- **`/healthz` must only report healthy when at least one market scanned
  cleanly, recently.** It's a readiness check, not a liveness check — see
  `http.rs`'s module doc for why wiring it to a liveness probe would be
  actively wrong (it would restart-loop a bot stuck on a persistent RPC
  problem without fixing anything).

## Gotchas

- MCR (minimum collateral ratio) values read from live mainnet markets can be
  decimal strings (`"1.25"`) or legacy 24-decimal integers, depending on when
  a given market contract was deployed — both shapes must parse. Don't assume
  the shape from the pinned `templar-common` type alone; verify against the
  actual on-chain market you're reading.
- yoctoNEAR (10⁻²⁴ NEAR — the unit for deposits and balances) is **not** the
  same unit as NEAR gas. Don't mix them in a calculation or a config knob.
- Oracle prices fail closed when stale: the on-chain read this bot depends on
  rejects prices older than its configured threshold rather than returning a
  stale value silently. A scan can legitimately come back empty/degraded
  because of this, not because of a bug.
- The dev container is memory-constrained by whatever Docker Desktop is given
  (commonly ~8 GB), and `nproc` reports the host's full core count — so cargo
  fans out far more parallel jobs than there is RAM for. Both `cargo test
  --lib --bins` and `cargo install` die there under defaults, with `signal: 9`
  or `collect2: fatal error: ld terminated with signal 9`. Capping jobs with
  `CARGO_BUILD_JOBS=1` helps both, but the debug-info knob is **per profile**,
  so the two cases need different variables:

  ```bash
  # dev/test profiles — cargo test, cargo build, cargo clippy
  CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo test --lib --bins
  # release profile — cargo install builds here, so the dev/test knobs do nothing
  CARGO_BUILD_JOBS=1 CARGO_PROFILE_RELEASE_DEBUG=0 cargo install <crate>
  ```

  Raising Docker's memory limit is the better fix; `.devcontainer/post-create.sh`
  sizes its own job count from the cgroup limit (falling back to
  `MemAvailable`) for this reason.
- The sandbox integration test (`tests/liquidation_sandbox.rs`) is
  `#[ignore]`d — it needs a `neard` sandbox plus prebuilt contract wasms
  (resolved at compile time through `CARGO_WORKSPACE_DIR`, which
  `.cargo/config.toml` in this repo points at this repo's own root, not a
  contracts checkout) and cannot run from a plain `cargo test`. See
  `docs/testing.md` for how to run it. Linking the test binary is
  memory-hungry — pass `-j 1` on constrained machines, and if the link still
  dies with `signal 9`, add `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`
  (most of the linker's memory is debug info, and dropping it costs only
  backtrace line numbers).

## Workflow

1. Branch → PR against `main`.
2. CI must be green: fmt, clippy, unit tests, docs, `cargo-deny`, Docker
   build.
3. Releases are tags `vX.Y.Z`, which publish a GHCR image and a GitHub
   Release.

## Where things live

- `src/` — the crate (binary `liquidator`, lib root `src/liquidator.rs`).
- `tests/` — integration tests, including the sandbox test (see Gotchas).
- `docs/` — architecture, configuration, deployment, and testing reference.
- `.github/workflows/` — CI and release automation.
