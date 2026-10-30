#!/usr/bin/env bash
# Check a channel after an update: dev site verify [<stable|nightly> [<expected>]] [--wait]
# local
# flags: --wait
set -euo pipefail

# shellcheck source-path=SCRIPTDIR source=__common.sh
source "$(dirname "$0")/__common.sh"

repo="chipsenkbeil/pomb"
api="https://api.github.com/repos/$repo"
wait_limit_secs=1200
wait_step_secs=15
release_step_secs=60

usage() {
  echo "usage: dev site verify [<stable|nightly> [<expected>]] [--wait]" >&2
  exit 2
}

wait=0
args=()
for a in "$@"; do
  case "$a" in
    --wait) wait=1 ;;
    -*) usage ;;
    *) args+=("$a") ;;
  esac
done
((${#args[@]} <= 2)) || usage

channel="${args[0]:-}"
expected="${args[1]:-}"
case "$channel" in
  stable | nightly) ;;
  "")
    [[ -z "$expected" ]] || usage
    ;;
  *) usage ;;
esac

if [[ -z "$channel" ]]; then
  rc=0
  for ch in stable nightly; do
    echo "== $ch"
    if ((wait == 1)); then
      "$0" "$ch" --wait || rc=1
    else
      "$0" "$ch" || rc=1
    fi
  done
  exit "$rc"
fi

pomb_load_host
case "$channel" in
  stable) url="https://pomb.app" ;;
  nightly) url="https://nightly.pomb.app" ;;
esac

failures=0

pass() { printf 'PASS  %s\n' "$*"; }
note() { printf 'NOTE  %s\n' "$*"; }
fail() {
  printf 'FAIL  %s\n' "$*"
  failures=$((failures + 1))
}

fetch() {
  curl -fsS --retry 3 --max-time 60 -H 'Accept: application/vnd.github+json' "$1"
}

release_commit() {
  jq -r '.body // ""' <<<"$1" | sed -n 's/^[[:space:]]*commit:[[:space:]]*\([0-9a-f]\{7,40\}\).*/\1/p' | head -n 1
}

resolve() {
  local rel
  if [[ "$channel" == nightly ]]; then
    if [[ -z "$expected" ]]; then
      rel=$(fetch "$api/releases/tags/nightly") || return 1
      expected=$(release_commit "$rel")
    fi
    [[ "$expected" =~ ^[0-9a-f]{7,40}$ ]] || return 2
    commit="$expected"
    id="${commit:0:12}"
  else
    if [[ -z "$expected" ]]; then
      rel=$(fetch "$api/releases/latest") || return 1
      expected=$(jq -r '.tag_name // empty' <<<"$rel")
    else
      rel=$(fetch "$api/releases/tags/$expected") || return 1
    fi
    [[ -n "$expected" && "$expected" != */* && "$expected" != .* ]] || return 2
    id="$expected"
    commit=$(release_commit "$rel")
    if [[ -z "$commit" ]]; then
      commit=$(fetch "$api/commits/$id" | jq -r '.sha // empty') || commit=""
    fi
    [[ -n "$commit" ]] || return 1
  fi
}

public_sha() {
  curl -fsS --max-time 10 "$url/api/v1/version" 2>/dev/null | jq -r '.build_sha // empty' 2>/dev/null || true
}

same_commit() {
  [[ -n "$1" && ("$commit" == "$1"* || "$1" == "$commit"*) ]]
}

remote_script=$(
  cat <<'REMOTE'
set -u
unit="pomb-$CH.service"
invocation=$(systemctl show -p InvocationID --value "$unit")
echo "last-update $(cat "/var/lib/pomb/last-update-$CH" 2>/dev/null || echo never)"
if journalctl -u "$unit" --no-pager -q "_SYSTEMD_INVOCATION_ID=$invocation" | grep -q 'setup code'; then
  echo "setupcode present"
else
  echo "setupcode absent"
fi
echo "journal-lines $(journalctl -u "$unit" --no-pager -q "_SYSTEMD_INVOCATION_ID=$invocation" | wc -l)"
journalctl -u "$unit" --no-pager -q "_SYSTEMD_INVOCATION_ID=$invocation" | grep -Ei 'panic|error' | sed -E 's/setup code.*/setup code (hidden)/' | head -n 5 | sed 's/^/journal-bad /'
if find "/var/lib/pomb/$CH/backups" -maxdepth 1 -type f -name "pomb-*-before-$ID.redb" 2>/dev/null | grep -q .; then
  echo "backup present"
else
  echo "backup absent"
fi
if find "/var/lib/pomb/$CH/backups" -maxdepth 1 -type d -name "pomb-*-before-$ID.archives" 2>/dev/null | grep -q .; then
  echo "archives present"
else
  echo "archives absent"
fi
echo "releases $(find "/opt/pomb/$CH/releases" -mindepth 1 -maxdepth 1 -type d ! -name '.tmp-*' | wc -l)"
REMOTE
)

remote_report() {
  for _ in 1 2 3; do
    if printf '%s\n' "$remote_script" | pomb_ssh CH="$channel" ID="$id" bash -s 2>/dev/null; then
      return 0
    fi
    sleep 5
  done
  return 1
}

deadline=$(($(date +%s) + wait_limit_secs))
while :; do
  rc=0
  resolve || rc=$?
  ((rc != 0)) || break
  if ((rc == 2 || wait == 0)) || (($(date +%s) >= deadline)); then
    fail "cannot work out what $channel should run${expected:+ (expected $expected)}"
    exit 1
  fi
  echo "verify: no $channel release${expected:+ for $expected} yet, waiting"
  sleep "$release_step_secs"
done
echo "verify: $channel expects $id (commit ${commit:0:12})"

deadline=$(($(date +%s) + wait_limit_secs))
if ((wait == 1)); then
  while :; do
    got=$(public_sha)
    if same_commit "$got"; then
      break
    fi
    if (($(date +%s) >= deadline)); then
      echo "verify: gave up waiting after $wait_limit_secs seconds"
      break
    fi
    echo "verify: $channel runs ${got:0:12}, waiting for ${commit:0:12}"
    sleep "$wait_step_secs"
  done
fi

got=$(public_sha)
if same_commit "$got"; then
  pass "build_sha at $url is ${got:0:12}"
else
  fail "build_sha at $url is ${got:-unreachable}, wanted ${commit:0:12}"
fi

login=$(curl -sS --max-time 15 -o /dev/null -D - -w 'status=%{http_code}\n' "$url/login" 2>/dev/null | tr -d '\r' || true)
setup=$(curl -sS --max-time 15 -o /dev/null -D - -w 'status=%{http_code}\n' "$url/setup" 2>/dev/null | tr -d '\r' || true)
status_of() { sed -n 's/^status=//p' <<<"$1"; }
location_of() { sed -n 's/^[Ll]ocation: //p' <<<"$1" | head -n 1; }
login_status=$(status_of "$login")
setup_status=$(status_of "$setup")
if [[ "$login_status" == 200 && "$setup_status" == 303 && "$(location_of "$setup")" == "/" ]]; then
  pass "/login answers 200 and /setup redirects to /, an instance with a passkey"
elif [[ "$login_status" == 303 && "$(location_of "$login")" == "/setup" && "$setup_status" == 200 ]]; then
  pass "/login redirects to /setup and /setup answers 200, a fresh instance"
else
  fail "/login answered ${login_status:-nothing} and /setup ${setup_status:-nothing}"
fi

csp=$(sed -n 's/^[Cc]ontent-[Ss]ecurity-[Pp]olicy: //p' <<<"$login" | head -n 1)
if [[ "$csp" == *"default-src 'self'"* && "$csp" == *"script-src 'self'"* && "$csp" == *"frame-ancestors 'none'"* && "$csp" != *unsafe-eval* ]]; then
  pass "content-security-policy is strict"
else
  fail "content-security-policy is missing or loose"
fi
hsts=$(sed -n 's/^[Ss]trict-[Tt]ransport-[Ss]ecurity: //p' <<<"$login" | head -n 1)
if [[ "$hsts" =~ max-age=[1-9][0-9]* ]]; then
  pass "strict-transport-security is set ($hsts)"
else
  fail "strict-transport-security is missing"
fi

report_ok=0
while :; do
  report_ok=0
  if report=$(remote_report); then
    report_ok=1
  fi
  last=$(sed -n 's/^last-update //p' <<<"$report")
  if ((wait == 0 || report_ok == 0)) || [[ "$last" =~ ok:\ "$id"($|\ ) || "$last" == *"failed: $id "* ]] || (($(date +%s) >= deadline)); then
    break
  fi
  sleep "$wait_step_secs"
done

if ((report_ok == 1)); then
  last=$(sed -n 's/^last-update //p' <<<"$report")
  if [[ "$last" =~ ^[0-9TZ:-]+\ ok:\ "$id"($|\ ) ]]; then
    pass "last-update-$channel records ok for $id"
  else
    fail "last-update-$channel is: $last"
  fi

  bad=$(sed -n 's/^journal-bad //p' <<<"$report")
  lines=$(sed -n 's/^journal-lines //p' <<<"$report")
  if [[ -z "$bad" ]]; then
    pass "journal since the unit started has no panic or error lines ($lines lines)"
  else
    fail "journal since the unit started has panic or error lines"
    while IFS= read -r line; do printf '        %s\n' "$line"; done <<<"$bad"
  fi
  note "setup code in the journal: $(sed -n 's/^setupcode //p' <<<"$report")"

  releases=$(sed -n 's/^releases //p' <<<"$report")
  if [[ "$(sed -n 's/^backup //p' <<<"$report")" == present ]]; then
    pass "a pre-update backup named for $id exists"
    if [[ "$(sed -n 's/^archives //p' <<<"$report")" == present ]]; then
      pass "its board archive directory exists"
    else
      note "no board archive directory beside the backup for $id (the binary running before the update has no archive command before 0.5.30)"
    fi
  elif [[ "$releases" == 1 ]]; then
    note "no pre-update backup for $id, and it is the only release on the box: a first install"
  else
    fail "no pre-update backup named for $id"
  fi
else
  fail "cannot reach the box over ssh after three tries"
fi

if ((failures > 0)); then
  echo "verify: $channel FAILED ($failures)"
  exit 1
fi
echo "verify: $channel ok"
