# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Removed

- [**breaking**] `TRANSACTION_TIMEOUT` / `--transaction-timeout` and `REDSTONE_GATEWAY_URL` / `--redstone-gateway-url`. Both were documented no-ops — nothing consumed either value (the gateway client exposes no timeout knob; RedStone prices come from `REDSTONE_API_URL` and on-chain pushes go through the proxy contract's own `update_prices` flow). Deployments passing either **flag** now fail at startup with an unknown-argument error; the env vars are simply ignored. Compose files and run scripts in this repo no longer set them.

- The GCP deployment (the `terraform/` Cloud Run Job + Scheduler module and `docs/deploy-gcp.md`), moved to a private Templar infra repo to keep this reference implementation platform-generic. `RUN_MODE=once` and its cron-style-scheduler guidance stay — they are platform-neutral. Forks that want the module can recover it and its walkthrough from git history at `v0.2.0` (`terraform/`, `docs/deploy-gcp.md`); the CI `terraform` job and the Terraform Dependabot ecosystem went with it.
- [**breaking**] The Ref Finance swap provider (`swap/ref.rs`, `RefSwap`) and its `REF_CONTRACT` / `--ref-contract` knob. Two reasons: it was dead wiring (the service constructed it and immediately discarded it — only 1-Click was ever used for JIT and batch swaps), and it was unsafe to wire in as-is: its `min_amount_out` was derived from the *input* amount with no price or decimals conversion, giving no effective slippage protection for any non-1:1 pair (a BTC→USDC swap would have accepted nearly any execution price). Passing `--ref-contract` now fails at startup as an unknown flag; the `REF_CONTRACT` env var is simply ignored. The multi-provider architecture stays: `SwapProvider` (trait) + `SwapProviderImpl` (dispatch enum) are the seam — a fork adds a venue by implementing the trait and adding a variant, and `swap/mod.rs` documents the slippage bar any AMM provider must clear.

### Added

- `LOG_FORMAT=json` — one JSON object per log line, for deployments shipping logs to an aggregator (Loki, CloudWatch); the default human-readable format is unchanged.

- Off-chain proxy-oracle price composition at scan time. Proxy-backed markets previously required a keeper to have recently pushed the proxy's on-chain price cache before they could even be *scanned* — in a standalone deployment (any fork not running a price-push keeper) every proxy market failed with `Missing price` forever. The scanner now reads each feed's source config from the proxy contract (a free view call) and composes the price off-chain: Pyth sources via Hermes, RedStone sources via the public price API (`REDSTONE_API_URL` / `--redstone-api-url`, default `https://api.redstone.finance`), transformers applied with their on-chain input (also a free view call). Feeds that can't be composed off-chain (e.g. Lazer-sourced) fall back to the on-chain cache read as before. Costs no gas anywhere; execution-time pricing is unchanged — the bot still pushes fresh prices on-chain before a live liquidation and the market contract still enforces its own oracle, freshness bound, and circuit breakers.
- `REDSTONE_GATEWAY_URL` is now documented as unused (it already was: the parameter it fed was discarded). Retained for compatibility; removal is tracked with the other dead knobs in the architecture review.
- `POSITION_CONCURRENCY` (`--position-concurrency`, default `1`) — how many positions one market's round evaluates/liquidates in flight at once. The default keeps the historical behavior exactly: sequential, with a 1-second pause between positions. Raising it drops the pause and fans evaluation out through a bounded pool; inventory reservations already serialize the capital commitment, so concurrent positions cannot double-spend the same balance. This is the scale lever `CONCURRENCY` was documented as but never was — that knob only ever parallelized registry deployment listing, and its docs now say so. Two consequences of concurrency are handled explicitly: the inventory reservation now happens *before* the paid oracle push (a position losing an inventory race skips before spending gas — `LiquidationExecutor::execute_liquidation` no longer reserves; its caller does), and the notifier's in-flight cap scales with the knob (≈2 notifications per in-flight position, floored at the previous cap of 10) so busy concurrent rounds don't silently drop alerts.
- `LiquidationStrategy::min_profit_margin_bps()` — strategies now report the margin they gate on, and `should_liquidate` became a provided trait method implemented in terms of it (both built-ins previously carried byte-identical copies). [**breaking**] for out-of-tree strategies: implementing `min_profit_margin_bps` is now required; `should_liquidate` may be dropped unless the fork's go/no-go policy genuinely differs.

### Changed

- `RpcError::TimeoutError` carries the underlying error's rendering instead of two duration numbers that were always a `(0, 0)` sentinel — no layer that produces timeouts surfaces real durations, so none are invented. Timeout log/notification messages now show the actual underlying error.

- [**breaking**] Swap errors are typed end-to-end: `SwapProvider::swap` (and `swap_with_retry`) now return `Result<(), SwapError>` instead of a rendered-string `AppError`. The 1-Click provider classifies every failure at its source, and the executor's JIT path and the service's batch path match on `SwapError::kind` directly — deleting two copy-pasted closures that re-classified errors by substring-matching display strings (they disagreed with each other — one matched `"Amount too low"` where the rendered form was `"Amount is too low"` — and their `Unknown` default made every error non-retryable, see the Fixed entry below). Retryability is now type-driven. Out-of-tree `SwapProvider` implementations must return `SwapError` and follow its classification contract, documented on the trait: any failure after funds may have moved must be `Indeterminate`, never a retryable kind.

- Market contract versions are read and parsed once, during registry refresh. Previously each market's NEP-330 version was fetched up to three times through three separate parser/policy combinations that disagreed with each other: the registry gate skipped markets missing metadata, a second compatibility check *assumed compatible* on the same missing metadata, and the sizing path treated an unparseable version as partial-capable. Now one read feeds one parser (`scanner::parse_semver`, strict `major.minor.patch`) and one policy — missing or unparseable metadata skips the market at registration — and the parsed version is handed to the liquidator at construction (`Liquidator::new` takes a `market_version` parameter; `fetch_market_version`, `get_market_version`, `check_market_compatibility`, and `test_market_compatibility` are gone). [**breaking**] for forks calling those removed methods.

- Position scans screen locally before any per-position RPC: each position's status is computed with the contract's own `MarketConfiguration::borrow_status` from data already fetched (positions, oracle prices, market config), and only apparent liquidation candidates get the authoritative on-chain check. Per-round RPC drops from one read per position to one per candidate — on a healthy market, roughly the number of markets rather than the number of positions. Behavior is unchanged for candidates (the contract still decides), and if the local price pair can't be built the round falls back to confirming every position on-chain.

- Markets whose configured asset decimals fall outside `[0, 38]` are now rejected at registration (they arrive from on-chain state unvalidated and would corrupt every `10^d` conversion as zero/infinity). A previously-scanned market with such a config **silently drops out of scope**: the only trace is a "Market filtered out" log line with reason `asset decimals out of sane range [0, 38]`, and a `--run-mode once` deployment whose *only* market is rejected now exits non-zero with `NoMarkets` where it previously ran. No real token uses more than 24 decimals, so a legitimately configured market cannot hit this.

### Fixed

- *(swap)* The 1-Click supported-token cache now **fails closed**: an empty cache (the `/v0/tokens` fetch failed) declines every pair instead of allowing them all, honoring `supports_assets`'s documented prefer-false-negatives contract — a wrongly-allowed pair only fails *after* the liquidation has landed and the collateral is already held. The cache reloads on every registry refresh, so one failed fetch no longer needs a restart to notice, and never silently green-lights unroutable pairs in the meantime.
- *(swap)* A failed implicit deposit-account creation now stops the swap before the token deposit instead of logging a warning and proceeding toward a guaranteed refund. (The creation transfer itself stays at 1 yoctoNEAR — NEP-448 zero-balance accounts waive storage staking for fresh implicit accounts, so the minimum suffices and anything more would be an unrecovered per-swap cost.)
- *(swap)* Retry backoff arithmetic saturates instead of overflowing: the doubling shift is undefined at 64 attempts and the delay multiplication could wrap — either would panic mid-retry under a large configured `SWAP_RETRY_ATTEMPTS`.
- *(service)* Rate-limit detection matches "429" and "TooManyRequests" as whole tokens instead of substrings, so a hex feed id or account hash containing the digits `429` no longer puts a market to sleep for 60 seconds.
- *(service)* USDC recognition (the batch-swap target preference) anchors on whole asset-id segments against an explicit list of known USDC variants — a token whose name merely contains "usdc" is no longer treated as USDC.
- *(service)* A missing swap provider now aborts the batch-swap pass up front instead of returning mid-loop with part of the bookkeeping done. This also stops the dry-run preview from narrating batch swaps that structurally cannot happen (the provider is absent exactly when the collateral strategy is `Hold`).
- *(format)* Stellar token recognition anchors its id fragments at the end of the asset id instead of matching anywhere inside it, so a different token embedding the fragment doesn't alias to the known ticker.

- *(swap)* Swap retry now actually retries transient failures — and can never double-spend a deposit. Previously the two swap call sites re-classified the provider's errors from rendered strings, defaulting everything unrecognized to non-retryable `Unknown`; since every provider error reached them as a flattened string, **no swap error was ever retried**, including genuinely transient quote/network failures — the configured retry/backoff knobs were effectively dead. Fixing that classification naively would have armed a double-spend: the provider marked its post-deposit failures (status polling timing out) retryable, and the retry wrapper re-runs the *entire* swap — new quote, new deposit address, a second deposit of the same funds, while the first may still settle. This release fixes both halves together: pre-deposit transient failures (quote request, network) retry with backoff for the first time, and every failure at or after the deposit transfer — deposit not reaching finality, deposit-notify failing, status polling timing out — classifies as the new `SwapErrorKind::Indeterminate`, which is never auto-retried and names the deposit address. Reconciliation is by construction: the next inventory refresh re-reads balances from chain, so a late settlement or refund is reflected before anything sizes a new swap.

- *(strategy)* Partial-liquidation support is now gated on the minimum version that ships it (`>= 1.1.0`) instead of **equality with `1.0.0`**. The old check routed every version that wasn't exactly `1.0.0` — including `1.0.x` patch releases and markets with unknown versions — down the partial-liquidation path, submitting requests those markets reject on-chain. Unknown versions now conservatively require full liquidation.
- *(strategy)* The fixed-amount strategy no longer submits an underfunded offer when a market requires full liquidation but the configured budget doesn't cover the position's debt (plus safety buffer). It previously capped the repay amount at the budget anyway — an offer the market contract rejects, wasting the oracle-push gas on a transaction that could never succeed. It now skips the position with a warning.

- *(profitability)* Oracle prices are validated before use as divisors: a zero, negative, or non-finite price is now an error instead of dividing to infinity and saturating into a plausible-looking `Ok(u128::MAX)` collateral value that approved losing liquidations. Both conversion functions also reject results the `f64 → u128` cast would silently saturate.
- *(oracle)* The direct-Pyth path now bounds Hermes prices by the market's freshness window, like every other pricing path, and a market whose feed pair is incomplete or stale now **fails its scan** (naming the missing feeds) rather than pricing positions off stale data or reading as a clean round — so a stale-oracle outage surfaces through the market-failure counters, consecutive-failure alerts, and `/healthz` instead of silently pausing liquidation.
- *(oracle)* A transformer read failure now surfaces as an error instead of an empty "no prices" response that hid the failure from the caller.

- *(strategy)* The profitability gate's minimum-revenue threshold now rounds **up** (`div_ceil`) instead of down: a trade whose expected revenue lands exactly on the fractional requirement — e.g. 1105 against a required 1105.5 at 50 bps — is now rejected where it was previously accepted (a one-raw-unit-stricter gate). An overflowing bps multiplication now fails closed (not profitable) instead of wrapping in release builds or panicking in debug.

- *(inventory)* A successful liquidation now **debits** the tracked balance (`InventoryManager::consume`) instead of merely releasing the reservation. Previously the spent amount went straight back into "available" until the next RPC refresh, so every later position in the same round sized against tokens that had already left the account and submitted transactions doomed to fail on-chain — one spurious failure (plus a Telegram alert) per remaining eligible position after each success.
- *(liquidator)* Failed oracle conversions now skip the position instead of proceeding on wrong-unit fallbacks. The expected-collateral-value fallback was the raw collateral amount (collateral units fed into a borrow-unit comparison); the gas fallback was a constant blind to the borrow asset's decimals.
- *(executor)* The JIT-swap USD threshold check scales by the market's actual borrow-asset decimals instead of a hardcoded `10^6` — previously off by `10^12` for an 18-decimal borrow asset, making `MIN_SWAP_VALUE_USD` meaningless there.
- *(liquidator)* The "not profitable, skipping" log line now shows `min_revenue_required` at the strategy's actual configured margin; it previously hardcoded the 50-bps default and lied whenever `MIN_PROFIT_BPS` was set to anything else.

## [0.2.0] - 2026-08-20

### Added

- Standalone repo: migrated out of the `Templar-Protocol/contracts` monorepo into `Templar-Protocol/templar-liquidator`, published under GPL-3.0-only as a public reference implementation.
- `RUN_MODE=once` (`--run-mode once` / `--once`) — a single registry refresh and liquidation round, then exit non-zero on failure (including a registry that yields zero markets). Built for cron-style schedulers; the new GCP Terraform module runs the bot this way.
- Optional `GET /healthz` (readiness) and `GET /metrics` (Prometheus text format, seven `templar_liquidator_*` series) HTTP surface, enabled by setting `HTTP_PORT`. Disabled by default and never started in `RUN_MODE=once`.
- Generic, variable-driven Terraform module under `terraform/` (Cloud Run Job + Cloud Scheduler + Secret Manager + an Artifact Registry GHCR mirror), with a complete worked example under `terraform/examples/basic`.
- CI (`fmt`, `clippy`, unit tests, `cargo doc`, `cargo-deny`, Docker build, `terraform validate`) and tagged releases publishing `ghcr.io/templar-protocol/templar-liquidator:<tag>`.

### Changed

- *(liquidator)* [**breaking**] `DRY_RUN` now defaults to `true`. Every previous deployment ran live by default; anyone relying on that must now set `DRY_RUN=false` explicitly. The env var also tightened to accept only the literal strings `true`/`false` — any other value aborts startup instead of silently falling back to either mode.

### Security

- Cleared both advisories that `deny.toml` previously had to ignore, by moving off `reqwest` 0.11:
  - **RUSTSEC-2026-0258** — `h2` 0.3.27, unbounded empty DATA frames. Reached only through `reqwest` 0.11.27 → `hyper` 0.14.
  - **RUSTSEC-2025-0134** — `rustls-pemfile` 1.0.4, unmaintained. This one compiled into the release binary.

  The affected *versions* are gone from `Cargo.lock`: `h2` 0.3.x no longer appears (only the patched 0.4.16, reached through `hyper` 1.x), and `rustls-pemfile` is absent entirely. The ignore entries were removed rather than left to rot as `advisory-not-detected` warnings.

### Dependencies

Versions below are this crate's **direct** dependencies. Older copies of all three remain in the graph transitively (see the note after the list), so audit against `cargo tree --package templar-liquidator --depth 1` rather than against `Cargo.lock` alone.

- `reqwest` 0.11.27 → **0.13.4**, switched to `default-features = false` with an explicit feature set (`json`, `query`, `native-tls`, `charset`, `http2`, `system-proxy`). Note `query` is now a feature gate rather than always-on, and `RequestBuilder::query` — used for the Pyth Hermes call in `oracle.rs` — is unavailable without it.
- `near-crypto` 0.34.7 → **0.37.3**
- `near-jsonrpc-client` 0.20.0 → **0.22.0**

  `Cargo.lock` still carries `reqwest` 0.12.28, `near-crypto` 0.34.7 and `near-jsonrpc-client` 0.20.0 alongside the versions above. None of that is leftovers this crate can drop:

  - `reqwest` 0.12.28 is required by `near-jsonrpc-client` **0.22.0** — the very version bumped to here — as well as by `near-api` and `near-openapi-client`, so that copy stays regardless of what this crate pins directly.
  - `near-jsonrpc-client` 0.20.0 and `near-crypto` 0.34.7 arrive through `templar-common` at the pinned contracts rev, so they move only when that rev moves (see THE SINGLE-REV RULE in `CLAUDE.md`), never through a Dependabot bump here.

  `deny.toml` sets `multiple-versions = "allow"` for exactly this reason.

  Net effect on the dependency graph: 894 → 884 packages.

## [0.1.4](https://github.com/Templar-Protocol/contracts/compare/templar-liquidator-v0.1.3...templar-liquidator-v0.1.4) - 2026-08-07

### Added

- *(gateway)* [**breaking**] oracle.updatePyth fetches its own payload (ENG-462) ([#586](https://github.com/Templar-Protocol/contracts/pull/586))

## [0.1.1](https://github.com/Templar-Protocol/contracts/compare/templar-liquidator-v0.1.0...templar-liquidator-v0.1.1) - 2026-08-03

### Added

- *(release)* automate per-crate releases and version contract artifacts (ENG-522) ([#528](https://github.com/Templar-Protocol/contracts/pull/528))
