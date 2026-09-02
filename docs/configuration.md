# Configuration reference

Every setting the bot reads. Almost all come from [`src/config.rs`](../src/config.rs)'s `Args` struct (`.env.example` is a curated subset with commentary; this table is exhaustive); the exceptions are the process-level logging variables `RUST_LOG` and `LOG_FORMAT`, read directly at startup and marked `—` in the flag column below.

## Precedence

**CLI flag beats env var beats default.** Every flag below also accepts its env var form; if both are set, the explicit CLI flag wins (standard `clap` `env` behavior). `docker-compose.yml`'s `command:` array sets flags from `.env` interpolation, so in the shipped Compose setups the env var is effectively what you're editing either way.

`--dry-run` is the one flag with special parsing — see below.

## Required

The bot refuses to start without these three:

| Env var | CLI flag | Description |
|---|---|---|
| `REGISTRY_ACCOUNT_IDS` | `--registries`, `-r` | Market registry account(s) to discover markets from. Comma-separated, like the filter knobs below (`REGISTRY_ACCOUNT_IDS=a.near,b.near`); the CLI flag is also repeatable (`--registries a.near --registries b.near`). |
| `SIGNER_KEY` | `--signer-key`, `-k` | The signer account's private key, e.g. `ed25519:...`. |
| `SIGNER_ACCOUNT_ID` | `--signer-account`, `-s` | The NEAR account the bot signs transactions as. |

## Network

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `NEAR_NETWORK` | `--network`, `-n` | `testnet` | `mainnet` or `testnet`. |
| `NEAR_RPC_URL` | `--near-rpc-url` | network default (`https://rpc.mainnet.fastnear.com` / `https://rpc.testnet.fastnear.com`) | Custom RPC endpoint. |
| `NEAR_RPC_API_KEY` | `--near-rpc-api-key` | unset | API key for the RPC endpoint, sent as an `Authorization` header. Effectively required against a public endpoint: unauthenticated, a full-registry scan gets rate-limited part-way through and the round completes over whatever it could read, without failing. The run scripts export it and accept the older `NEAR_API_KEY` spelling as a deprecated alias, mapping it across with a notice; the binary reads only `NEAR_RPC_API_KEY`, and warns at startup when it finds the old name alone. Endpoints that authenticate by query parameter instead of by header take the key folded into `NEAR_RPC_URL` as `?apiKey=` — still supported, and the only option against such an endpoint, but it puts the credential in every place a URL is printed, so the header is the default and what the scripts use. |

## Execution

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `RUN_MODE` | `--run-mode` | `loop` | `loop` (continuous), `once` (single registry refresh + liquidation round, then exit — for cron-style schedulers), or `push-check` (diagnostic: one registry refresh, then for every admitted market push prices on-chain — live mode only — and report whether its oracle reads fresh within its own `price_maximum_age_s`; exit 0 only when this process's push succeeded and every market then read fresh — live — or after a successful non-empty refresh in dry-run, which only reads and reports; a failed or empty registry refresh exits 1 in both). `push-check` with `DRY_RUN=false` requires at least one push leg: `LAZER_API_TOKEN` (Pyth Pro) or `REDSTONE_PUSH=true` (RedStone, the default). |
| — | `--once` | `false` | Shorthand for `--run-mode once`. No env var equivalent; forces once mode and takes precedence over `--run-mode` if both are given. |
| `LIQUIDATION_SCAN_INTERVAL` | `--liquidation-scan-interval` | `600` | Seconds between liquidation scan rounds (loop mode). |
| `REGISTRY_REFRESH_INTERVAL` | `--registry-refresh-interval` | `3600` | Seconds between registry re-discovery (loop mode). |
| `CONCURRENCY` | `--concurrency`, `-c` | `10` | Concurrency for registry deployment listing. Must be ≥ 1 — `0` would stall the pipeline and is rejected at startup. |
| `POSITION_CONCURRENCY` | `--position-concurrency` | `1` | Positions evaluated/liquidated concurrently within one market's round. `1` (the default) is fully sequential with a 1-second pause between positions — what free public RPC endpoints tolerate. Raising it drops the pause and fans evaluation out; each in-flight position costs several RPC reads (and in live mode, possibly an oracle push), so bring an RPC endpoint sized for the load. Must be ≥ 1 (`0` is rejected at startup). Validate a raised value in dry-run or a staging deployment before going live: watch for RPC rate-limit errors, "Inventory no longer covers the sized amount" skips (thin inventory makes the knob buy less than it looks), and "Notification dropped" warnings. |

## Liquidation strategy

`PARTIAL_LIQUIDATION_PERCENTAGE` and `FIXED_LIQUIDATION_AMOUNT_USD` are **mutually exclusive** — setting both panics at startup with a clear error. Neither set → percentage strategy at 100% (full liquidation).

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `PARTIAL_LIQUIDATION_PERCENTAGE` | `--partial-percentage` | unset (100% if neither strategy flag is set) | Percentage (1–100) of available inventory to deploy per liquidation. |
| `FIXED_LIQUIDATION_AMOUNT_USD` | `--fixed-liquidation-amount-usd` | unset | Fixed USD amount to repay per liquidation. USD-denominated borrow assets only (no price lookup — assumes the borrow asset is a USD stablecoin). On markets requiring full liquidation (contract version < 1.1.0) it acts as an eligibility threshold, not a cap: a full liquidation must buy the position's *entire* collateral deposit, so a position is skipped when that whole deposit — valued at the liquidation discount, plus a 0.5% safety buffer — costs more than the budget. That threshold tracks collateral value, not debt, so it can sit far above what the position owes. |
| `MIN_PROFIT_BPS` | `--min-profit-bps` | `50` | Minimum profit margin, in basis points, required to submit a liquidation. |
| `LOOP_LIQUIDATION` | `--loop-liquidation` | `false` | Repeatedly liquidate the same position (re-checking each iteration) until it's healthy or inventory runs out. Disabled in dry-run (position state never changes there, so re-checking is a no-op). |
| `MAX_LOOP_ITERATIONS` | `--max-loop-iterations` | `10` | Safety cap on loop-liquidation iterations. Must be ≥ 1 — `0` would mean "never liquidate anything" and is rejected at startup. |

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
| `ALLOWED_MARKETS` | `--allowed-markets` | empty (all markets) | Allowlist of market account ids; when set, only these markets are processed. `IGNORED_MARKETS` still subtracts within the allowlist (a market on both lists is skipped). Entries are parsed as account ids at startup — an unparseable entry refuses to start rather than silently emptying the allowlist (which would fail open to every market). |
| `IGNORED_MARKETS` | `--ignored-markets` | empty | Market account ids to skip entirely. |

## Oracle price sources

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `LAZER_WS_URL` | `--lazer-ws-url` | `wss://pyth-lazer-0.dourolabs.app/v1/stream` | Pyth Pro websocket stream the on-chain price push subscribes to (same bearer token as `LAZER_API_TOKEN`). Must be `wss://` when a token is set — refused at startup otherwise. One endpoint; no multi-host failover. |
| `REDSTONE_API_URL` | `--redstone-api-url` | `https://api.redstone.finance` | RedStone public price API, used to compose proxy-oracle prices off-chain at scan time (no gas, no keeper dependency). Scan-side only — execution still prices through the on-chain oracle. |
| `REDSTONE_GATEWAY_URL` | `--redstone-gateway-url` | `https://oracle-gateway-1.a.redstone.finance` | RedStone data-package gateway the RedStone push leg fetches signed packages from (public, no key; one GET of the data-service dump per push). Must be `https` — the packages are signed, but the gateway decides which feed set the bot submits. |
| `REDSTONE_DATA_SERVICE_ID` | `--redstone-data-service-id` | `redstone-primary-prod` | RedStone data-service id, used by **both** RedStone legs. Push: the service whose signer set the adapters are configured with — packages from any other service are rejected on-chain. Scan: the `provider` of every `/prices` query, so it must also name a service the public price API serves, or scan pricing goes quiet for every RedStone-priced market. Rejected at startup if empty. |
| `REDSTONE_PUSH` | `--redstone-push` | `true` | RedStone push leg: before a liquidation, push RedStone-signed packages to the RedStone adapter for feeds without a Pyth Pro source (the proxy's RedStone-only feeds and Pyth Core + RedStone feeds) — nothing else keeps those adapters fresh. Packages are verified locally against the adapter's own signer set, threshold and timestamp window (read from the contract) before any gas is spent; an untrusted writer's minimum interval per feed (40 s on mainnet) is enforced inside the contract, and the bot keeps a memo of its own writes so it never resubmits a feed inside that interval. Feeds with a Pyth Pro source are RedStone-pushed only when the Pyth Pro push is unavailable (no `LAZER_API_TOKEN`); otherwise one transaction fewer. Same optional-value form as `DRY_RUN`. |
| `LAZER_API_URL` | `--lazer-api-url` | `https://pyth-lazer.dourolabs.app` | Pyth Pro price API for scan-side proxy price composition — the only Pyth Pro scan leg (no on-chain adapter read). Only used when `LAZER_API_TOKEN` is set; without a token, Pyth Pro–sourced markets are filtered at registration. Must be `https` when the token is set — a bearer token over plain http travels in cleartext, so the bot refuses to start. |
| `LAZER_API_TOKEN` | `--lazer-api-token` | unset | Pyth API key from Pyth Terminal (pythdata.app; no anonymous tier) — the same key authenticates Pyth Pro (Lazer) and the authenticated Hermes endpoint; the bot uses it for Pyth Pro only. Enables both the scan-side Pyth Pro API leg and the on-chain Pyth Pro push. When unset, Pyth Pro–sourced markets are filtered at registration (scan prices are off-chain only — there is no adapter read to fall back to). |

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
| `HTTP_PORT` | `--http-port` | unset (disabled) | Enables `GET /healthz` + `GET /metrics`. Never started in `RUN_MODE=once` or `push-check`. Both endpoints are unauthenticated. |
| `HTTP_BIND_ADDR` | `--http-bind-addr` | `127.0.0.1` | Interface the HTTP listener binds. Loopback by default so the unauthenticated endpoints above aren't reachable off the host; the shipped Compose files also publish the container port to `127.0.0.1` on the host for the same reason. Scraping from another machine is an explicit opt-in — see [README's Metrics and health](../README.md#metrics-and-health). |
| `RUST_LOG` | — | `info,templar_liquidator=debug` | Standard `tracing`/`env_logger`-style filter. Not a `clap` arg — read directly by `tracing_subscriber::EnvFilter`. |
| `LOG_FORMAT` | — | `text` | Set to `json` for one JSON object per log line (for aggregators like Loki or CloudWatch). Not a `clap` arg — read directly at startup. |

## Safety: `DRY_RUN`

| Env var | CLI flag | Default | Description |
|---|---|---|---|
| `DRY_RUN` | `--dry-run` | `true` | Simulate only — scan and log, never submit a transaction. |

`DRY_RUN` has stricter parsing than every other boolean flag in this table, by design:

- As an **env var**, it accepts only the literal strings `true` or `false` (case-sensitive). `1`, `0`, `False`, `no`, an empty `DRY_RUN=`, or a quoted `"false"` all **fail to parse and abort the process at startup** — a malformed value is a crash loop, not a silent fallback to either mode.
- As a **CLI flag**, it accepts an optional value so live mode is reachable from argv-only surfaces (Docker Compose `command:` arrays, Cloud Run Job args): bare `--dry-run` means `true`; `--dry-run=false` or `--dry-run false` opt out.
- There is **no other way** to enable live trading. Going live is always an explicit, single-var change.

See [docs/economics.md](economics.md) for how `MIN_PROFIT_BPS`, strategy choice, and `LOOP_LIQUIDATION` interact; [docs/architecture.md](architecture.md) for how these flow through the module pipeline.
