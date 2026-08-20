#!/usr/bin/env bash
# post-create.sh — dev container one-time setup (postCreateCommand).
#
# Everything after the toolchain step is non-fatal: a crates.io hiccup should
# leave you inside a usable container to retry from, not fail the create and
# drop you back to the host with nothing.

set -euo pipefail

warn() { echo "!!  $*" >&2; }

# Memory available to THIS container, in kB.
#
# Prefers the cgroup limit over /proc/meminfo, which is not cgroup-aware: run
# with an explicit `docker --memory`, /proc/meminfo still reports the host or
# VM figure and we would size the build for memory the container cannot have.
# When no limit is set — the Docker Desktop default, where memory.max reads
# "max" — the VM itself is the constraint and /proc/meminfo is the right
# number.
#
# Every read is guarded and the function always prints a plain integer. This
# whole step is non-fatal by contract, so an unreadable /proc or cgroup must
# degrade to "assume nothing available" (and therefore one job) rather than
# abort the script under `set -e`.
available_kb() {
	local limit="" current="" avail=""

	if [ -r /sys/fs/cgroup/memory.max ]; then
		limit=$(cat /sys/fs/cgroup/memory.max 2>/dev/null || true)
		[ "${limit}" = "max" ] && limit=""
		if [ -n "${limit}" ]; then
			current=$(cat /sys/fs/cgroup/memory.current 2>/dev/null || true)
		fi
	elif [ -r /sys/fs/cgroup/memory/memory.limit_in_bytes ]; then
		limit=$(cat /sys/fs/cgroup/memory/memory.limit_in_bytes 2>/dev/null || true)
		# cgroup v1 reports a near-INT64_MAX sentinel when unlimited.
		case "${limit}" in
		'' | *[!0-9]*) limit="" ;;
		*) [ "${#limit}" -ge 19 ] && limit="" ;;
		esac
		if [ -n "${limit}" ]; then
			current=$(cat /sys/fs/cgroup/memory/memory.usage_in_bytes 2>/dev/null || true)
		fi
	fi

	if [ -n "${limit}" ]; then
		# A limit is in force. Usage must be readable and numeric to say
		# anything about free memory — treating an unreadable usage as 0 would
		# hand back the WHOLE limit as available and license exactly the
		# over-parallel build this function exists to prevent. /proc/meminfo is
		# not a safe fallback here either: under a limit it reports host
		# memory, which is worse. Report nothing available and let the caller
		# clamp to a single job.
		case "${limit}" in '' | *[!0-9]*) echo 0; return ;; esac
		case "${current}" in '' | *[!0-9]*) echo 0; return ;; esac
		avail=$(( (limit - current) / 1024 ))
		[ "${avail}" -lt 0 ] && avail=0
	else
		avail=$(awk '/^MemAvailable:/ { print $2 }' /proc/meminfo 2>/dev/null || true)
	fi

	case "${avail}" in
	'' | *[!0-9]*) echo 0 ;;
	*) echo "${avail}" ;;
	esac
}

# Bump deliberately. `cargo install --locked --version` is what stops a
# rebuild from silently picking up a different CLI — same reasoning as the
# image digest and feature version pins in devcontainer.json.
NEAR_CLI_RS_VERSION="0.30.0"

# 1. Mark the workspace safe for git.
#
#    The workspace is a virtiofs bind mount from the host, and the uid it
#    reports through that mount is not stable: `stat` and `git` can disagree
#    seconds apart, and `git pull` fails with "detected dubious ownership"
#    while `git status` succeeds, because pull re-discovers the repository in
#    subprocesses that get their own attribute read. The files really are the
#    user's on the host, so this is a uid-mapping artefact rather than a
#    permissions problem worth respecting.
#
#    VS Code usually adds this itself, but ~/.gitconfig is regenerated on every
#    rebuild and is not on a volume, so it cannot be relied on. Idempotent:
#    plain `--add` would append a duplicate line on each re-run.
echo "==> Marking the workspace safe for git"
workspace="$(cd "$(dirname "$0")/.." && pwd)"
if ! git config --global --get-all safe.directory 2>/dev/null | grep -qxF "${workspace}"; then
	git config --global --add safe.directory "${workspace}"
fi

# 2. Warn if commit signing cannot work in here.
#
#    VS Code copies the host ~/.gitconfig in verbatim, so `user.signingkey`
#    still points at a path on the HOST (typically ~/.ssh/id_ed25519) that does
#    not exist in the container. With commit.gpgsign=true that fails closed —
#    `git commit` is refused outright rather than quietly producing unsigned
#    commits, which is the right way round but blocks committing until fixed.
#
#    The fix does NOT require mounting a private key into the container. VS
#    Code forwards the host ssh-agent (SSH_AUTH_SOCK), so once the signing key
#    is loaded there, ssh-keygen signs through the agent and the private key
#    never leaves the host:
#
#      # on the host, once
#      ssh-add --apple-use-keychain ~/.ssh/id_ed25519    # macOS; ssh-add on Linux
#      # in the container, point signingkey at the matching PUBLIC key
#      git config --global user.signingkey ~/.ssh/id_ed25519.pub
#
#    Only a warning: an unsigned-commit setup is a perfectly reasonable way to
#    use this container, and failing the create over it would be worse than the
#    problem.
if [ "$(git config --global --get commit.gpgsign 2>/dev/null || echo false)" = "true" ]; then
	signing_key="$(git config --global --get user.signingkey 2>/dev/null || true)"
	case "${signing_key}" in
	"~/"*) signing_key="${HOME}/${signing_key#\~/}" ;;
	esac
	if [ -n "${signing_key}" ] && [ ! -f "${signing_key}" ]; then
		warn "commit.gpgsign is on but user.signingkey points at '${signing_key}', which does not exist in this container, so 'git commit' will fail. Load the key into the host ssh-agent (ssh-add --apple-use-keychain ~/.ssh/id_ed25519) and set user.signingkey to the matching .pub inside the container — see the comment in .devcontainer/post-create.sh."
	elif [ -n "${signing_key}" ]; then
		# The key resolves, so signing will work. Verifying is a SEPARATE
		# setting: without gpg.ssh.allowedSignersFile, git reports every
		# signed commit as "N" (no signature) locally — `git log --show-
		# signature` even errors outright — while GitHub happily reports it
		# as verified. Generate the file from config that is already present
		# rather than asking anyone to hand-maintain it.
		email="$(git config --global --get user.email 2>/dev/null || true)"
		if [ -n "${email}" ]; then
			allowed="${HOME}/.ssh/allowed_signers"
			entry="${email} $(cut -d' ' -f1,2 "${signing_key}")"
			if [ ! -f "${allowed}" ] || ! grep -qxF "${entry}" "${allowed}"; then
				printf '%s\n' "${entry}" >> "${allowed}"
			fi
			git config --global gpg.ssh.allowedSignersFile "${allowed}"
		fi
	fi
fi

# 3. Toolchain — `rustup show` reads rust-toolchain.toml and installs the
#    pinned 1.97.0 toolchain plus rustfmt/clippy. Fatal if it fails; there is
#    no usable container without it.
echo "==> Installing the pinned Rust toolchain"
rustup show

# 4. near-cli-rs — docs/TESTNET.md's walkthrough opens with `near account
#    create-account ...`, so the CLI has to actually be here.
#
#    Compiled from source rather than installed from the project's prebuilt
#    release tarball (which is what templar-backend's post-create.sh does).
#    Those tarballs are built on Ubuntu 24.04 and their binaries need
#    GLIBC_2.39; this container is Debian bookworm (glibc 2.36) to match the
#    production Dockerfile's builder stage, so a prebuilt `near` would install
#    fine and then fail to execute. Every published near-cli-rs release has
#    that same floor, so pinning an older one is not a way out — building is.
#    Costs roughly three minutes on a container rebuild.
#
#    libudev-dev is a build-time requirement of the default `ledger` feature
#    (near-ledger -> hidapi), and is not on the bookworm base image. The
#    default feature set is kept rather than dropping `ledger` so this `near`
#    behaves identically to the released binary the docs link to; a Ledger
#    isn't reachable from inside a container without USB passthrough, so the
#    feature is inert here either way.
#
#    near-cli-rs 0.30.0 declares rust-version 1.93, which the pinned 1.97.0
#    satisfies. Re-check when bumping: a near-cli-rs whose MSRV passes 1.97.0
#    would need rust-toolchain.toml moved first, and that pin is itself tied
#    to Cargo.toml's rust-version and the Dockerfile builder digest.
if [ "$(near --version 2>/dev/null | awk '{ print $2 }')" = "${NEAR_CLI_RS_VERSION}" ]; then
	echo "==> near-cli-rs ${NEAR_CLI_RS_VERSION} already installed"
else
#    Build parallelism is capped on purpose. cargo defaults to one rustc job
#    per core (14 on an M-series host), and near-primitives alone peaks well
#    past a gigabyte, so the default fans out to far more memory than a
#    Docker Desktop VM has: the build dies with `signal: 9, SIGKILL` from the
#    OOM killer and — because this step is non-fatal — leaves you with a
#    container that came up fine but has no `near`. Same failure mode and same
#    remedy as the sandbox test's link step (see CLAUDE.md's Gotchas).
#
#    Budget ~1.5 GB per job against MemAvailable rather than hardcoding a
#    number, so a roomy machine still builds fast and a squeezed one still
#    finishes. CARGO_BUILD_JOBS in the environment wins if you set it.
	echo "==> Installing near-cli-rs ${NEAR_CLI_RS_VERSION} (compiles from source; slow)"
	avail_kb=$(available_kb)
	if [ -z "${CARGO_BUILD_JOBS:-}" ]; then
		CARGO_BUILD_JOBS=$(( avail_kb / 1500000 ))
		[ "${CARGO_BUILD_JOBS}" -lt 1 ] && CARGO_BUILD_JOBS=1
		[ "${CARGO_BUILD_JOBS}" -gt "$(nproc)" ] && CARGO_BUILD_JOBS=$(nproc)
	fi
	export CARGO_BUILD_JOBS
	echo "    using ${CARGO_BUILD_JOBS} build job(s) ($(( avail_kb / 1024 )) MB available, $(nproc) cores)"

	if sudo apt-get update -qq && sudo apt-get install -y -qq --no-install-recommends libudev-dev; then
		# debug=0 drops debuginfo generation, which is most of rustc's peak
		# memory here; near-cli-rs already strips it at link, so this costs
		# nothing but backtrace line numbers in a tool we only run by hand.
		#
		# RELEASE, not DEV/TEST: `cargo install` builds in the release profile,
		# so the dev/test knobs used for `cargo test` do nothing here. The retry
		# hint below carries both settings for the same reason — a retry without
		# them just reproduces the original OOM.
		CARGO_PROFILE_RELEASE_DEBUG=0 \
			cargo install near-cli-rs --locked --version "${NEAR_CLI_RS_VERSION}" || warn \
			"near-cli-rs install failed. Nothing in the build depends on it — only the docs/TESTNET.md walkthrough does. Retry with: CARGO_BUILD_JOBS=1 CARGO_PROFILE_RELEASE_DEBUG=0 cargo install near-cli-rs --locked --version ${NEAR_CLI_RS_VERSION}"
	else
		warn "Could not install libudev-dev; skipping near-cli-rs. See docs/TESTNET.md."
	fi
fi

# 5. Claude Code — the CLI itself is disposable and reinstalled on each
#    rebuild, but its state is not: ~/.claude is a named volume (see
#    devcontainer.json) holding session transcripts, config and credentials.
#
#    Docker creates a fresh volume root owned by root, so it has to be chowned
#    before the CLI can write to it. ~/.claude.json sits *outside* that
#    directory, so it is moved in and symlinked back — otherwise it is the one
#    piece of state that would still be lost on rebuild.
echo "==> Setting up Claude Code"
sudo chown -R "$(id -u):$(id -g)" "${HOME}/.claude" || warn "could not chown ${HOME}/.claude"

#
#    Each step below warns rather than aborting. The chown above is the one
#    most likely to fail (a volume the daemon hands over root-owned), and it
#    only warns — so leaving these unguarded under `set -e` would turn that
#    warning into a failed create two lines later, which is exactly the
#    outcome the non-fatal contract at the top of this file promises not to.
if [ -f "${HOME}/.claude.json" ] && [ ! -L "${HOME}/.claude.json" ]; then
	mv "${HOME}/.claude.json" "${HOME}/.claude/.claude.json" ||
		warn "could not migrate ${HOME}/.claude.json into the volume; it will not persist across rebuilds"
fi
if [ ! -e "${HOME}/.claude/.claude.json" ]; then
	echo '{}' > "${HOME}/.claude/.claude.json" ||
		warn "could not seed ${HOME}/.claude/.claude.json"
fi
ln -sfn "${HOME}/.claude/.claude.json" "${HOME}/.claude.json" ||
	warn "could not link ${HOME}/.claude.json into the volume; config will not persist across rebuilds"

#    Installed WITHOUT sudo, deliberately. The node feature puts node/npm under
#    /usr/local/share/nvm, which is writable by this user but is not on root's
#    sudo secure_path — `sudo npm install -g` fails with "npm: command not
#    found". (templar-backend can use sudo there because it installs Node from
#    the NodeSource apt repo into /usr/bin instead.)
if command -v claude >/dev/null 2>&1; then
	echo "==> Claude Code already installed ($(claude --version 2>/dev/null || echo unknown))"
else
	npm install -g --no-fund --loglevel=error @anthropic-ai/claude-code || warn \
		"Claude Code install failed. Nothing in the build depends on it. Retry with: npm install -g @anthropic-ai/claude-code"
fi

# 6. Warm the dependency cache, including the git checkout of the pinned
#    templar-* rev — worth doing up front since that fetch needs network and
#    would otherwise happen lazily on the first build/test/clippy run.
echo "==> Warming the cargo dependency cache"
cargo fetch || warn "cargo fetch failed; it will run again on your first build."
