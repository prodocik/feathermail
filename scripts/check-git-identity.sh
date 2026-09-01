#!/usr/bin/env bash
# D68 + D69: the only git identity in this repo is the owner.
# Author and committer must be prodocik <prodocik@gmail.com>.
# No Claude, no other humans, no Co-authored-by. See docs/plan.md.
set -euo pipefail

ALLOWED_NAME='prodocik'
ALLOWED_EMAIL='prodocik@gmail.com'
ALLOWED="$ALLOWED_NAME <$ALLOWED_EMAIL>"
FORBIDDEN_MODEL_RE='claude|anthropic|claude\.ai|cursoragent|copilot|chatgpt|openai|gemini|devin|noreply@.*anthropic'

fail() {
  echo "D68/D69: git identity rejected: $*" >&2
  echo "This repository has one contributor: $ALLOWED" >&2
  echo "Not Claude, not another person, no Co-authored-by trailers." >&2
  exit 1
}

parse_name() {
  echo "$1" | sed -E 's/[[:space:]]+<[^>]+>.*$//' | sed -E 's/[[:space:]]+[0-9]+[[:space:]]+[+-][0-9]+$//'
}

parse_email() {
  echo "$1" | sed -nE 's/.*<([^>]+)>.*/\1/p' | tr '[:upper:]' '[:lower:]'
}

check_ident() {
  local label="$1"
  local value="$2"
  local name email
  name=$(parse_name "$value")
  email=$(parse_email "$value")
  if echo "$value" | grep -Eiq "$FORBIDDEN_MODEL_RE"; then
    fail "$label names a model: $value"
  fi
  if [ "$name" != "$ALLOWED_NAME" ] || [ "$email" != "$ALLOWED_EMAIL" ]; then
    fail "$label is '$value' (required: $ALLOWED)"
  fi
}

check_message() {
  local msg="$1"
  if echo "$msg" | grep -Eiq '^[[:space:]]*(co-authored-by|generated-by|assisted-by):'; then
    fail "commit message has a co-author / generated-by trailer"
  fi
  if echo "$msg" | grep -Eiq 'generated with (claude|cursor|chatgpt|copilot)'; then
    fail "commit message credits a model as author"
  fi
}

check_commit() {
  local sha="$1"
  check_ident "author of $sha" "$(git log -1 --format='%an <%ae>' "$sha")"
  check_ident "committer of $sha" "$(git log -1 --format='%cn <%ce>' "$sha")"
  check_message "$(git log -1 --format='%B' "$sha")"
}

mode="${1:-ident}"

case "$mode" in
  ident)
    check_ident "GIT_AUTHOR" "$(git var GIT_AUTHOR_IDENT)"
    check_ident "GIT_COMMITTER" "$(git var GIT_COMMITTER_IDENT)"
    ;;
  msg)
    file="${2:?commit message file}"
    check_ident "GIT_AUTHOR" "$(git var GIT_AUTHOR_IDENT)"
    check_ident "GIT_COMMITTER" "$(git var GIT_COMMITTER_IDENT)"
    check_message "$(cat "$file")"
    ;;
  range)
    range="${2:?rev range}"
    while IFS= read -r sha; do
      [ -n "$sha" ] || continue
      check_commit "$sha"
    done < <(git rev-list "$range")
    ;;
  all)
    while IFS= read -r sha; do
      check_commit "$sha"
    done < <(git rev-list --all)
    ;;
  *)
    echo "usage: $0 ident|msg <file>|range <rev>|all" >&2
    exit 2
    ;;
esac
