#!/usr/bin/env bash
# Shared settings for the site verbs; sourced, not run.

POMB_KEY="${POMB_KEY:-$HOME/.ssh/pomb_app}"
POMB_SSH_OPTS=(-i "$POMB_KEY" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=15 -o LogLevel=ERROR)

pomb_pass_line() {
  local entry="$1" value
  if ! value="$(pass show "$entry" 2>/dev/null)" || [[ -z "$value" ]]; then
    echo "site: the password store entry $entry is missing or empty" >&2
    return 1
  fi
  printf '%s\n' "${value%%$'\n'*}"
}

pomb_load_host() {
  [[ -n "${POMB_HOST:-}" ]] && return 0
  POMB_HOST="$(pomb_pass_line Agents/Linode/pomb.app-host)" || exit 1
  if [[ ! "$POMB_HOST" =~ ^[A-Za-z0-9_.-]+@[^@[:space:]]+$ ]]; then
    echo "site: the password store entry Agents/Linode/pomb.app-host is not user@host" >&2
    exit 1
  fi
}

pomb_ssh() {
  pomb_load_host

  # shellcheck disable=SC2029
  ssh "${POMB_SSH_OPTS[@]}" "$POMB_HOST" "$@"
}

pomb_site_dir() {
  cd "$(dirname "${BASH_SOURCE[0]}")" && pwd
}
