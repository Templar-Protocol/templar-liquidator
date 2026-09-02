#!/usr/bin/env bash
# check-repo-invariants.sh — enforce the two cross-file pins that CLAUDE.md
# describes and that nothing else in CI can catch.
#
# Both failures are nasty precisely because the build does not name them:
# a drifted templar-* rev surfaces as "expected MarketConfiguration, found
# MarketConfiguration", and a drifted Rust pin surfaces as CI and Docker
# happily compiling syntax the declared MSRV does not support.
#
# Run it locally the same way CI does: ./scripts/check-repo-invariants.sh
set -euo pipefail

cd "$(dirname "$0")/.."

fail=0
note() { printf '  %s\n' "$*"; }
bad() { printf '::error::%s\n' "$*"; fail=1; }

# ── 1. THE SINGLE-REV RULE ───────────────────────────
# Every dependency on the Templar contracts repo must be pinned to the SAME
# rev, and sandbox.yml's CONTRACTS_REV must match it — that workflow clones
# the contracts repo at that rev to build the wasms the sandbox test loads.
#
# Matched by the repository URL rather than by crate name, on purpose. Naming
# the repo is what the rule is actually about (two checkouts of the SAME
# upstream repo are what fail to unify), it picks up `test-utils` which does
# not carry the templar- prefix, and it leaves an unrelated git dependency
# with its own independent rev alone instead of failing CI over it.
echo "THE SINGLE-REV RULE"
CONTRACTS_REPO="github.com/Templar-Protocol/contracts"
mapfile -t revs < <(grep -F "${CONTRACTS_REPO}" Cargo.toml |
	grep -oE 'rev = "[0-9a-f]{7,40}"' | grep -oE '[0-9a-f]{7,40}' | sort -u)

if [ "${#revs[@]}" -eq 0 ]; then
	bad "no rev pins found for ${CONTRACTS_REPO} in Cargo.toml — has the dependency style changed?"
elif [ "${#revs[@]}" -gt 1 ]; then
	bad "${CONTRACTS_REPO} dependencies are pinned to ${#revs[@]} different revs; they must all match:"
	printf '    %s\n' "${revs[@]}"
else
	note "all ${CONTRACTS_REPO} deps pinned to ${revs[0]}"

	sandbox_rev=$(grep -oE '^\s*CONTRACTS_REV:\s*[0-9a-f]{7,40}' .github/workflows/sandbox.yml |
		grep -oE '[0-9a-f]{7,40}' || true)
	if [ -z "${sandbox_rev}" ]; then
		bad "could not read CONTRACTS_REV from .github/workflows/sandbox.yml"
	elif [ "${sandbox_rev}" != "${revs[0]}" ]; then
		bad "sandbox.yml CONTRACTS_REV (${sandbox_rev}) != Cargo.toml rev (${revs[0]})"
	else
		note "sandbox.yml CONTRACTS_REV matches"
	fi

	# docs/testing.md walks a human through the same checkout by hand, so it
	# carries the rev too — a copy this check missed once already.
	doc_revs=$(grep -oE '[0-9a-f]{40}' docs/testing.md | sort -u || true)
	if [ -z "${doc_revs}" ]; then
		bad "no 40-hex rev found in docs/testing.md — its checkout walkthrough should pin the contracts rev"
	elif [ "${doc_revs}" != "${revs[0]}" ]; then
		bad "docs/testing.md pins a different rev than Cargo.toml (${revs[0]}):"
		printf '    %s\n' "${doc_revs}"
	else
		note "docs/testing.md rev matches"
	fi
fi

# ── 2. THE THREE-WAY RUST PIN ────────────────────────
# Cargo.toml `rust-version` (declared MSRV), rust-toolchain.toml `channel`
# (what CI and local dev build with), and the Dockerfile builder's tag must
# agree. A channel newer than rust-version makes CI and Docker accept syntax
# the declared MSRV does not support.
echo "Rust version pins"
cargo_rv=$(grep -oE '^rust-version\s*=\s*"[^"]+"' Cargo.toml | grep -oE '[0-9]+\.[0-9]+(\.[0-9]+)?' || true)
toolchain=$(grep -oE '^channel\s*=\s*"[^"]+"' rust-toolchain.toml | grep -oE '[0-9]+\.[0-9]+(\.[0-9]+)?' || true)
docker_rv=$(grep -oE '^FROM rust:[0-9]+\.[0-9]+(\.[0-9]+)?' Dockerfile | grep -oE '[0-9]+\.[0-9]+(\.[0-9]+)?' || true)

for pair in "Cargo.toml rust-version:${cargo_rv}" "rust-toolchain.toml channel:${toolchain}" "Dockerfile FROM rust:${docker_rv}"; do
	if [ -z "${pair#*:}" ]; then
		bad "could not parse ${pair%%:*}"
	else
		note "${pair%%:*} = ${pair#*:}"
	fi
done

if [ -n "${cargo_rv}" ] && [ -n "${toolchain}" ] && [ -n "${docker_rv}" ]; then
	if [ "${cargo_rv}" != "${toolchain}" ] || [ "${cargo_rv}" != "${docker_rv}" ]; then
		bad "the three Rust pins disagree — bump all three together (see rust-toolchain.toml's comment)"
	else
		note "all three agree"
	fi
fi

if [ "${fail}" -ne 0 ]; then
	echo
	echo "Repository invariants violated — see CLAUDE.md."
	exit 1
fi
echo
echo "All repository invariants hold."
