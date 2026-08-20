# Contributing

Thanks for considering a contribution to `templar-liquidator`. This is a public reference implementation — the expected way most people will use this repo is forking it and configuring/extending it for their own deployment (see the README's [extension seams](README.md#how-it-works)).

## Who can open pull requests and issues

Pull requests and issues are currently limited to members of the Templar
Protocol organization. **Forking is unaffected** — the repo exists to be forked,
run, and modified for your own deployment, and nothing here restricts that or
the GPL-3.0 rights you have over your fork.

If you have found a bug, a security issue, or want a change upstreamed, that is
genuinely wanted — reach the maintainers through the contact in
[`SECURITY.md`](SECURITY.md) rather than the issue tracker, and a maintainer
will open the issue or shepherd the change. The rest of this guide applies to
anyone working in the repo, org member or fork owner.

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

To cut one:

```bash
# 1. Bump the version and refresh the lock
#    (edit Cargo.toml's `version`, then:)
cargo update -p templar-liquidator

# 2. Move CHANGELOG.md's [Unreleased] content into a `## [X.Y.Z] - YYYY-MM-DD` section

# 3. Check the working tree agrees before going further
./scripts/check-release.sh vX.Y.Z

# 4. COMMIT the bump and land it on main through a PR — the tag must point at a
#    merged commit, not at an uncommitted working tree
git switch -c release/vX.Y.Z && git commit -am "chore(release): vX.Y.Z"
#    ...open the PR, get it green, merge it...

# 5. Tag the MERGED commit and push
git switch main && git pull --ff-only
./scripts/check-release.sh vX.Y.Z   # re-check: this is now the tree CI will see
git tag vX.Y.Z && git push origin vX.Y.Z
```

Step 4 is not optional bookkeeping. `check-release.sh` reads the **working tree**, while the `preflight` job reads the **tagged commit** — so bumping the files without committing them lets step 3 pass locally and `preflight` fail afterwards, once `vX.Y.Z` is already on origin and has to be deleted remotely before you can retry. Tagging a merged commit also keeps the release reachable from `main`; `git push origin vX.Y.Z` pushes the tag *plus its objects*, so an unmerged commit would publish fine and then be unreachable from any branch.

The `preflight` job runs `check-release.sh` again and blocks the release if the tag, `Cargo.toml`, `Cargo.lock` and `CHANGELOG.md` disagree — each of those mismatches otherwise fails silently, producing an image labelled with a version its binary does not report.

`:latest` only moves for non-prerelease tags, so `vX.Y.Z-rc.1` publishes `X.Y.Z-rc.1` without repointing `:latest` (or `:<major>.<minor>`) at a release candidate.

Note that a GHCR package is **private by default even for a public repo** — after the first release, make the package public once, or the `docker pull` in the README will fail for everyone else.

## License

By contributing, you agree your contribution is licensed under this repo's [GPL-3.0-only license](LICENSE).
