# Security

This repo is an off-chain liquidation bot, not a smart contract. It holds a live signer key (`SIGNER_KEY`) and submits real transactions once `DRY_RUN=false` — see the [README's disclaimer](README.md#disclaimer) for the operational risk that implies for anyone running it.

## Reporting a vulnerability in this bot

If you find a vulnerability in `templar-liquidator` itself — key handling, transaction construction, an unsafe default, a way to trick the bot into an unintended liquidation, a dependency issue — report it via email to [security@templarprotocol.com](mailto:security@templarprotocol.com) rather than opening a public issue.

## Templar Protocol smart contracts

This repo consumes the [Templar Protocol contracts](https://github.com/Templar-Protocol/contracts) as a pinned dependency but does not contain them. Smart contract vulnerabilities should be reported to [the Immunefi program](https://immunefi.com/bug-bounty/templar-protocol) instead.

For more information, see [the security guide](https://docs.templarfi.org/guide/security.html).
