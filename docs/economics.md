# Economics

This document expands on the README's [worked example](../README.md#a-worked-example-with-real-numbers): where the profit actually comes from, how to size strategy and inventory around it, and what eats into it before it reaches your wallet.

## Where the profit comes from

Every Templar market defines a `liquidation_maximum_spread` (a per-market constant, e.g. 5% on `ibtc-usdc.v1.tmplr.near` at the time of writing — verify per-market via `get_configuration`). When the bot repays `D` of a position's debt, it requests collateral whose *fair* USD value (at oracle prices) is:

```text
collateral_value = D / (1 - spread)
```

The markup over `D` is exactly `spread / (1 - spread)` — for a 5% spread, ≈5.26% of the repaid amount, before gas and before any swap cost. This is the entire economic incentive for running the bot: no spread, no reason to liquidate.

[`liquidation_strategy.rs`](../src/liquidation_strategy.rs)'s `borrow_to_collateral` / `collateral_to_borrow` implement this conversion; [`profitability.rs`](../src/profitability.rs)'s `ProfitabilityCalculator` converts the resulting collateral amount back into borrow-asset terms so it can be compared against cost.

## Strategy selection

| Strategy | Flag | When to reach for it |
|---|---|---|
| **Percentage of inventory** (default) | `PARTIAL_LIQUIDATION_PERCENTAGE=<1-100>` | The general-purpose choice. A lower percentage spreads limited inventory across more liquidatable positions per round, reduces exposure to any single position, and produces faster/smaller transactions — at the cost of not fully repairing any one position in a single pass (pair with `LOOP_LIQUIDATION` to compensate by repeating the strategy against the same position across iterations). |
| **Full liquidation** | `PARTIAL_LIQUIDATION_PERCENTAGE=100` (or leave both strategy flags unset — 100% is the default) | Maximum single-pass impact: fully liquidate every eligible position your inventory can cover, capped at the position's liquidatable collateral. Best when inventory is not the binding constraint and you want each liquidatable position resolved in one transaction. |
| **Fixed USD amount** | `FIXED_LIQUIDATION_AMOUNT_USD=<usd>` | A predictable, position-size-independent capital cap per liquidation — "never risk more than $X in a single liquidation, regardless of how large the underwater position is." Only works for USD-denominated borrow assets (USDC, USDT, DAI, …): the conversion multiplies the USD figure by `10^decimals` with no price lookup, so it is silently wrong for a non-USD borrow asset. |

`PARTIAL_LIQUIDATION_PERCENTAGE` and `FIXED_LIQUIDATION_AMOUNT_USD` are mutually exclusive — the bot panics at startup if both are set.

## Tuning `MIN_PROFIT_BPS`

`MIN_PROFIT_BPS` (default `50` = 0.5%) is the final go/no-go gate: a sized liquidation is only submitted if the expected collateral value covers `liquidation_amount + gas_cost`, scaled up by this margin. A few things to weigh when tuning it:

- **It's a floor on top of the spread's markup, not a substitute for one.** A 5%-spread market clears ~526 bps gross before this gate even applies; `MIN_PROFIT_BPS=50` mostly protects against price movement between scan and execution and against thin-spread markets, not against zero-spread ones.
- **The bot already reserves a safety margin below what you'd compute by hand.** Both built-in strategies apply a `SAFETY_BUFFER_BPS` (50 bps / 0.5%) pad to the theoretical repay amount, and clamp the requested collateral to `LIQUIDATABLE_CAP_BUFFER_BPS` (300 bps / 3%) under the on-chain eligibility cap to absorb price drift between scan time and transaction execution. These aren't configurable — they're the bot's own execution-safety margin, separate from the profitability gate.
- **Set it too low** and marginal liquidations that don't survive oracle-price movement or swap slippage start reverting or eating into what should have been profit.
- **Set it too high** and the bot skips real liquidatable positions — every skip is a `LiquidationOutcome::Unprofitable`, visible in logs and (if `HTTP_PORT` is set) folded into `templar_liquidator_candidates_found_total` but not `templar_liquidator_liquidations_attempted_total`.

There's no universally correct value; start conservative (the 50 bps default, or higher while you're still validating a deployment in dry-run) and lower it once you've watched real scan output long enough to trust the market's actual spread and your RPC's price freshness.

## Inventory sizing

The bot **never buys inventory** — it only spends what's already sitting in `SIGNER_ACCOUNT_ID`'s wallet. You need to hold the **borrow asset** of every market you intend to serve (see the README FAQ). Practical guidance:

- Size inventory per-asset against the liquidation volume you expect to catch, not against total protocol TVL — a bot with $500 of USDC inventory simply cannot fully liquidate a $50,000 underwater position; it will either skip it (`InsufficientBalance` → `LiquidationOutcome::Skipped`) or catch a partial slice of it, repeatedly, across rounds.
- `PARTIAL_LIQUIDATION_PERCENTAGE` interacts directly with sizing: a lower percentage means each round consumes less inventory per position, letting fixed inventory reach more positions before running dry — at the cost of needing more rounds (or `LOOP_LIQUIDATION`) to fully repair any one of them.
- Inventory is refreshed periodically (see [`inventory.rs`](../src/inventory.rs)); a liquidation that would exceed the currently-known available balance is skipped rather than attempted and left to fail on-chain.
- Full automatic rebalancing of inventory levels (e.g. proactively topping up a depleted asset from another) is not implemented — see [docs/backlog.md](backlog.md). What *is* implemented is collateral-side rebalancing, next.

## Swap cost and slippage

Only relevant when `COLLATERAL_STRATEGY=swap-to-borrow` — the default (`hold`) never swaps, so this section doesn't apply until you opt in.

- **1-Click** (`swap/oneclick.rs`) — the only shipped venue — charges a 0.1% fee when `ONECLICK_API_TOKEN` is unset; set the token to avoid it. Its slippage tolerance is `DEFAULT_MAX_SLIPPAGE_BPS` of 300 (3%), not currently exposed as a CLI/env knob. 1-Click only supports exact-input swaps (spend a known input amount, receive however much the venue returns) — `SwapProvider::quote` is unimplemented (see [docs/backlog.md](backlog.md)). A failing swap is retried per `SWAP_RETRY_*`, or surfaces as a `SwapIssue::Failed` notification if retries are exhausted.
- **`MIN_SWAP_VALUE_USD`** (default `10.0`) skips swaps below this threshold rather than paying fixed costs (gas, potential storage-registration deposits) on a swap too small to be worth it. Skipped amounts aren't lost — they accumulate as held collateral and get picked up by the next `BATCH_SWAP_ON_CYCLE_START` batch swap (default `true`) once the accumulated value clears the threshold.
- Every swap cost — venue fee, slippage, and any storage-registration deposit for a token the bot has never held before — comes **out of** the gross spread markup computed above; it is not currently factored into the pre-execution profitability gate (`should_liquidate` compares collateral value to `liquidation_amount + gas_cost` only). A thin-spread market combined with `swap-to-borrow` can turn a liquidation that looked profitable at execution time into a net loss once swap costs land. Widen `MIN_PROFIT_BPS` accordingly if you run `swap-to-borrow` on low-spread markets.

## Loop-liquidation semantics

`LOOP_LIQUIDATION=true` (default `false`) makes the bot re-check the same position after each liquidation and repeat the process — re-scan status, re-size, re-check profitability, re-execute — until the position reports healthy, a sizing/profitability check fails, or `MAX_LOOP_ITERATIONS` (default `10`) is reached.

- **Disabled in dry-run automatically.** In dry-run no liquidation actually executes, so the position's on-chain state never changes between iterations — looping would just repeat the same simulated result. The bot caps iterations at 1 in dry-run regardless of `LOOP_LIQUIDATION`.
- **Compensates for partial strategies.** `PARTIAL_LIQUIDATION_PERCENTAGE=25` with `LOOP_LIQUIDATION=true` will keep re-liquidating a position in 25%-of-inventory slices (subject to `MIN_PROFIT_BPS` on each slice) until it's healthy or the loop cap / inventory runs out — approximating a full liquidation via several smaller transactions instead of one large one.
- **Re-fetches position state** between iterations (fresh collateral/debt amounts), but reuses the oracle response fetched at the start of the round; a long-running loop on a stale price is bounded by the market's own `price_maximum_age_s`. For on-chain price reads that bound is enforced by the contract, independent of anything the bot does; for proxy prices composed off-chain at scan time, the bot applies the same bound itself before using an entry (see `oracle.rs`), and the market contract still enforces it against its own oracle at execution.
- **Safety cap, not a target.** `MAX_LOOP_ITERATIONS` exists to prevent a pathological position (or a bug) from looping indefinitely; hitting it stops the loop and returns whatever partial progress was made, it does not retry later within the same round.
