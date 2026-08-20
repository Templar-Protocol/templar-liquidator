#!/usr/bin/env bash
# check-release.sh <tag>
#
# Verify a release tag is internally consistent BEFORE anything is published.
# Every check here guards a failure that is silent rather than loud: the image
# builds, the release appears, and the wrongness only shows up later in an
# operator's `--version` output or a changelog nobody can find.
#
# Run it locally before tagging:  ./scripts/check-release.sh v0.2.0
set -euo pipefail

cd "$(dirname "$0")/.."

tag="${1:?usage: $0 vX.Y.Z}"
version="${tag#v}"
fail=0
note() { printf '  %s\n' "$*"; }
bad() { printf '::error::%s\n' "$*"; fail=1; }

echo "Release preflight for ${tag}"

# 1. The tag itself must be v + semver. The release workflow triggers on "v*",
#    which would happily accept `vfoo` and hand a nonsense tag to the registry.
if ! printf '%s' "${version}" | grep -qE '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?(\+[0-9A-Za-z.-]+)?$'; then
	bad "tag '${tag}' is not v<semver> (expected vX.Y.Z, optionally -prerelease)"
fi

# 2. Cargo.toml must agree. Otherwise the image is LABELLED with the tag while
#    the binary inside reports the old version — the mismatch survives into
#    every deployment and is invisible until someone checks.
cargo_version=$(grep -m1 -oE '^version = "[^"]+"' Cargo.toml | grep -oE '[0-9][^"]*' || true)
if [ -z "${cargo_version}" ]; then
	bad "could not read version from Cargo.toml"
elif [ "${cargo_version}" != "${version}" ]; then
	bad "Cargo.toml version (${cargo_version}) != tag (${version}) — bump Cargo.toml, or tag the right version"
else
	note "Cargo.toml version = ${cargo_version}"
fi

# 3. Cargo.lock must have been regenerated after the bump. A stale lock means
#    `cargo build --locked` (what the Dockerfile runs) fails inside the release
#    build rather than here, wasting the tag.
lock_version=$(awk '/^name = "templar-liquidator"$/ { found = 1; next } found && /^version = / { gsub(/[",]/, "", $3); print $3; exit }' Cargo.lock || true)
if [ -z "${lock_version}" ]; then
	bad "could not read templar-liquidator version from Cargo.lock"
elif [ "${lock_version}" != "${version}" ]; then
	bad "Cargo.lock has templar-liquidator ${lock_version}, expected ${version} — run 'cargo update -p templar-liquidator' and commit the lock"
else
	note "Cargo.lock version = ${lock_version}"
fi

# 4. CHANGELOG must carry a section for this version. The release notes are
#    generated from commit history, so a missing entry is not otherwise
#    noticed — the release just quietly ships without the curated summary.
if grep -qE "^## \[${version}\]" CHANGELOG.md; then
	note "CHANGELOG.md has a section for ${version}"
else
	bad "CHANGELOG.md has no '## [${version}]' section — add one before tagging"
fi

if [ "${fail}" -ne 0 ]; then
	echo
	echo "Release preflight failed for ${tag}; nothing has been published."
	exit 1
fi
echo
echo "Release preflight passed for ${tag}."
