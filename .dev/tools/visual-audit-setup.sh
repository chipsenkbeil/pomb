#!/bin/bash
# Visual audit prerequisite: start test instance on :3200 with a fresh DB,
# create admin user, log in, create a board, and write /tmp/cf-cookies2.txt
# and /tmp/cf-bid.txt for chipflow-visual-audit.sh.
# Password is generated at runtime (never pass a literal in a tool call).
#
# Handoff copy (2026-10-16): the only change from the original on Dale's VM
# is the repo location — it defaults to ~/workspace/pomodoro-kanban and can
# be overridden with CHIPFLOW_REPO (e.g. CHIPFLOW_REPO=~/projects/chipflow).
# CAUTION: this starts a LOCAL test instance; the pkill below matches any
# process named chipflow, so never run it on a host serving production.
set -e

REPO_DIR="${CHIPFLOW_REPO:-$HOME/workspace/pomodoro-kanban}"
BIN="$REPO_DIR/target/release/chipflow"
DB="/tmp/chipflow-audit-test.redb"

# Stop any stale test instance on :3200 first (comm-name match only).
pkill -x chipflow 2>/dev/null || true
sleep 1

rm -f "$DB"
"$BIN" --host 127.0.0.1 --port 3200 --database-path "$DB" \
  > /tmp/chipflow-audit-server.log 2>&1 &
echo $! > /tmp/chipflow-audit-server.pid

# Wait for the version endpoint (up to 30s).
for i in $(seq 1 30); do
  if curl -s http://127.0.0.1:3200/api/v1/version > /dev/null; then break; fi
  sleep 1
done
curl -s http://127.0.0.1:3200/api/v1/version || { echo "server failed to start"; cat /tmp/chipflow-audit-server.log; exit 1; }
echo

PW=$(head -c 12 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9' | head -c 12)
CODE=$(curl -s -o /dev/null -w "%{http_code}" \
  --data-urlencode "username=audit" \
  --data-urlencode "password=$PW" \
  --data-urlencode "confirm=$PW" \
  http://127.0.0.1:3200/setup)
echo "setup -> HTTP $CODE"

CODE=$(curl -s -c /tmp/cf-cookies2.txt -o /dev/null -w "%{http_code}" \
  --data-urlencode "user=audit" \
  --data-urlencode "pass=$PW" \
  http://127.0.0.1:3200/login)
echo "login -> HTTP $CODE"
grep -q "session" /tmp/cf-cookies2.txt || { echo "no session cookie saved"; exit 1; }

BID=$(curl -s -b /tmp/cf-cookies2.txt -H 'Content-Type: application/json' \
  -d '{"name":"Audit Board"}' http://127.0.0.1:3200/api/boards \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
echo "bid=$BID" > /tmp/cf-bid.txt
echo "board id: $BID"
echo "setup complete"
