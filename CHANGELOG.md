# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-08-18

### Added

- Standalone repo: migrated out of the `Templar-Protocol/contracts` monorepo into `Templar-Protocol/templar-liquidator`, published under GPL-3.0-only as a public reference implementation.
- `RUN_MODE=once` (`--run-mode once` / `--once`) — a single registry refresh and liquidation round, then exit non-zero on failure (including a registry that yields zero markets). Built for cron-style schedulers; the new GCP Terraform module runs the bot this way.
- Optional `GET /healthz` (readiness) and `GET /metrics` (Prometheus text format, seven `templar_liquidator_*` series) HTTP surface, enabled by setting `HTTP_PORT`. Disabled by default and never started in `RUN_MODE=once`.
- Generic, variable-driven Terraform module under `terraform/` (Cloud Run Job + Cloud Scheduler + Secret Manager + an Artifact Registry GHCR mirror), with a complete worked example under `terraform/examples/basic`.
- CI (`fmt`, `clippy`, unit tests, `cargo doc`, `cargo-deny`, Docker build, `terraform validate`) and tagged releases publishing `ghcr.io/templar-protocol/templar-liquidator:<tag>`.

### Changed

- *(liquidator)* [**breaking**] `DRY_RUN` now defaults to `true`. Every previous deployment ran live by default; anyone relying on that must now set `DRY_RUN=false` explicitly. The env var also tightened to accept only the literal strings `true`/`false` — any other value aborts startup instead of silently falling back to either mode.

## [0.1.4](https://github.com/Templar-Protocol/contracts/compare/templar-liquidator-v0.1.3...templar-liquidator-v0.1.4) - 2026-08-07

### Added

- *(gateway)* [**breaking**] oracle.updatePyth fetches its own payload (ENG-462) ([#586](https://github.com/Templar-Protocol/contracts/pull/586))

## [0.1.1](https://github.com/Templar-Protocol/contracts/compare/templar-liquidator-v0.1.0...templar-liquidator-v0.1.1) - 2026-08-03

### Added

- *(release)* automate per-crate releases and version contract artifacts (ENG-522) ([#528](https://github.com/Templar-Protocol/contracts/pull/528))
