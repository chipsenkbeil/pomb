#!/usr/bin/env bash
# Seed the "ChipFlow Visual Test" board, the small board behind the
# KanbanFlow timer, modal and card goldens (FQhhSs1).
#
# Usage: seed-visual-test.sh BASE_URL COOKIE_JAR
# Prints the new board id as the last line of stdout.
#
# Layout (the cards/07 golden): Personal To-do holds Task Alpha (green, due
# 2026-10-02 17:00), Task Beta, Task Delta, Task Epsilon and the long-title
# blue task (the modal goldens' task: markdown description, three subtasks);
# Do today holds Task Gamma; In progress has WIP limit 3; Done is
# the done column.
set -euo pipefail
base=${1:?base url} jar=${2:?cookie jar}

api() {
  local method=$1 path=$2 body=${3:-}
  local args=(-sS -f -X "$method" -b "$jar" -H 'Accept: application/json')
  [[ -z $body ]] || args+=(-H 'Content-Type: application/json' --data-binary "$body")
  curl "${args[@]}" "$base$path"
}

board=$(api POST /api/boards '{"name":"ChipFlow Visual Test"}' | jq -r .id)
todo=$(api GET "/api/boards/$board/columns" | jq -r '.[0].id')
api PATCH "/api/columns/$todo" '{"name":"Personal To-do","wip_limit":null}' >/dev/null
api POST /api/columns "{\"board_id\":\"$board\",\"name\":\"Do today\"}" >/dev/null
api POST /api/columns "{\"board_id\":\"$board\",\"name\":\"In progress\",\"wip_limit\":3}" >/dev/null
api POST /api/columns "{\"board_id\":\"$board\",\"name\":\"Done\"}" >/dev/null
cols=$(api GET "/api/boards/$board/columns")
today=$(jq -r '.[1].id' <<<"$cols")
done_col=$(jq -r '.[-1].id' <<<"$cols")
api PATCH "/api/columns/$done_col" '{"is_done":true}' >/dev/null
colors=$(api GET "/api/boards/$board/colors")

# Same eight-colour palette as the Golden Master board.
palette=(yellow green blue red orange purple magenta cyan)
while read -r extra; do
  api DELETE "/api/boards/$board/colors/$extra" >/dev/null
done < <(jq -r --argjson keep "$(printf '%s\n' "${palette[@]}" | jq -R . | jq -sc .)" \
  '.[] | select((.label | ascii_downcase) as $l | $keep | index($l) | not) | .id' <<<"$colors")
order=1
for name in "${palette[@]}"; do
  id=$(jq -r --arg l "$name" 'map(select(.label|ascii_downcase==$l))|first|.id' <<<"$colors")
  api PATCH "/api/boards/$board/colors/$id" "{\"sort_order\":$order}" >/dev/null
  order=$((order + 1))
done
color() { jq -r --arg l "$1" 'map(select(.label|ascii_downcase==$l))|first|.id' <<<"$colors"; }

mk() { # name color column [extra JSON object]
  local extra=${4:-}
  [[ -n $extra ]] || extra='{}'
  api POST /api/tasks "$(jq -nc --arg n "$1" --arg k "$(color "$2")" --arg c "$3" --argjson x "$extra" \
    '{name: $n, column_id: $c, color_id: $k} + $x')" | jq -r .id
}

alpha=$(mk "Task Alpha" green "$todo" '{"due_at":"2026-10-02 17:00"}')
api PATCH "/api/tasks/$alpha" "{\"due_target_column_id\":\"$done_col\"}" >/dev/null
mk "Task Beta" yellow "$todo" >/dev/null
mk "Task Delta" yellow "$todo" >/dev/null
mk "Task Epsilon" yellow "$todo" >/dev/null
long=$(mk "GM long title: Refactor the authentication middleware so it validates tokens, refreshes sessions and logs every failure for audit" blue "$todo")
api PATCH "/api/tasks/$long" "$(jq -nc '{description: "Golden master **bold** and *italic* text.\n\n- item one\n  - item two [link](https://example.com)"}')" >/dev/null
first=$(api POST "/api/tasks/$long/subtasks" '{"name":"First subtask"}' | jq -r .id)
api PATCH "/api/tasks/$long/subtasks/$first" '{"done":true}' >/dev/null
api POST "/api/tasks/$long/subtasks" '{"name":"Second subtask with a much longer name that should wrap onto another line in the modal"}' >/dev/null
api POST "/api/tasks/$long/subtasks" '{"name":"Third subtask"}' >/dev/null
mk "Task Gamma" yellow "$today" >/dev/null
printf '%s\n' "$board"
