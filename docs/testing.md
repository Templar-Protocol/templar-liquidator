# Testing

## Tiers

| Tier | Command | Runs | Needs |
|---|---|---|---|
| Unit | `cargo test --lib --bins` | every PR (`.github/workflows/ci.yml`) | nothing beyond the toolchain |
| Sandbox integration | `cargo test --test liquidation_sandbox -- --ignored` | nightly + manual (`.github/workflows/sandbox.yml`) | a contracts monorepo checkout, `cargo-near`, network |

`tests/liquidation_sandbox.rs` drives the liquidator's own
[`LiquidationExecutor`] against a market deployed on a live `neard` sandbox
node and asserts it lands a real liquidation. It is `#[ignore]`d, so a plain
`cargo test` (or `cargo test --lib --bins`, the PR gate) never attempts it.

## Why it can't just run standalone

`templar-gateway-testing` (a dev-dependency pulled in from the contracts
monorepo at the pinned rev — see `Cargo.toml`'s `THE SINGLE-REV RULE`) locates
contract wasms through `gateway/testing/src/wasm.rs`:

```rust
fn workspace_root() -> &'static Path {
    Path::new(env!("CARGO_WORKSPACE_DIR"))
}
```

`env!` resolves at **compile time** of that crate — it bakes a path into the
compiled binary. From there, `wasm.rs::get()` either reads a prebuilt artifact
from `<workspace_root>/target/near/...` (when `TEST_CONTRACTS_PREBUILT` is
set) or runs `cargo near build ... --manifest-path <workspace_root>/.../Cargo.toml`
to build one fresh. Either way `workspace_root` has to be a real checkout of
the `Templar-Protocol/contracts` monorepo containing packages like
`templar-market-contract`, `mock-ft`, `mock-mt`, `mock-oracle` — a plain
`templar-liquidator` checkout has none of those.

This repo's own `.cargo/config.toml` sets the same variable purely so the
`env!()` call compiles at all:

```toml
[env]
CARGO_WORKSPACE_DIR = { value = "", relative = true }
```

`relative = true` with an empty `value` resolves to *this* repo's own root
(confirmed empirically — see Verification below — it expands to the
config-file's directory, trailing slash included). That's a harmless
placeholder, not a real contracts checkout, so `cargo near build` invoked
against it fails to find `templar-market-contract` and the harness panics.
The fix isn't in this repo's gate — it's supplying a real contracts checkout
at test time, which is what the procedure below and `sandbox.yml` do.

Cargo's `[env]` table does **not** override a variable already present in the
process environment unless the entry sets `force = true` (see the [Cargo
reference on `[env]`](https://doc.rust-lang.org/cargo/reference/config.html#env)).
This repo's entry does not set `force`, so exporting a real
`CARGO_WORKSPACE_DIR` before `cargo build`/`cargo test` wins over the
placeholder. That's the entire mechanism the procedure below relies on — no
change to `.cargo/config.toml` is needed or wanted (a different concern owns
that file).

## Running it locally

```bash
# 1. Check out the contracts monorepo at the exact pinned rev (must match
#    Cargo.toml's `rev =` for every templar-* git dependency in this repo).
git clone https://github.com/Templar-Protocol/contracts /tmp/contracts
git -C /tmp/contracts checkout 8f8fe1d1a057756f71438abc75d7b3c688b282f0

# 2. Point the compile-time env var at that checkout, then build+run the
#    ignored test. -j 1 avoids the linker OOM noted below.
export CARGO_WORKSPACE_DIR=/tmp/contracts/
cargo test --test liquidation_sandbox -j 1 -- --ignored --nocapture
```

That's the whole procedure — no `just`, no Postgres, no manually-managed
`neard`. Two things make it that simple:

- **Postgres isn't needed.** The monorepo's `just test-sandbox` starts
  Postgres because *other* packages in that nextest run need it (relayer's
  gateway-store tests). `liquidation_sandbox.rs` itself never touches
  Postgres — it only calls `SandboxHarness::start()`.
- **No manual node pool.** `SandboxHarness::start()` runs in **owned mode**
  when `NEAR_SANDBOX_RPC_URL` is unset: it launches its own dedicated `neard`
  via the `near-sandbox` crate (which fetches a matching `neard` binary on
  first use) and tears it down when the harness drops. The monorepo's
  `script/sandbox-up.sh` pool of out-of-band nodes exists to amortize node
  startup across *many* concurrent sandbox tests; for this one test, owned
  mode is simpler and is exactly what running a lone `#[tokio::test]` gets
  you for free.

Prerequisites the procedure above assumes are on `PATH`:

- **`cargo-near`** (pinned to `0.19.2` in the contracts repo's own CI —
  `.github/actions/cargo-near/action.yml` at the pinned rev). Needed because
  the default (non-`TEST_CONTRACTS_PREBUILT`) path builds each contract wasm
  via `cargo near build ... non-reproducible-wasm --no-abi`.
- **`wasm32-unknown-unknown`** target for whatever toolchain builds the
  contracts. In practice you don't need to add this yourself: `cargo near
  build` runs with its working directory set to the contracts checkout, so
  rustup resolves *that* directory's own `rust-toolchain.toml` (channel
  `1.86.0`, `targets = ["wasm32-unknown-unknown"]` at the pinned rev) and
  auto-installs both the toolchain and the target on first use — separately
  from whatever toolchain builds `templar-liquidator` itself (this repo pins
  `1.97.0`). This needs network access the first time.
- Network access in general: the first run downloads crates for the contract
  packages, the `neard` sandbox binary, and (if not already installed)
  `cargo-near` and the `1.86.0` toolchain + its wasm target.

### What this test does *not* need

`gateway/testing/src/wasm.rs` also exposes `wasm::released()`, which
downloads a *past* release's exact wasm bytes (verified against a SHA-256
pin) for migration/upgrade tests — that's what `just artifacts-fetch` warms.
`liquidation_sandbox.rs`'s `harness.deploy_market()` never calls
`wasm::released()` — it only uses `wasm::market()`, `wasm::ft()`,
`wasm::mt()`, `wasm::mock_oracle()`, all of which build from source (or read
`target/near` under `TEST_CONTRACTS_PREBUILT`). So warming the
released-artifact cache is irrelevant to this specific test, even though the
contracts monorepo's CI always does it (other tests in that same nextest
invocation need it).

`TEST_CONTRACTS_PREBUILT=1` (with a `just prebuild-test-contracts` /
`just sandbox-up` step first) is also not needed here — that optimization
exists so many tests sharing one process don't each redundantly rebuild the
same wasm; a single test builds each of the four artifacts it needs exactly
once regardless (each is cached in-process behind a `OnceCell`), so the
on-demand build path is already minimal for this case.

### `-j 1`: why

Linking `liquidation_sandbox`'s test binary pulls in `near-sandbox`
(wasmtime + cranelift) plus the full gateway/oracle/swap dependency graph.
Parallel linking of this binary intermittently OOMs on memory-constrained
machines (`ld terminated with signal 9 [Killed]`, not a compile error — seen
repeatedly on the dev machine used to write this doc, an 8 GB container with
other processes competing for RAM). `-j 1` avoids competing with other
cargo/rustc jobs for the link step's memory; it does not by itself guarantee
enough headroom on a very constrained host, but it's the cheapest mitigation.
Retry a couple of times if it still happens — it's memory pressure, not a
correctness issue.

## What was verified vs. reasoned out

This procedure was assembled by reading the pinned contracts monorepo's
source at the exact rev this repo depends on (`gateway/testing/src/wasm.rs`,
`gateway/testing/src/sandbox.rs`, `contract/artifacts/src/workspace_loader.rs`,
`contract/artifacts/src/prebuild.rs`, `justfile`, `script/sandbox-up.sh`,
`script/prebuild-test-contracts.sh`, `.github/workflows/test.yml`,
`.github/actions/cargo-near/action.yml`), not by running the sandbox test to
completion — the machine used to write this doc is memory-constrained (a
single `cargo test --no-run -j 1` of just the *type-checking + linking* step
OOM'd three times running the test binary's final link) and lacks
`cargo-near`, so a full build-and-run wasn't attempted here.

Verified by actually running something:

- `.cargo/config.toml` in this repo does not set `force = true` on the `[env]`
  entry (read directly).
- The env-override mechanism itself: a throwaway crate outside this repo,
  replicating this repo's exact `[env]` stanza, printed `env!("CARGO_WORKSPACE_DIR")`
  at runtime. Without an override it printed this repo's own root (with a
  trailing slash, confirming `relative = true` + empty `value` resolves that
  way). With `CARGO_WORKSPACE_DIR=/tmp/contracts-checkout-marker cargo run`,
  it printed exactly that path — proving a real environment variable wins
  over the config placeholder when `force` is unset, on the toolchain
  available in that environment (1.86.0).
- `cargo check --tests -j 1` and `cargo clippy --all-targets -j 1 -- -D
  warnings` both pass cleanly in this repo, including the now-`#[ignore]`d
  sandbox test — so the gating change compiles and lints clean even though
  the final link of that specific test binary couldn't be completed here.
- `cargo test --lib --bins -j 1` — 132 passed, unaffected by the gating
  change (the sandbox test lives in a separate integration-test binary that
  target never builds).

Reasoned from source, not executed:

- That `SandboxHarness::start()` defaults to owned mode (launches its own
  `neard`) when `NEAR_SANDBOX_RPC_URL` is unset, and that this test never
  reaches `wasm::released()` or Postgres.
- That `cargo near build`'s working directory makes rustup pick up the
  contracts checkout's own pinned toolchain/target automatically.
- The `cargo-near` version pin and the CI-level `wasm32-unknown-unknown`
  target requirement (read from the contracts repo's own workflow files, not
  reproduced by actually installing `cargo-near` — this container is aarch64
  and the pinned release tarball is `x86_64-unknown-linux-gnu` only).

If you run this procedure locally and it diverges from what's written here,
trust the code over this doc and update it.
