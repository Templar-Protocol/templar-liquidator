# Contributing

Thanks for considering a contribution to `templar-liquidator`. This is a public reference implementation — the expected way most people will use this repo is forking it and configuring/extending it for their own deployment (see the README's [extension seams](README.md#how-it-works)), but improvements to the shared bot are welcome upstream too.

## Toolchain

The Rust toolchain is pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (`rustup` picks it up automatically once you `cd` into the repo, including the `rustfmt` and `clippy` components).

## Before opening a PR

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test --lib --bins
```

These are exactly what CI runs on every PR ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)), plus `cargo doc --no-deps` (warnings-as-errors), [`cargo-deny`](deny.toml) (license/advisory checks), a Docker build, and `terraform fmt -check` / `terraform validate` for changes under `terraform/`. Run the ones relevant to what you touched before pushing — it's faster than waiting for CI to tell you.

A couple of repo-specific lint notes:

- `clippy::pedantic` is warn-level with `unwrap_used`/`expect_used` denied outside tests (see `Cargo.toml`'s `[lints.clippy]` and [`clippy.toml`](clippy.toml)'s `allow-unwrap-in-tests` / `allow-expect-in-tests`). Don't silence a real lint with an inline `#[allow]` where narrowing the code is the actual fix.
- Every `templar-*` git dependency in `Cargo.toml` must stay pinned to the **same** `rev`. If you bump one, bump all of them together — see [`CLAUDE.md`](CLAUDE.md#the-single-rev-rule) for why a mismatched pin produces confusing "expected X, found X" type errors instead of an obvious version conflict.

## Tests

`cargo test --lib --bins` covers the unit test suite (what CI runs on every PR). There's also a node-backed sandbox integration test, `tests/liquidation_sandbox.rs`, which spins up a real `neard` sandbox and prebuilt contract wasms — it's `#[ignore]`d by default and runs on a separate nightly/manual CI schedule rather than on every PR, because it's slow and needs more than `cargo test` alone provides. See [docs/testing.md](docs/testing.md) for how to run it locally.

## Docs and code generation

There's no OpenAPI/codegen step in this repo (that's a backend-monorepo pattern, not this one) — if you change behavior that's documented, update the relevant page under `docs/` or the README in the same PR rather than letting docs drift.

## Submitting a PR

- PRs target `main`.
- Keep the change focused; unrelated formatting/refactor churn makes review harder.
- If your change affects a documented env var, CLI flag, or default, update [docs/configuration.md](docs/configuration.md) (and `.env.example` if it's new) in the same PR.

## Releases

Releases are tags (`vX.Y.Z`). Pushing one triggers [`.github/workflows/release.yml`](.github/workflows/release.yml), which builds and pushes `ghcr.io/templar-protocol/templar-liquidator:<tag>` (plus `:<major>.<minor>` and `:latest`) and cuts a GitHub Release. See [CHANGELOG.md](CHANGELOG.md) for the format each release entry should follow.

## License

By contributing, you agree your contribution is licensed under this repo's [GPL-3.0-only license](LICENSE).
