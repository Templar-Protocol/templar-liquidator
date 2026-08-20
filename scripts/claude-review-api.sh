#!/usr/bin/env bash
# claude-review-api.sh — the ONLY GitHub API surface allowed to the model
# inside the claude-review workflow: a dedupe read of the PR's inline
# comments, and a COMMENT-review submission to that same PR.
#
#   claude-review-api.sh comments
#   claude-review-api.sh submit <payload.json>
#
# GH_REPO and PR_NUMBER come from the workflow environment, never from the
# caller — and the permission layer rejects env-prefixed commands as compound
# operations — so a prompt-injected model cannot point this at another PR,
# another repo, or another route. The submit payload is rebuilt field by
# field with event forced to COMMENT, so nothing else (an APPROVE, say, or an
# unexpected API field) survives into the request.
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
    jq '{
          commit_id,
          event: "COMMENT",
          body,
          comments: [ (.comments // [])[]
                      | { path, line, side, start_line, start_side, body }
                      | with_entries(select(.value != null)) ]
        }
        | with_entries(select(.value != null))' \
      "${payload}" > "${payload}.sanitized"
    exec gh api "repos/${GH_REPO}/pulls/${PR_NUMBER}/reviews" \
      --method POST --input "${payload}.sanitized"
    ;;
  *)
    echo "usage: $0 comments | submit <payload.json>" >&2
    exit 64
    ;;
esac
