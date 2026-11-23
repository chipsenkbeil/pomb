#!/usr/bin/env bash
# summary: Show service states, current releases, last update results and disk use
# remote: false
# runs_local
# positional
set -eu

script=$DEV_ROOT/.dev/helpers/site_status.sh
entry_name=Agents/Linode/pomb.app-host
tunnel_host=ssh.pomb.app

host=${POMB_HOST:-}
if [ -z "$host" ]; then
  entry=$(pass show "$entry_name" 2>/dev/null | head -n 1 || true)
  if [ -z "$entry" ]; then
    echo "The password store entry $entry_name is missing or empty." >&2
    exit 1
  fi
  if [[ ! $entry =~ ^([A-Za-z0-9_.-]+)@[^@[:space:]]+$ ]]; then
    echo "The password store entry $entry_name is not user@host." >&2
    exit 1
  fi
  host=${BASH_REMATCH[1]}@$tunnel_host
fi

key=${POMB_KEY:-$HOME/.ssh/pomb_app}
exec ssh -i "$key" -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=15 \
  -o LogLevel=ERROR "$host" bash -s <"$script"
