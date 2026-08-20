# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `POSITION_CONCURRENCY` (`--position-concurrency`, default `1`) — how many positions one market's round evaluates/liquidates in flight at once. The default keeps the historical behavior exactly: sequential, with a 1-second pause between positions. Raising it drops the pause and fans evaluation out through a bounded pool; inventory reservations already serialize the capital commitment, so concurrent positions cannot double-spend the same balance. This is the scale lever `CONCURRENCY` was documented as but never was — that knob only ever parallelized registry deployment listing, and its docs now say so. Two consequences of concurrency are handled explicitly: the inventory reservation now happens *before* the paid oracle push (a position losing an inventory race skips before spending gas — `LiquidationExecutor::execute_liquidation` no longer reserves; its caller does), and the notifier's in-flight cap scales with the knob (≈2 notifications per in-flight position, floored at the previous cap of 10) so busy concurrent rounds don't silently drop alerts.
- `LiquidationStrategy::min_profit_margin_bps()` — strategies now report the margin they gate on, and `should_liquidate` became a provided trait method implemented in terms of it (both built-ins previously carried byte-identical copies). [**breaking**] for out-of-tree strategies: implementing `min_profit_margin_bps` is now required; `should_liquidate` may be dropped unless the fork's go/no-go policy genuinely differs.

### Changed

- Markets whose configured asset decimals fall outside `[0, 38]` are now rejected at registration (they arrive from on-chain state unvalidated and would corrupt every `10^d` conversion as zero/infinity). A previously-scanned market with such a config **silently drops out of scope**: the only trace is a "Market filtered out" log line with reason `asset decimals out of sane range [0, 38]`, and a `--run-mode once` deployment whose *only* market is rejected now exits non-zero with `NoMarkets` where it previously ran. No real token uses more than 24 decimals, so a legitimately configured market cannot hit this.

### Fixed

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
