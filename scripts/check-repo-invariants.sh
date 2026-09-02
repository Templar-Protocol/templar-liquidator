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

	# Cargo.toml may pin full 40-hex while a copy uses a short form (or the
	# reverse); either direction is the same pin, so compare by prefix.
	same_rev() {
		case "$1" in "$2"*) return 0 ;; esac
		case "$2" in "$1"*) return 0 ;; esac
		return 1
	}

	# Every other copy of the pin, each extracted from the line that carries
	# it rather than by scanning for bare hex — an unrelated hash in a doc
	# must not trip this, and an anchored miss must fail loudly:
	#   - sandbox.yml CONTRACTS_REV (the workflow's clone)
	#   - sandbox.yml's cargo-near action ref (the @rev it installs FROM —
	#     left behind, the job runs one rev's installer against another
	#     rev's checkout)
	#   - docs/testing.md's checkout walkthrough (the copy a human follows)
	check_rev_copy() {
		# $1: label for messages, $2: newline-separated extracted revs
		if [ -z "$2" ]; then
			bad "could not read the contracts rev from $1"
			return
		fi
		local r ok=1
		while IFS= read -r r; do
			if ! same_rev "$r" "${revs[0]}"; then
				bad "$1 pins ${r}, Cargo.toml pins ${revs[0]}"
				ok=0
			fi
		done <<<"$2"
		if [ "$ok" -eq 1 ]; then
			note "$1 matches"
		fi
	}

	check_rev_copy "sandbox.yml CONTRACTS_REV" \
		"$(grep -oE '^\s*CONTRACTS_REV:\s*[0-9a-f]{7,40}' .github/workflows/sandbox.yml | grep -oE '[0-9a-f]{7,40}' | sort -u || true)"
	check_rev_copy "sandbox.yml cargo-near action ref" \
		"$(grep -oE 'Templar-Protocol/contracts/[^@[:space:]]*@[0-9a-f]{7,40}' .github/workflows/sandbox.yml | grep -oE '[0-9a-f]{7,40}$' | sort -u || true)"
	check_rev_copy "docs/testing.md checkout" \
		"$(grep -oE 'checkout [0-9a-f]{7,40}' docs/testing.md | awk '{print $2}' | sort -u || true)"
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
