#!/usr/bin/env bash
# Start a ChipFlow binary under a frozen server clock, create the admin user,
# seed the two parity boards, and write WORKDIR/state.json for run.mjs and
# regress.mjs.
#
# Usage: start-server.sh BINARY PORT WORKDIR
#
#   BINARY   release or debug chipflow executable
#   PORT     free TCP port (127.0.0.1 only)
#   WORKDIR  scratch directory for the database, cookie jar, log, state.json
#
# The Golden Master board is seeded with the clock frozen at PARITY_SEED_TIME
# (the fixture's capture day). The server then restarts on the same database
# with the clock frozen at PARITY_SERVE_TIME (the day the goldens were
# captured), so Done groups and overdue counts match the goldens, and the
# Visual Test board is seeded on that day.
#
# Env: PARITY_PASSWORD (default parity-pass-1234), PARITY_SEED_TIME (default
# "2026-09-30 15:22:00"), PARITY_SERVE_TIME (default "2026-10-16 18:00:00"),
# PARITY_TZ (default America/Chicago), FAKETIME_LIB. Needs libfaketime
# (brew install libfaketime). Stop the server with
# `kill $(jq -r .pid WORKDIR/state.json)`.
set -euo pipefail

bin=${1:?binary} port=${2:?port} work=${3:?workdir}
here=$(cd "$(dirname "$0")" && pwd)
tools=$here/../tools
pw=${PARITY_PASSWORD:-parity-pass-1234}
seed_time=${PARITY_SEED_TIME:-2026-09-30 15:22:00}
serve_time=${PARITY_SERVE_TIME:-2026-10-16 18:00:00}
tz=${PARITY_TZ:-America/Chicago}
lib=${FAKETIME_LIB:-/opt/homebrew/lib/faketime/libfaketime.1.dylib}
user="Chipflow Seed"
base=http://127.0.0.1:$port

mkdir -p "$work"
db=$work/parity-$port.redb jar=$work/jar-$port.txt ftfile=$work/ft-$port.txt
rm -f "$db" "$jar"

# The server clock is frozen at the wall time in $ftfile (absolute, server-local)
# and moves only when run.mjs rewrites the file. FAKETIME_CACHE_DURATION=0
# re-reads the file on every call; FAKETIME_NO_CACHE=1 returned real time on
# some threads, so it is not used.
start_server() {
  printf '%s' "$1" >"$ftfile"
  nohup env TZ="$tz" FAKETIME_TIMESTAMP_FILE="$ftfile" FAKETIME_CACHE_DURATION=0 \
    FAKETIME_DONT_FAKE_MONOTONIC=1 DYLD_INSERT_LIBRARIES="$lib" \
    DYLD_FORCE_FLAT_NAMESPACE=1 \
    "$bin" --host 127.0.0.1 --port "$port" --database-path "$db" \
    >>"$work/server-$port.log" 2>&1 &
  pid=$!
  for _ in $(seq 1 100); do
    curl -s -o /dev/null "$base/login" && return
    sleep 0.2
  done
  echo "server did not start" >&2
  exit 1
}

api() { curl -sS -f -b "$jar" -H 'Accept: application/json' -H 'Content-Type: application/json' -X "$1" --data-binary "$3" "$base$2"; }

: >"$work/server-$port.log"
start_server "$seed_time"
curl -s -o /dev/null --data-urlencode username="$user" \
  --data-urlencode password="$pw" --data-urlencode confirm="$pw" "$base/setup"
curl -s -o /dev/null -c "$jar" --data-urlencode user="$user" \
  --data-urlencode pass="$pw" "$base/login"

gm=$(bash "$tools/seed-golden-master.sh" --as-of 2026-09-30 "$base" "$jar" 2>"$work/seed-gm.err" | tail -1)

# The golden board shows inline subtasks on every column and the colour legend.
for col in $(curl -sS -b "$jar" "$base/api/boards/$gm/columns" | jq -r '.[].id'); do
  api PATCH "/api/columns/$col" '{"config_json":"{\"prop_subtasks\":\"show\"}"}' >/dev/null
done
api PUT "/api/boards/$gm/config" '{"legend_visible":true}' >/dev/null

kill "$pid"
wait "$pid" 2>/dev/null || true
start_server "$serve_time"
vt=$(bash "$here/seed-visual-test.sh" "$base" "$jar" 2>"$work/seed-vt.err" | tail -1)

date_header=$(curl -sI "$base/login" | rg -i '^date:' | tr -d '\r')
jq -n --arg base "$base" --arg jar "$jar" --arg gm "$gm" --arg vt "$vt" \
  --arg ft "$serve_time" --arg tz "$tz" --argjson pid "$pid" --arg date "$date_header" \
  --arg ftfile "$ftfile" \
  '{base: $base, jar: $jar, boards: {gm: $gm, vt: $vt}, faketime: $ft, ftfile: $ftfile,
    tz: $tz, pid: $pid, serverDate: $date}' >"$work/state.json"
cat "$work/state.json"
