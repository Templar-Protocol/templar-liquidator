#!/usr/bin/env bash
# git-signing.sh — make SSH commit signing work inside the container.
#
# Run from BOTH postCreateCommand and postStartCommand, deliberately. VS Code
# forwards the host ssh-agent, but SSH_AUTH_SOCK is not reliably present during
# container creation — it is set up around the connection. Running again on
# every start means signing is wired up as soon as the agent is reachable
# rather than only on the one rebuild where the timing happened to work.
# Idempotent and fast (a few milliseconds) once configured, so the extra call
# costs nothing.
#
# THE PROBLEM. VS Code copies the host ~/.gitconfig in verbatim, so
# user.signingkey points at a path on the HOST (~/.ssh/id_ed25519) that does
# not exist here, and ~/.gitconfig is regenerated on every rebuild so any
# manual fix is lost. With commit.gpgsign=true, `git commit` then fails
# outright:
#
#     error: No private key found for public key "..."
#     fatal: failed to write commit object
#
# THE FIX. The private key never needs to enter the container: ssh-keygen signs
# through the forwarded agent, given a public key to identify WHICH key. Two
# ways for user.signingkey to supply one, and only the second needs this script:
#
#   key::ssh-ed25519 AAAA... — a literal public key. No path to go stale, so
#     setting it once in the HOST's ~/.gitconfig covers every container and
#     every repo, including repos not shipping this script. Prefer it. Needs
#     git 2.35+, which added the key:: prefix; 2.34 accepted a literal only
#     when it began "ssh-". Use the key GitHub registered as an SSH SIGNING
#     key (`gh api users/<login>/ssh_signing_keys`), not whichever
#     `ssh-add -L` prints first; an unregistered one signs commits GitHub
#     shows Unverified.
#
#   A path — a HOST path, which does not exist here. Recovered below by
#     matching an agent key's comment against user.email. A comment is
#     arbitrary metadata, so this misses good keys commented user@hostname.
#
# Verifying is a separate setting from signing: without
# gpg.ssh.allowedSignersFile, git reports correctly-signed commits as "N" (no
# signature) locally while GitHub reports them verified. Written on both paths,
# when user.email is set to say who the key is trusted for.
#
# Never fatal. Working in this container without commit signing is a perfectly
# reasonable setup, and failing the create over it would be worse than the
# problem.
set -euo pipefail

note() { echo "    $*"; }
warn() { echo "!!  $*" >&2; }

# Record the signing key as trusted for user.email, so signatures verify
# locally. Takes a public key as "<type> <base64> [comment]"; the comment is
# dropped because allowed_signers matches on the key itself.
write_allowed_signers() {
	local allowed entry
	[ -n "${email}" ] || return 0
	allowed="${HOME}/.ssh/allowed_signers"
	entry="${email} $(printf '%s' "$1" | cut -d' ' -f1,2)"
	mkdir -p "${HOME}/.ssh"
	if [ ! -f "${allowed}" ] || ! grep -qxF "${entry}" "${allowed}"; then
		printf '%s\n' "${entry}" >> "${allowed}"
	fi
	git config --global gpg.ssh.allowedSignersFile "${allowed}"
}

[ "$(git config --global --get commit.gpgsign 2>/dev/null || echo false)" = "true" ] || exit 0
[ "$(git config --global --get gpg.format 2>/dev/null || true)" = "ssh" ] || exit 0

signing_key="$(git config --global --get user.signingkey 2>/dev/null || true)"
email="$(git config --global --get user.email 2>/dev/null || true)"

case "${signing_key}" in
"~/"*) signing_key="${HOME}/${signing_key#\~/}" ;;
esac

# A literal key needs no recovery — but it is not a file either, so without
# this branch the not-a-file test below would drag it through the recovery path
# and warn that `git commit` will fail while signing in fact works.
case "${signing_key}" in
key::*)
	if [ -z "${SSH_AUTH_SOCK:-}" ]; then
		warn "commit signing is on and user.signingkey is a literal key, but no ssh-agent is forwarded to hold its private half, so 'git commit' will fail. Enable agent forwarding, or unset commit.gpgsign for this container."
		exit 0
	fi
	write_allowed_signers "${signing_key#key::}"
	note "user.signingkey is a literal key; signing goes through the forwarded agent"
	exit 0
	;;
esac

# Recover a usable public key when the configured one is not present here.
if [ -n "${signing_key}" ] && [ ! -f "${signing_key}" ]; then
	if [ -z "${email}" ]; then
		warn "commit signing is on but user.email is unset, so the right agent key cannot be identified. 'git commit' will fail."
		exit 0
	fi
	if [ -z "${SSH_AUTH_SOCK:-}" ]; then
		warn "commit signing is on but user.signingkey ('${signing_key}') does not exist here and no ssh-agent is forwarded, so 'git commit' will fail. Enable agent forwarding, or unset commit.gpgsign for this container."
		exit 0
	fi

	# `ssh-add -L` exits non-zero when the agent holds no identities (2 when
	# the socket is unusable), so it must be captured separately with `|| true`
	# rather than piped directly: under `set -euo pipefail` a pipeline whose
	# first stage fails takes the whole script down, and this one promises
	# never to be fatal.
	agent_keys="$(ssh-add -L 2>/dev/null || true)"

	# Match on the key COMMENT equal to user.email. Comments can contain
	# spaces, so rebuild the tail of the line rather than taking $3.
	match="$(printf '%s\n' "${agent_keys}" |
		awk -v e="${email}" '{ c = ""; for (i = 3; i <= NF; i++) c = c (i > 3 ? " " : "") $i; if (c == e) { print $1 " " $2 " " c; exit } }')"

	if [ -z "${match}" ]; then
		warn "commit signing is on but no forwarded agent key has the comment '${email}', so 'git commit' will fail. On the host: ssh-add --apple-use-keychain ~/.ssh/id_ed25519 (macOS) or ssh-add ~/.ssh/id_ed25519."
		exit 0
	fi

	base="$(basename "${signing_key}")"
	recovered="${HOME}/.ssh/${base%.pub}.pub"
	mkdir -p "${HOME}/.ssh"
	printf '%s\n' "${match}" > "${recovered}"
	chmod 644 "${recovered}"
	git config --global user.signingkey "${recovered}"
	signing_key="${recovered}"
	note "recovered signing key from the forwarded agent -> ${recovered}"
fi

[ -n "${signing_key}" ] && [ -f "${signing_key}" ] || exit 0

write_allowed_signers "$(cat "${signing_key}")"
