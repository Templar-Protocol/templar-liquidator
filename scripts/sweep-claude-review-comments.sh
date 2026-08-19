#!/usr/bin/env bash
# sweep-claude-review-comments.sh <pr-number>
#
# Delete leftover claude-review tracking comments on a PR — the "Review in
# progress…" bodies a cancelled run leaves behind forever, because a cancelled
# run never reaches the action's finalizer. Called by claude-review.yml from
# the review job (before each review) and the janitor job (on PR close).
# Expects GH_TOKEN and GH_REPO in the environment.
#
# Deletion is positive-recognition only; anything unrecognised is KEPT:
#   - a body starting "**Claude" is one of the action's terminal formats
#     ("**Claude finished …", "**Claude encountered an error …") — kept
#   - a body linking a run of this workflow is deleted only when that run has
#     finished and was NOT successful (cancelled, failed, timed out). A
#     successful run's summary is kept even when its header is missing —
#     which happens (PR #721) — and a still-running run's comment is live,
#     not stale, so the racing-predecessor window cannot eat a real comment
#   - a body linking some other workflow's run is not ours — kept
#   - a body with no run link is deleted only when it carries the action's
#     progress spinner; a claude[bot] comment we cannot attribute is kept
# Every drift direction (new comment formats, reworded headers) therefore
# fails toward leaking a stale comment, never toward deleting a real one.
set -euo pipefail

pr="${1:?usage: $0 <pr-number>}"
workflow=".github/workflows/claude-review.yml"
# First path segment of the spinner image the action embeds in progress
# bodies. Spliced into the jq program below as '"${spinner_marker}"' — close
# quote, expand, reopen — so jq receives a plain contains("5ac382c7").
spinner_marker="5ac382c7"

gh api "repos/{owner}/{repo}/issues/${pr}/comments?per_page=100" --paginate \
  --jq '.[] | select(.user.login=="claude[bot]")
        | select((.body // "") | startswith("**Claude") | not)
        | [ .id,
            (((.body // "") | capture("actions/runs/(?<rid>[0-9]+)") | .rid)? // ""),
            ((.body // "") | contains("'"${spinner_marker}"'") | tostring) ]
        | @tsv' |
{
  fails=0
  while IFS=$'\t' read -r id rid spinner; do
    if [ -n "${rid}" ]; then
      run=$(gh api "repos/{owner}/{repo}/actions/runs/${rid}" \
        --jq '"\(.path) \(.status) \(.conclusion // "none")"' 2>/dev/null) || run=""
      read -r rpath rstatus rconclusion <<< "${run:-unknown unknown unknown}"
      if [ "${rpath}" != "${workflow}" ]; then
        echo "keep ${id}: linked run ${rid} is not this workflow's (${rpath})"
        continue
      fi
      if [ "${rstatus}" != "completed" ] || [ "${rconclusion}" = "success" ]; then
        echo "keep ${id}: linked run ${rid} is ${rstatus}/${rconclusion}"
        continue
      fi
    elif [ "${spinner}" != "true" ]; then
      echo "keep ${id}: no run link and no progress spinner — cannot attribute"
      continue
    fi
    if gh api -X DELETE "repos/{owner}/{repo}/issues/comments/${id}" >/dev/null; then
      echo "deleted stale tracking comment ${id}"
    else
      echo "::warning::could not delete stale comment ${id}"
      fails=$((fails + 1))
    fi
  done
  # A swept-but-not-deleted comment must fail the janitor rather than let it
  # report green with the zombie still up; the review job stays best-effort
  # through its continue-on-error.
  if [ "${fails}" -gt 0 ]; then
    echo "::error::${fails} stale comment(s) could not be deleted"
    exit 1
  fi
}
