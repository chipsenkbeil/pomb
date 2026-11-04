#!/usr/bin/env bash
# Seed the "Pomb Epic Test" board, the small board behind the epic card and
# Epics panel regress entries.
#
# Usage: seed-epic.sh BASE_URL COOKIE_JAR
# Prints the new board id as the last line of stdout.
#
# Layout: To do holds the epic "Epic Launch" (size 2, so its final-check chip
# reads 2, one final-check subtask, the highlighted label "hot") and the open
# child "Write the guide"; Done holds the finished child "Draft the outline".
set -euo pipefail
base=${1:?base url} jar=${2:?cookie jar}

api() {
  local method=$1 path=$2 body=${3:-}
  local args=(-sS -f -X "$method" -b "$jar" -H 'Accept: application/json')
  [[ -z $body ]] || args+=(-H 'Content-Type: application/json' --data-binary "$body")
  curl "${args[@]}" "$base$path"
}

board=$(api POST /api/boards '{"name":"Pomb Epic Test"}' | jq -r .id)
cols=$(api GET "/api/boards/$board/columns")
todo=$(jq -r '.[0].id' <<<"$cols")
done_col=$(jq -r '.[-1].id' <<<"$cols")
api PATCH "/api/columns/$done_col" '{"is_done":true}' >/dev/null
api PUT "/api/boards/$board/label-settings" '{"highlights":[{"label":"hot","color":"#b8461f"}]}' >/dev/null

mk() { # name column [extra JSON object]
  api POST /api/tasks "$(jq -nc --arg n "$1" --arg c "$2" --argjson x "${3:-{\}}" \
    '{name: $n, column_id: $c} + $x')" | jq -r .id
}

epic=$(mk "Epic Launch" "$todo" '{"kind":"epic","size":2,"labels":["hot"]}')
api POST "/api/tasks/$epic/subtasks" '{"name":"Try it live"}' >/dev/null
open=$(mk "Write the guide" "$todo")
finished=$(mk "Draft the outline" "$done_col")
for child in "$open" "$finished"; do
  api POST "/api/tasks/$child/links" "$(jq -nc --arg e "$epic" '{kind: "parent", target_id: $e}')" >/dev/null
done
mk "Plain card" "$todo" >/dev/null
printf '%s\n' "$board"
