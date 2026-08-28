#!/usr/bin/env bash
# claude-review-api.sh — the ONLY GitHub API surface allowed to the model
# inside the claude-review workflow: a dedupe read of the PR's inline
# comments, and a COMMENT-review submission to that same PR.
#
#   claude-review-api.sh comments
#   claude-review-api.sh submit <payload.json>
#
# GH_REPO, PR_NUMBER and GITHUB_WORKSPACE come from the workflow environment,
# never from the caller — and the permission layer rejects env-prefixed
# commands as compound operations — so a prompt-injected model cannot point
# this at another PR, another repo, or another route. The submit payload is
# rebuilt field by field with event forced to COMMENT, so nothing else (an
# APPROVE, say, or an unexpected API field) survives into the request.
#
# The one thing the caller does choose is the payload path, and the model
# reads attacker-controlled PR content before choosing it — so `submit`
# treats that path as untrusted: it must resolve inside the workspace, and
# the sanitized copy is written to a path this script picks rather than one
# derived from the argument. Without both halves the shell's redirect creates
# a file wherever the argument points, before jq ever runs.
set -euo pipefail

: "${GH_REPO:?GH_REPO must be set}"
: "${PR_NUMBER:?PR_NUMBER must be set}"

case "${1:-}" in
  comments)
    exec gh api "repos/${GH_REPO}/pulls/${PR_NUMBER}/comments?per_page=100" \
      --paginate --jq '.[].body[0:500]'
    ;;
  submit)
    payload="${2:?usage: $0 submit <payload.json>}"
    : "${GITHUB_WORKSPACE:?GITHUB_WORKSPACE must be set}"

    # -m so a non-existent path still resolves (reported below as not found
    # rather than as an escape), and because it resolves symlinks a link
    # planted inside the workspace cannot point the write outside it.
    resolved=$(realpath -m -- "${payload}")
    workspace=$(realpath -m -- "${GITHUB_WORKSPACE}")
    case "${resolved}" in
      "${workspace}"/*) ;;
      *)
        echo "refusing a payload outside the workspace: ${resolved}" >&2
        exit 65
        ;;
    esac
    [ -f "${resolved}" ] || { echo "payload not found: ${resolved}" >&2; exit 66; }

    # Our path, not the caller's: deriving it from the argument is what let a
    # write land anywhere the argument pointed.
    sanitized="${RUNNER_TEMP:-/tmp}/claude-review-payload.sanitized.json"
    jq '{
          commit_id,
          event: "COMMENT",
          body,
          comments: [ (.comments // [])[]
                      | { path, line, side, start_line, start_side, body }
                      | with_entries(select(.value != null)) ]
        }
        | with_entries(select(.value != null))' \
      "${resolved}" > "${sanitized}"
    exec gh api "repos/${GH_REPO}/pulls/${PR_NUMBER}/reviews" \
      --method POST --input "${sanitized}"
    ;;
  *)
    echo "usage: $0 comments | submit <payload.json>" >&2
    exit 64
    ;;
esac
