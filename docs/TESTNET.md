# Testnet walkthrough

An end-to-end path from nothing to watching the bot evaluate (and, if you choose, liquidate) a position on NEAR testnet. Steps are marked **[verified]** where this was actually run against live testnet/mainnet infrastructure while writing this doc (2026-08-18), or **[procedure]** where it's the documented steps without a live run — testnet market/registry availability changes over time, so treat those as "how to," not "guaranteed to work as-is today."

## 1. Create a testnet account and key **[verified]**

This repo's devcontainer ships [`near-cli-rs`](https://github.com/near/near-cli-rs). Create a fresh testnet account funded by the faucet:

```bash
near account create-account sponsor-by-faucet-service YOUR_NAME.testnet \
  autogenerate-new-keypair save-to-legacy-keychain network-config testnet create
```

This funds the new account with test NEAR (for gas and storage deposits) and writes the key to `~/.near-credentials/testnet/YOUR_NAME.testnet.json` in the legacy-keychain format — the same JSON shape `near-cli`'s original JS tool used, and the format most tooling (including reading `private_key` straight into `SIGNER_KEY`) expects. Confirm it landed without printing the key itself:

```bash
near account view-account-summary YOUR_NAME.testnet network-config testnet now
grep -o '"account_id"\|"public_key"\|"private_key"' ~/.near-credentials/testnet/YOUR_NAME.testnet.json | sort -u
```

Don't `cat` this file — its `.private_key` field would print straight to your terminal, and terminal scrollback, session recordings, and support transcripts can all retain it. Step 3 below pulls the key into `.env` directly, without ever displaying it.

If you're on a devcontainer that gets rebuilt, `~/.near-credentials` doesn't persist — re-run the create-account step (it's idempotent from the faucet's perspective for a *new* account name) or keep the key somewhere durable before you rebuild.

## 2. Fund inventory **[procedure]**

The bot spends the **borrow asset** of whatever market you point it at (see the README FAQ) — it needs some in `YOUR_NAME.testnet`'s wallet before it can liquidate anything. On testnet this is almost always a test stablecoin (a NEP-141 FT contract), not NEAR itself. To get some:

1. Identify the borrow asset's contract id from the target market's `get_configuration` (see step 3).
2. Register storage on that FT contract for your account (`storage_deposit`), if it isn't auto-registered.
3. Obtain test tokens — typically a project-run faucet, a `mint`/`ft_transfer` call from an account that already holds them, or asking in the project's community channels. There is no single documented faucet command here because it's specific to whichever test token the market you're targeting actually uses.

## 3. Configure `.env` for testnet **[verified command syntax / procedure for the registry value]**

```bash
cp .env.example .env
chmod 600 .env   # it is about to hold a private key; cp leaves it world-readable
```

Set at minimum:

```bash
NEAR_NETWORK=testnet
SIGNER_ACCOUNT_ID=YOUR_NAME.testnet
REGISTRY_ACCOUNT_IDS=<testnet registry — see below>
DRY_RUN=true                    # default; leave it for now
```

Pull `SIGNER_KEY` in from the credentials file directly, rather than copy-pasting a printed key — this replaces `.env.example`'s `SIGNER_KEY=ed25519:YOUR_PRIVATE_KEY_HERE` placeholder line in place. The key is never expanded into a shell command, because process arguments are world-readable through `/proc/<pid>/cmdline`; only the file path is:

```bash
KEY_FILE=~/.near-credentials/testnet/YOUR_NAME.testnet.json
python3 - "$KEY_FILE" <<'PY'
import json, pathlib, re, sys
key = json.load(open(sys.argv[1]))["private_key"]
env = pathlib.Path(".env")
env.write_text(re.sub(r"(?m)^SIGNER_KEY=.*$", "SIGNER_KEY=" + key, env.read_text()))
PY
```

`scripts/run-testnet.sh` defaults `REGISTRY_ACCOUNT_IDS` to `templar-registry.testnet` if unset. **Verify this before relying on it** — testnet deployments are less stable than mainnet's, and at the time of writing (2026-08-18) `templar-registry.testnet` did not resolve on testnet RPC:

```bash
curl -s https://rpc.testnet.fastnear.com -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"query","params":{"request_type":"view_account","finality":"final","account_id":"templar-registry.testnet"}}'
# {"error":{"cause":{"name":"UNKNOWN_ACCOUNT"}, ...}} means it doesn't currently exist
```

If it doesn't exist when you try this, get the current testnet registry account id from the project (issues/community channels), or stand up your own test market by following the [contracts repo](https://github.com/Templar-Protocol/contracts)'s own deployment tooling, and set `REGISTRY_ACCOUNT_IDS` to that instead.

Also relevant for testnet:

```bash
REF_CONTRACT=v2.ref-dev.testnet   # only if you plan to test COLLATERAL_STRATEGY=swap-to-borrow
```

## 4. Find or create a borrow position **[procedure]**

Once `REGISTRY_ACCOUNT_IDS` points at a real testnet registry, enumerate its markets and check for existing borrow positions the same way the README's mainnet worked example does, substituting the testnet RPC endpoint:

```bash
curl -s https://rpc.testnet.fastnear.com -H 'Content-Type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"query","params":{"request_type":"call_function","finality":"final","account_id":"<registry>.testnet","method_name":"list_deployments","args_base64":"'"$(printf '{"offset":0,"count":10}' | base64 -w0)"'"}}'
```

To drive a position underwater deliberately for testing (rather than waiting for a real price move): open a borrow position against a test market at close to its `borrow_mcr_liquidation` threshold, then either wait for interest to accrue it past the threshold, or — if the test market's oracle is itself testnet-mocked/controllable — move the oracle price. The exact mechanics depend on the specific test market's setup and are out of scope for this bot's own docs; see the [contracts repo](https://github.com/Templar-Protocol/contracts)'s testing tooling for sandbox-level control over positions and prices, and this repo's own `tests/liquidation_sandbox.rs` (see [docs/testing.md](testing.md)) for a fully self-contained, no-live-network alternative that doesn't depend on testnet's state at all.

## 5. Watch the bot evaluate it in dry-run **[verified command syntax]**

```bash
./scripts/run-testnet.sh
```

or directly:

```bash
docker run --env-file .env ghcr.io/templar-protocol/templar-liquidator:latest
```

> This pull currently fails for anyone outside the organization — the GHCR
> package is private and cannot yet be made public
> ([#24](https://github.com/Templar-Protocol/templar-liquidator/issues/24)).
> Use `docker compose up` instead, which builds from source and needs no
> registry access. See the note in the [README quickstart](../README.md#quickstart).

With `DRY_RUN=true` (the default — leave it), the bot scans, logs what it finds, and logs what it *would* do for any liquidatable position it finds — repay amount, collateral it would request, and the profitability verdict — without submitting anything. This is also where you'll discover configuration mistakes (wrong registry, no inventory, RPC issues) cheaply.

## 6. Go live **[procedure]**

Once you've watched a real liquidatable position get correctly identified and sized in the logs:

```bash
# .env
DRY_RUN=false
```

Restart the bot. It will now submit the liquidation transaction for real, against testnet. This is the same `DRY_RUN` mechanism used everywhere else in this repo — see [docs/configuration.md](configuration.md#safety-dry_run) for the exact parsing rules.
