# templar-liquidator

[![CI](https://github.com/Templar-Protocol/templar-liquidator/actions/workflows/ci.yml/badge.svg)](https://github.com/Templar-Protocol/templar-liquidator/actions/workflows/ci.yml)
[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![GHCR](https://img.shields.io/badge/ghcr.io-templar--liquidator-blue?logo=docker)](https://github.com/Templar-Protocol/templar-liquidator/pkgs/container/templar-liquidator)

An inventory-based liquidation bot for [Templar Protocol](https://templarfi.org) lending markets on [NEAR](https://near.org). It holds a pool of borrow-asset tokens (e.g. USDC), repays the debt of underwater positions directly from that inventory, and receives their collateral at a discount in return. Received collateral can optionally be swapped back into borrow assets so the same inventory is available for the next round.

## Quickstart

Pull the released image and run it with your own `.env` — **note the access caveat below; for most readers the Compose path is the one that works**:

```bash
cp .env.example .env
nano .env  # fill in the three required vars below
docker login ghcr.io       # see the note below — the package is not yet public
docker run --env-file .env ghcr.io/templar-protocol/templar-liquidator:latest
```

> **The published image is not publicly pullable yet**, and `docker login`
> alone is not enough to fix that. GHCR packages are private by default, this
> organization currently restricts making them public, and a private package
> is readable only by accounts explicitly granted access to it — so:
>
> - **If you have been granted access:** log in with a GitHub personal access
>   token (classic) carrying `read:packages`, supplying the token as the Docker
>   password. If the organization enforces SSO, authorize the token for it
>   first, or the pull still fails.
> - **If you have not** — which is everyone outside the organization — this
>   path cannot work at all. Use the Compose path below; it builds from source,
>   needs no registry access, and is the supported route.
>
> Tracked in [#24](https://github.com/Templar-Protocol/templar-liquidator/issues/24);
> this note goes away once the package is public.

Or build and run locally with Docker Compose:

```bash
cp .env.example .env
nano .env
docker compose up
```

`docker compose up` **builds from source** — it compiles the crate and its
git dependencies inside the image, including an `npm install` a dependency's
`build.rs` runs — so the first run is slow (several minutes). It is also the
**only path that works without registry access**, so unless you have been
granted access to the private package it is the way to get started, not just
the way to iterate on the code.

Three env vars are required — the bot refuses to start without them:

| Var | What it is |
|---|---|
| `REGISTRY_ACCOUNT_IDS` | Templar market registry contract(s) to discover markets from, e.g. `v1.tmplr.near` |
| `SIGNER_ACCOUNT_ID` | The NEAR account the bot signs transactions as — also where your inventory lives |
| `SIGNER_KEY` | That account's private key, e.g. `ed25519:...` |

**The bot starts in DRY-RUN and sends nothing until you explicitly set `DRY_RUN=false`.** A fresh checkout scans markets and logs what it *would* liquidate — no transaction is ever submitted — until you opt in to live trading. See [Configuration](#configuration) below.

## What is a liquidator?

Templar's lending markets let users borrow assets against posted collateral. If the collateral's value falls (or the debt grows via interest) enough that the position's collateralization ratio drops below the market's liquidation threshold, the position becomes **liquidatable**: anyone can repay part of its debt and receive the equivalent collateral *plus a discount* in return. That discount — the **liquidation spread** — is the liquidator's compensation for keeping the protocol solvent; without liquidators, under-collateralized debt would sit unpaid and lenders would take the loss.

This bot automates that: it scans every market in a registry, sizes a profitable repayment against whatever inventory it holds, and submits the liquidation transaction.

### A worked example, with real numbers

The numbers below are **live mainnet data**, fetched via NEAR RPC and Pyth Hermes on 2026-08-18. The position itself is **illustrative** — constructed to be underwater — but every market parameter and price is real.

Market: [`ibtc-usdc.v1.tmplr.near`](https://nearblocks.io/address/ibtc-usdc.v1.tmplr.near) (BTC collateral, USDC borrow), one of the markets returned by `v1.tmplr.near`'s `list_deployments`. Its `get_configuration` view returns:

| Field | Value |
|---|---|
| `borrow_mcr_maintenance` | `1.25` (125%) |
| `borrow_mcr_liquidation` | `1.2`* (120%) |
| `liquidation_maximum_spread` | `0.05`* (5%) |

\* Stored on-chain as `1.199999999999999999999999999999999999999` / `0.05000000000000000000000000000000000001` — a fixed-point rounding artifact from how the value was set, functionally 120% / 5%.

Pyth spot prices at fetch time: BTC ≈ **$64,350**, USDC ≈ **$1.00**.

Say a borrower has deposited **0.5 BTC** ($32,175) as collateral against **$28,000 USDC** of debt:

```text
collateralization ratio = 32,175 / 28,000 = 1.149  (114.9%)
```

That's below `borrow_mcr_liquidation` (120%), so the position is liquidatable. The bot decides to repay **D = $10,000 USDC** of the debt (a partial liquidation — see [`PercentageLiquidationStrategy`](src/liquidation_strategy.rs)). The collateral it requests is sized so its *fair* USD value equals `D / (1 - spread)`:

```text
collateral received = (D / price) / (1 - spread)
                     = (10,000 / 64,350) / 0.95
                     = 0.163579 BTC

fair value of that collateral = 0.163579 × 64,350 = $10,526.32
gross markup           = $10,526.32 − $10,000 = $526.32   (≈ 5.26% of D)
```

That 5.26% is `spread / (1 - spread)` — the exact fraction the [`borrow_to_collateral`/`collateral_to_borrow`](src/liquidation_strategy.rs) conversion bakes in for this market's 5% spread. Subtract gas (`ProfitabilityCalculator::DEFAULT_GAS_COST_USD` ≈ $0.05, negligible on NEAR) and the liquidation clears **≈$526/10,000 = 526 bps** of profit — comfortably above the default `MIN_PROFIT_BPS=50` (0.5%) gate. Holding the collateral (`COLLATERAL_STRATEGY=hold`) banks the full $526.27; routing it back through Ref Finance or 1-Click (`swap-to-borrow`) nets slightly less after that venue's fee/slippage.

Verify it yourself against live chain state:

```bash
set -a; source .env; set +a   # needs NEAR_API_KEY if hitting FastNEAR

curl -s https://rpc.mainnet.fastnear.com \
  -H "Authorization: Bearer $NEAR_API_KEY" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"query","params":{"request_type":"call_function","finality":"final","account_id":"v1.tmplr.near","method_name":"list_deployments","args_base64":"'"$(printf '{"offset":0,"count":10}' | base64 -w0)"'"}}'

curl -s https://rpc.mainnet.fastnear.com \
  -H "Authorization: Bearer $NEAR_API_KEY" -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"query","params":{"request_type":"call_function","finality":"final","account_id":"ibtc-usdc.v1.tmplr.near","method_name":"get_configuration","args_base64":""}}'
```

See [docs/economics.md](docs/economics.md) for strategy tuning, inventory sizing, and loop-liquidation semantics.

## How it works

Each liquidation round moves through these stages in order — every stage is owned by a single module, so a fork that only needs to change one piece of behavior can read (or replace) just that module:

1. **Registry refresh** — discover deployed markets across the configured registries and validate each one's contract version ([`src/service.rs`](src/service.rs)).
2. **Position scan** — read every borrow position in a market and check which are currently liquidatable ([`src/scanner.rs`](src/scanner.rs)).
3. **Strategy sizing** — decide how much of a liquidatable position to repay, given available inventory ([`src/liquidation_strategy.rs`](src/liquidation_strategy.rs)).
4. **Profitability gate** — reject the sizing decision unless the discounted collateral is expected to cover the repay amount plus gas, with the configured `MIN_PROFIT_BPS` margin ([`src/profitability.rs`](src/profitability.rs)).
5. **Execution** — submit the liquidation transaction and confirm every receipt in it actually succeeded, not just the top-level transaction ([`src/executor.rs`](src/executor.rs)).
6. **Collateral handling** — hold the received collateral, or swap it back to the borrow asset via [Ref Finance](src/swap/ref.rs) or [1-Click](src/swap/oneclick.rs) ([`src/executor.rs`](src/executor.rs), [`src/swap/`](src/swap/)).
7. **Notification** — report the round's outcome, or any failure along the way, to Telegram ([`src/notifier.rs`](src/notifier.rs)).

Prices for steps 2–4 come from [`src/oracle.rs`](src/oracle.rs) (Pyth Hermes, RedStone, and proxy/LST feeds); available balances for steps 3 and 5 come from [`src/inventory.rs`](src/inventory.rs).

```mermaid
flowchart LR
    Registry[("NEAR registry<br/>contract")] --> Service
    Market[("NEAR market<br/>contract")]

    subgraph Bot["templar-liquidator"]
        Service["service.rs<br/>registry refresh + round scheduling"] --> Scanner["scanner.rs<br/>position scan"]
        Oracle["oracle.rs<br/>Pyth + RedStone"] --> Scanner
        Oracle --> Strategy
        Scanner --> Strategy["liquidation_strategy.rs<br/>sizing"]
        Inventory["inventory.rs<br/>balances"] --> Strategy
        Strategy --> Profitability["profitability.rs<br/>profit gate"]
        Profitability -->|profitable| Executor["executor.rs<br/>submit tx"]
        Executor --> Inventory
        Executor -->|swap-to-borrow| Swap["swap/ref.rs<br/>swap/oneclick.rs"]
        Executor --> Notifier["notifier.rs<br/>Telegram"]
        Metrics["metrics.rs + http.rs<br/>/healthz /metrics"]
    end

    Scanner --> Market
    Executor -->|Liquidate tx| Market
    Service -.observed by.-> Metrics
```

## Configuration

Full reference (every env var / CLI flag, defaults, precedence rules): [docs/configuration.md](docs/configuration.md). The ~10 most-used:

| Env var | Default | Purpose |
|---|---|---|
| `REGISTRY_ACCOUNT_IDS` | — (required) | Market registries to scan |
| `SIGNER_ACCOUNT_ID` | — (required) | Bot's NEAR account (signs txs, holds inventory) |
| `SIGNER_KEY` | — (required) | That account's private key |
| `NEAR_NETWORK` | `testnet` | `mainnet` or `testnet` |
| `NEAR_RPC_URL` | network default | Custom RPC endpoint (recommended for mainnet — see [FAQ](#faq)) |
| `DRY_RUN` | `true` | Simulate only; set to exactly `false` to go live |
| `RUN_MODE` | `loop` | `loop` (continuous) or `once` (single cycle, for cron/Cloud Run) |
| `MIN_PROFIT_BPS` | `50` | Minimum profit margin (basis points) to attempt a liquidation |
| `PARTIAL_LIQUIDATION_PERCENTAGE` / `FIXED_LIQUIDATION_AMOUNT_USD` | 100% if neither set | Sizing strategy (mutually exclusive) |
| `COLLATERAL_STRATEGY` | `hold` | `hold` or `swap-to-borrow` |
| `HTTP_PORT` | unset (disabled) | Enables `/healthz` + `/metrics` |

## Run modes

- **`loop`** (default) — runs indefinitely: registry refresh and liquidation rounds each tick on their own interval (`REGISTRY_REFRESH_INTERVAL`, `LIQUIDATION_SCAN_INTERVAL`). This is what `docker compose up` runs.
- **`--run-mode once`** (equivalently `--once`, or `RUN_MODE=once`) — performs exactly one registry refresh and one liquidation round, then exits. Exits **non-zero** on failure, including a registry that yields zero supported markets. This is what the Terraform Cloud Run Job runs on a cron schedule (see [Deployment](#deployment)).

### Metrics and health

When `HTTP_PORT` is set, the bot serves:

- `GET /healthz` — readiness (not liveness): `200` once at least one market has scanned cleanly recently, `503` otherwise.
- `GET /metrics` — Prometheus text format, seven `templar_liquidator_*` series: scan/liquidation counters and a last-successful-scan timestamp gauge.

Neither endpoint is authenticated — anyone who can reach the port can read them. `HTTP_BIND_ADDR` defaults to `127.0.0.1`, and `docker-compose.yml`/`docker-compose.prod.yml` publish the container port to `127.0.0.1` on the host too, so out of the box this surface isn't reachable from anywhere but the host itself. It is **not** inherently private, though: exposing it to another machine (a Prometheus scraper on your network, say) is a deliberate two-part opt-in — set `HTTP_BIND_ADDR=0.0.0.0` (or the specific interface you want) *and* change the Compose port mapping's host side away from `127.0.0.1` — and once you do, put it behind your own network controls (private network, VPN, reverse-proxy auth), since the bot doesn't add any of its own. See [docs/configuration.md](docs/configuration.md#observability) for both knobs and [docs/deploy-vm.md](docs/deploy-vm.md) for why a host firewall rule alone isn't a substitute for keeping the Compose binding on loopback.

Both endpoints are inert in `RUN_MODE=once` — a single-cycle run exits before anything could scrape them.

## Deployment

- **GCP (Cloud Run Job + Scheduler)**: [docs/deploy-gcp.md](docs/deploy-gcp.md), backed by the generic Terraform module in [`terraform/`](terraform/README.md).
- **Any VM (Docker Compose)**: [docs/deploy-vm.md](docs/deploy-vm.md).

## FAQ

**Which RPC should I use?** Public endpoints rate-limit under sustained scanning — that includes both the binary's own compiled-in default (`https://rpc.mainnet.fastnear.com` / `https://rpc.testnet.fastnear.com`, used when `NEAR_RPC_URL` is unset) and `free.rpc.fastnear.com`, the endpoint `.env.example` sets explicitly. For mainnet, get a [FastNEAR](https://fastnear.com) API key and set `NEAR_RPC_URL` + `NEAR_RPC_API_KEY` — the key is sent as an `Authorization` header (or folded into the URL as `?apiKey=`).

**What inventory do I need to hold?** The **borrow assets** of every market you serve — e.g. USDC in `SIGNER_ACCOUNT_ID`'s wallet to liquidate USDC-borrow markets. The bot never buys inventory; it only spends what's already there.

**How does rebalancing work?** By default (`COLLATERAL_STRATEGY=hold`) the bot keeps whatever collateral it receives — you rebalance manually. Set `COLLATERAL_STRATEGY=swap-to-borrow` to route collateral back into borrow assets automatically, immediately after each liquidation (above `MIN_SWAP_VALUE_USD`) or batched at the start of the next cycle. Full automatic inventory rebalancing (deciding *when* and *how much* to swap proactively, not just what's received) is [tracked as backlog](docs/backlog.md).

**How do I pick a strategy?** The percentage-of-inventory strategy is the default, and it defaults to 100% — a full liquidation of every eligible position your inventory can cover — unless you set `PARTIAL_LIQUIDATION_PERCENTAGE` below 100 to spread limited inventory across more positions per round instead. `fixed-amount` (`FIXED_LIQUIDATION_AMOUNT_USD`) is the third option: a predictable USD cap per liquidation regardless of position size, USD-denominated borrow assets only. See [docs/economics.md](docs/economics.md).

## Disclaimer

This bot holds and spends funds under `SIGNER_KEY`. Unlike the rest of the Templar backend, it is **not non-custodial** — it signs and submits real transactions on your behalf, unsupervised, once you set `DRY_RUN=false`. You are solely responsible for the funds it controls, the correctness of your configuration, and any losses that result from running it. **Use at your own risk.** This code is provided as-is under the GPL-3.0-only license (see [LICENSE](LICENSE)), with no warranty of any kind.
