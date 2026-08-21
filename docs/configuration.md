# Configuration reference

Every setting the bot reads, generated against [`src/config.rs`](../src/config.rs)'s `Args` struct (the single source of truth — `.env.example` is a curated subset with commentary, this table is exhaustive).

## Precedence

**CLI flag beats env var beats default.** Every flag below also accepts its env var form; if both are set, the explicit CLI flag wins (standard `clap` `env` behavior). `docker-compose.yml`'s `command:` array sets flags from `.env` interpolation, so in the shipped Compose setups the env var is effectively what you're editing either way.

`--dry-run` is the one flag with special parsing — see below.

## Required

The bot refuses to start without these three:

| Env var | CLI flag | Description |
|---|---|---|
| `REGISTRY_ACCOUNT_IDS` | `--registries`, `-r` | Market registry account(s) to discover markets from. As a **CLI flag**, repeatable (`--registries a.near --registries b.near`). As the **env var**, this `Vec` has no `value_delimiter`, unlike the comma-delimited filters below — one registry per assignment; `REGISTRY_ACCOUNT_IDS=a.near,b.near` parses as a single invalid account id, not two registries. Multiple registries via env aren't currently expressible; see [docs/backlog.md](backlog.md). |
| `SIGNER_KEY` | `--signer-key`, `-k` | The signer account's private key, e.g. `ed25519:...`. |
| `SIGNER_ACCOUNT_ID` | `--signer-account`, `-s` | The NEAR account the bot signs transactions as. |

## Network

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `NEAR_NETWORK` | `--network`, `-n` | `testnet` | `mainnet` or `testnet`. |
| `NEAR_RPC_URL` | `--near-rpc-url` | network default (`https://rpc.mainnet.fastnear.com` / `https://rpc.testnet.fastnear.com`) | Custom RPC endpoint. |
| `NEAR_RPC_API_KEY` | `--near-rpc-api-key` | unset | API key for the RPC endpoint, sent as an `Authorization` header. May also be folded into `NEAR_RPC_URL` as an `?apiKey=` query parameter (what `scripts/run-mainnet.sh` / `scripts/run-testnet.sh` do with `NEAR_API_KEY` from the shell environment — note that shell-script variable is distinct from this one). |

## Execution

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `RUN_MODE` | `--run-mode` | `loop` | `loop` (continuous) or `once` (single registry refresh + liquidation round, then exit — for cron/Cloud Run Jobs). |
| — | `--once` | `false` | Shorthand for `--run-mode once`. No env var equivalent; forces once mode and takes precedence over `--run-mode` if both are given. |
| `LIQUIDATION_SCAN_INTERVAL` | `--liquidation-scan-interval` | `600` | Seconds between liquidation scan rounds (loop mode). |
| `REGISTRY_REFRESH_INTERVAL` | `--registry-refresh-interval` | `3600` | Seconds between registry re-discovery (loop mode). |
| `CONCURRENCY` | `--concurrency`, `-c` | `10` | Concurrency for registry deployment listing. Floored at 1 internally — `0` would stall the pipeline. |
| `POSITION_CONCURRENCY` | `--position-concurrency` | `1` | Positions evaluated/liquidated concurrently within one market's round. `1` (the default) is fully sequential with a 1-second pause between positions — what free public RPC endpoints tolerate. Raising it drops the pause and fans evaluation out; each in-flight position costs several RPC reads (and in live mode, possibly an oracle push), so bring an RPC endpoint sized for the load. Floored at 1 internally. Validate a raised value in dry-run or a staging deployment before going live: watch for RPC rate-limit errors, "Inventory no longer covers the sized amount" skips (thin inventory makes the knob buy less than it looks), and "Notification dropped" warnings. |

## Liquidation strategy

`PARTIAL_LIQUIDATION_PERCENTAGE` and `FIXED_LIQUIDATION_AMOUNT_USD` are **mutually exclusive** — setting both panics at startup with a clear error. Neither set → percentage strategy at 100% (full liquidation).

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `PARTIAL_LIQUIDATION_PERCENTAGE` | `--partial-percentage` | unset (100% if neither strategy flag is set) | Percentage (1–100) of available inventory to deploy per liquidation. |
| `FIXED_LIQUIDATION_AMOUNT_USD` | `--fixed-liquidation-amount-usd` | unset | Fixed USD amount to repay per liquidation. USD-denominated borrow assets only (no price lookup — assumes the borrow asset is a USD stablecoin). On markets requiring full liquidation (contract version < 1.1.0) it acts as an eligibility threshold, not a cap: positions whose full debt exceeds it are skipped. |
| `MIN_PROFIT_BPS` | `--min-profit-bps` | `50` | Minimum profit margin, in basis points, required to submit a liquidation. |
| `LOOP_LIQUIDATION` | `--loop-liquidation` | `false` | Repeatedly liquidate the same position (re-checking each iteration) until it's healthy or inventory runs out. Disabled in dry-run (position state never changes there, so re-checking is a no-op). |
| `MAX_LOOP_ITERATIONS` | `--max-loop-iterations` | `10` | Safety cap on loop-liquidation iterations. |

## Collateral strategy

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `COLLATERAL_STRATEGY` | `--collateral-strategy` | `hold` | `hold` (keep collateral as received) or `swap-to-borrow` (route back to the market's borrow asset). |
| `ONECLICK_API_TOKEN` | `--oneclick-api-token` | unset | 1-Click API auth token. Optional but avoids a 0.1% fee; unauthenticated calls still work. |
| `MIN_SWAP_VALUE_USD` | `--min-swap-value-usd` | `10.0` | Minimum USD value to attempt a swap (just-in-time or batch); smaller amounts are deferred to the next batch cycle. |
| `BATCH_SWAP_ON_CYCLE_START` | `--batch-swap-on-cycle-start` | `true` | Swap all accumulated collateral above the threshold at the start of each liquidation round. |
| `SWAP_RETRY_ATTEMPTS` | `--swap-retry-attempts` | `3` | Retry attempts for transient swap errors (includes the first attempt). |
| `SWAP_RETRY_BASE_DELAY_MS` | `--swap-retry-base-delay-ms` | `2000` | Base delay for swap retry exponential backoff (2s, 4s, 8s, …). |

## Market filtering

All comma-delimited (`value_delimiter = ','` in clap — repeat the flag or comma-join in one).

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `ALLOWED_COLLATERAL_ASSETS` | `--allowed-collateral-assets` | empty (all assets allowed) | Allowlist of collateral asset ids; if set, only markets with these collateral assets are processed. |
| `IGNORED_COLLATERAL_ASSETS` | `--ignored-collateral-assets` | empty | Collateral asset ids to skip. |
| `IGNORED_MARKETS` | `--ignored-markets` | empty | Market account ids to skip entirely. |

## Oracle price sources

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `PYTH_HERMES_URL` | `--hermes-url` | network default (`https://hermes.pyth.network` mainnet / `https://hermes-beta.pyth.network` testnet) | Pyth Hermes endpoint for fetching latest price data. |
| `REDSTONE_API_URL` | `--redstone-api-url` | `https://api.redstone.finance` | RedStone public price API, used to compose proxy-oracle prices off-chain at scan time (no gas, no keeper dependency). Scan-side only — execution still prices through the on-chain oracle. |

## Notifications (Telegram)

`TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` must be **both set or both empty** — one without the other panics at startup.

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `TELEGRAM_BOT_TOKEN` | `--telegram-bot-token` | `""` (disabled) | Bot token from [@BotFather](https://t.me/BotFather). |
| `TELEGRAM_CHAT_ID` | `--telegram-chat-id` | `""` (disabled) | Target chat/channel id. |
| `TELEGRAM_THREAD_ID` | `--telegram-thread-id` | `""` (unset) | Optional thread/topic id for supergroups. |
| `SCAN_FAILURE_NOTIFY_THRESHOLD` | `--scan-failure-notify-threshold` | `2` | Consecutive scan failures for a market before alerting. `0` disables scan-failure notifications. A recovery notification fires when the market next scans cleanly. |
| `FAILURE_NOTIFICATION_COOLDOWN_HOURS` | `--failure-notification-cooldown-hours` | `24` | Cooldown for repeated "liquidation failed" notifications with the same (market, borrower, error class). A successful liquidation for that borrower resets it immediately. |

## Observability

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `HTTP_PORT` | `--http-port` | unset (disabled) | Enables `GET /healthz` + `GET /metrics`. Never started in `RUN_MODE=once`. Both endpoints are unauthenticated. |
| `HTTP_BIND_ADDR` | `--http-bind-addr` | `127.0.0.1` | Interface the HTTP listener binds. Loopback by default so the unauthenticated endpoints above aren't reachable off the host; the shipped Compose files also publish the container port to `127.0.0.1` on the host for the same reason. Scraping from another machine is an explicit opt-in — see [README's Metrics and health](../README.md#metrics-and-health). |
| `RUST_LOG` | — | `info,templar_liquidator=debug` | Standard `tracing`/`env_logger`-style filter. Not a `clap` arg — read directly by `tracing_subscriber::EnvFilter`. |

## Safety: `DRY_RUN`

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `DRY_RUN` | `--dry-run` | `true` | Simulate only — scan and log, never submit a transaction. |

`DRY_RUN` has stricter parsing than every other boolean flag in this table, by design:

- As an **env var**, it accepts only the literal strings `true` or `false` (case-sensitive). `1`, `0`, `False`, `no`, an empty `DRY_RUN=`, or a quoted `"false"` all **fail to parse and abort the process at startup** — a malformed value is a crash loop, not a silent fallback to either mode.
- As a **CLI flag**, it accepts an optional value so live mode is reachable from argv-only surfaces (Docker Compose `command:` arrays, Cloud Run Job args): bare `--dry-run` means `true`; `--dry-run=false` or `--dry-run false` opt out.
- There is **no other way** to enable live trading. Going live is always an explicit, single-var change.

See [docs/economics.md](economics.md) for how `MIN_PROFIT_BPS`, strategy choice, and `LOOP_LIQUIDATION` interact; [docs/architecture.md](architecture.md) for how these flow through the module pipeline.
