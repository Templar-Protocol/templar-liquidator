#!/bin/bash
# USAGE:
#   cp .env.example .env
#   # Edit .env: set SIGNER_ACCOUNT_ID and SIGNER_KEY
#   ./scripts/run-testnet.sh

set -e

# Load .env file
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="$SCRIPT_DIR/../.env"
if [ -f "$ENV_FILE" ]; then
    # shellcheck disable=SC1090
    source "$ENV_FILE"
fi

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info() { echo -e "${GREEN}[INFO]${NC} $1"; }
warn() { echo -e "${YELLOW}[WARN]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1"; }

# Validate required environment variables
if [ -z "$SIGNER_ACCOUNT_ID" ]; then
    error "SIGNER_ACCOUNT_ID not set"
    echo "  Set in .env or: export SIGNER_ACCOUNT_ID=\"your-account.testnet\""
    exit 1
fi

if [ -z "$SIGNER_KEY" ]; then
    error "SIGNER_KEY not set"
    echo "  Set in .env or: export SIGNER_KEY=\"ed25519:...\""
    exit 1
fi

# Configuration with testnet defaults
NETWORK="testnet"
REGISTRIES="${REGISTRY_ACCOUNT_IDS:-templar-registry.testnet}"
LIQUIDATION_SCAN_INTERVAL="${LIQUIDATION_SCAN_INTERVAL:-600}"
REGISTRY_REFRESH_INTERVAL="${REGISTRY_REFRESH_INTERVAL:-3600}"
CONCURRENCY="${CONCURRENCY:-10}"
PARTIAL_LIQUIDATION_PERCENTAGE="${PARTIAL_LIQUIDATION_PERCENTAGE}"
FIXED_LIQUIDATION_AMOUNT_USD="${FIXED_LIQUIDATION_AMOUNT_USD}"
LOOP_LIQUIDATION="${LOOP_LIQUIDATION:-false}"
MAX_LOOP_ITERATIONS="${MAX_LOOP_ITERATIONS:-10}"
MIN_PROFIT_BPS="${MIN_PROFIT_BPS:-50}"
DRY_RUN="${DRY_RUN:-true}"
# Exported (unlike the other CMD_ARGS-driven values above) because the binary
# reads it back via `env = "DRY_RUN"` rather than an explicit --dry-run
# argument below — a `source`d .env value stays shell-local otherwise.
export DRY_RUN

# Collateral strategy configuration
COLLATERAL_STRATEGY="${COLLATERAL_STRATEGY:-hold}"

# Swap provider configuration (1-Click; initialized automatically when needed)
ONECLICK_API_TOKEN="${ONECLICK_API_TOKEN}"

# Market filtering configuration
ALLOWED_COLLATERAL_ASSETS="${ALLOWED_COLLATERAL_ASSETS}"
IGNORED_COLLATERAL_ASSETS="${IGNORED_COLLATERAL_ASSETS}"
IGNORED_MARKETS="${IGNORED_MARKETS}"

# Oracle price update configuration

# Build binary
# scripts/ lives directly under the repo root, so the root is scripts/..
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BINARY_PATH="$PROJECT_ROOT/target/debug/liquidator"

info "Building liquidator..."
cd "$PROJECT_ROOT"
cargo build -p templar-liquidator --bin liquidator
if [ ! -f "$BINARY_PATH" ]; then
    error "Build failed"
    exit 1
fi

# Print configuration
echo ""
info "Templar Liquidator - Testnet (Inventory-Based)"
echo ""
echo "  Network:              $NETWORK"
echo "  Account:              $SIGNER_ACCOUNT_ID"
echo "  Registries:           $REGISTRIES"

# Show liquidation strategy
if [ -n "$FIXED_LIQUIDATION_AMOUNT_USD" ]; then
    echo "  Liquidation Strategy: Fixed Amount ($FIXED_LIQUIDATION_AMOUNT_USD USD)"
elif [ -n "$PARTIAL_LIQUIDATION_PERCENTAGE" ]; then
    echo "  Liquidation Strategy: Percentage ($PARTIAL_LIQUIDATION_PERCENTAGE%)"
else
    echo "  Liquidation Strategy: Percentage (100% - default)"
fi

echo "  Min Profit:           ${MIN_PROFIT_BPS} bps"
echo "  Dry Run:              $DRY_RUN"

# Show market filtering if configured
if [ -n "$ALLOWED_COLLATERAL_ASSETS" ]; then
    echo "  Allowed Assets:       $ALLOWED_COLLATERAL_ASSETS"
fi
if [ -n "$IGNORED_COLLATERAL_ASSETS" ]; then
    echo "  Ignored Assets:       $IGNORED_COLLATERAL_ASSETS"
fi
if [ -n "$IGNORED_MARKETS" ]; then
    echo "  Ignored Markets:      $IGNORED_MARKETS"
fi

echo ""

if [ "$DRY_RUN" = "true" ]; then
    info "✓ DRY RUN MODE (scan and log only, no liquidations)"
elif [ "$MIN_PROFIT_BPS" -ge 5000 ]; then
    info "✓ OBSERVATION MODE (min profit >= 50%)"
else
    warn "WARNING: Min profit is ${MIN_PROFIT_BPS} bps"
    read -p "Continue? (yes/no) " -n 3 -r
    echo
    if [[ ! $REPLY =~ ^yes$ ]]; then
        exit 0
    fi
fi

# Set log level
export RUST_LOG="${RUST_LOG:-info,templar_liquidator=debug}"

# Secrets are passed through the environment, never on argv: process arguments
# are world-readable via /proc/<pid>/cmdline, so anything in CMD_ARGS shows up
# in `ps` for every local user. clap reads each of these from the environment.
export SIGNER_KEY

# Build command arguments
CMD_ARGS=(
    "--network" "$NETWORK"
    "--signer-account" "$SIGNER_ACCOUNT_ID"
    "--liquidation-scan-interval" "$LIQUIDATION_SCAN_INTERVAL"
    "--registry-refresh-interval" "$REGISTRY_REFRESH_INTERVAL"
    "--concurrency" "$CONCURRENCY"
    "--min-profit-bps" "$MIN_PROFIT_BPS"
)

for registry in $REGISTRIES; do
    CMD_ARGS+=("--registries" "$registry")
done

# DRY_RUN reaches the binary via the exported env var above, not an argv
# flag — an argv flag would need `--dry-run=false` to select live mode, and
# omitting it entirely would leave the binary on its own default either way.

# RPC endpoint and its key. The binary reads both from the environment
# (clap: NEAR_RPC_URL, NEAR_RPC_API_KEY) and sends the key as an
# Authorization header, so nothing here needs to put a credential in a URL.
#
# NEAR_API_KEY is the older spelling, from when the bot had no header support
# and this script folded the key into the URL as `?apiKey=`. It is still
# accepted, mapped onto NEAR_RPC_API_KEY, because a credential inside a URL
# reaches every place a URL is printed — logs, process lists, error messages
# — while a header does not.
# A value that is only whitespace is not a key. Trimming before the tests below
# keeps three things in agreement: the deprecation notice still fires when
# NEAR_RPC_API_KEY is blank-but-set (which would otherwise look present and
# suppress it), no empty credential is exported, and the binary's own check —
# which trims — reaches the same verdict about the same .env.
NEAR_RPC_API_KEY="$(printf '%s' "$NEAR_RPC_API_KEY" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"
NEAR_API_KEY="$(printf '%s' "$NEAR_API_KEY" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//')"

if [ -z "$NEAR_RPC_API_KEY" ] && [ -n "$NEAR_API_KEY" ]; then
    NEAR_RPC_API_KEY="$NEAR_API_KEY"
    echo "NOTE: NEAR_API_KEY is deprecated. Rename it to NEAR_RPC_API_KEY in .env; the binary reads that one directly." >&2
fi

# A key without an explicit URL is not a mistake any more: the binary applies
# it to the network's default endpoint, which is the one it would call anyway.
[ -n "$NEAR_RPC_URL" ] && export NEAR_RPC_URL
[ -n "$NEAR_RPC_API_KEY" ] && export NEAR_RPC_API_KEY

# Add liquidation strategy arguments (mutually exclusive)
[ -n "$PARTIAL_LIQUIDATION_PERCENTAGE" ] && CMD_ARGS+=("--partial-percentage" "$PARTIAL_LIQUIDATION_PERCENTAGE")
[ -n "$FIXED_LIQUIDATION_AMOUNT_USD" ] && CMD_ARGS+=("--fixed-liquidation-amount-usd" "$FIXED_LIQUIDATION_AMOUNT_USD")

# Add loop liquidation arguments
[ "$LOOP_LIQUIDATION" = "true" ] && CMD_ARGS+=("--loop-liquidation")
[ -n "$MAX_LOOP_ITERATIONS" ] && CMD_ARGS+=("--max-loop-iterations" "$MAX_LOOP_ITERATIONS")

# Add collateral strategy arguments
CMD_ARGS+=("--collateral-strategy" "$COLLATERAL_STRATEGY")
# ONECLICK_API_TOKEN goes via the environment, not argv (see the SIGNER_KEY note above).
[ -n "$ONECLICK_API_TOKEN" ] && export ONECLICK_API_TOKEN
# Pyth Pro token: secret, goes via the environment, not argv. Without it every
# Pyth Pro-sourced market is filtered at registration.
[ -n "$LAZER_API_TOKEN" ] && export LAZER_API_TOKEN

# Add market filtering arguments
if [ -n "$ALLOWED_COLLATERAL_ASSETS" ]; then
    IFS=',' read -ra ASSETS <<< "$ALLOWED_COLLATERAL_ASSETS"
    for asset in "${ASSETS[@]}"; do
        CMD_ARGS+=("--allowed-collateral-assets" "$asset")
    done
fi

if [ -n "$IGNORED_COLLATERAL_ASSETS" ]; then
    IFS=',' read -ra ASSETS <<< "$IGNORED_COLLATERAL_ASSETS"
    for asset in "${ASSETS[@]}"; do
        CMD_ARGS+=("--ignored-collateral-assets" "$asset")
    done
fi

if [ -n "$IGNORED_MARKETS" ]; then
    IFS=',' read -ra MARKETS <<< "$IGNORED_MARKETS"
    for market in "${MARKETS[@]}"; do
        CMD_ARGS+=("--ignored-markets" "$market")
    done
fi

# Forwarded the same way, and for the same reason: this script sources .env
# without `set -a`, so a value there is shell-local and never reaches the
# binary's environment. Missing this block, a .env naming retired markets
# would launch a bot that happily scans them.
if [ -n "$DEPRECATED_MARKETS" ]; then
    IFS=',' read -ra MARKETS <<< "$DEPRECATED_MARKETS"
    for market in "${MARKETS[@]}"; do
        CMD_ARGS+=("--deprecated-markets" "$market")
    done
fi

# Add oracle price update arguments
[ -n "$LAZER_API_URL" ] && CMD_ARGS+=("--lazer-api-url" "$LAZER_API_URL")
[ -n "$LAZER_WS_URL" ] && CMD_ARGS+=("--lazer-ws-url" "$LAZER_WS_URL")

# Add Telegram notification arguments (use = syntax because chat IDs start with -)
# TELEGRAM_BOT_TOKEN goes via the environment, not argv (see the SIGNER_KEY note above).
[ -n "$TELEGRAM_BOT_TOKEN" ] && export TELEGRAM_BOT_TOKEN
[ -n "$TELEGRAM_CHAT_ID" ] && CMD_ARGS+=("--telegram-chat-id=$TELEGRAM_CHAT_ID")
[ -n "$TELEGRAM_THREAD_ID" ] && CMD_ARGS+=("--telegram-thread-id=$TELEGRAM_THREAD_ID")

info "Starting liquidator..."
echo ""
exec "$BINARY_PATH" "${CMD_ARGS[@]}"
