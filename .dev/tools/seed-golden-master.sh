#!/usr/bin/env bash
# Reproduce the KanbanFlow "Golden Master" fixture board on a ChipFlow instance.
#
# Usage: seed-golden-master.sh [--as-of YYYY-MM-DD] BASE_URL COOKIE_JAR [FIXTURE_JSON]
#
#   BASE_URL      e.g. http://127.0.0.1:3230 (never production)
#   COOKIE_JAR    curl cookie jar holding a session from POST /login
#   FIXTURE_JSON  defaults to .dev/golden-master-board.json in this repo
#
# Creates a new board (existing boards are untouched) and prints its id as
# the last line of stdout. Progress and the list of fixture fields the API
# cannot reproduce go to stderr.
#
# Relative dates: the fixture was captured on 2026-09-30, and its due dates
# encode "due today" (2026-09-30T17:00), "overdue" (2026-09-25T17:00) and
# "due this week" (2026-10-02T17:00). Every fixture date is shifted by the
# whole number of days between 2026-09-30 and --as-of (default: today), so
# each task keeps its distance from "today". Times of day are kept and are
# read in the server's local timezone. The weekday shifts with the date, so
# the card text matches the reference ("Friday 5:00 PM") only when --as-of
# falls on a Wednesday. Pass --as-of 2026-09-30 to seed the literal dates.
#
# Mapping decisions:
#   - The last fixture column becomes the Done column (KanbanFlow treats its
#     rightmost column as Done). Its tasks are created in the first column
#     and moved in, so ChipFlow stamps a completion time and groups them
#     under "Today".
#   - Columns, colors, and members are resolved by name from API responses;
#     no ids are hard-coded.
#   - Board colors absent from the fixture palette are deleted.
#   - An assignee with no matching username falls back to the only
#     registered user (ChipFlow is single-admin).
#   - logged_hours becomes one manual time entry starting 09:00 on the day
#     before --as-of. The fixture gives no log date, the entry must lie in
#     the past, and POST /api/tasks/{id}/time would stamp it from now into
#     the future.
set -euo pipefail

readonly CAPTURE_DATE=2026-09-30

die() {
  printf 'seed-golden-master: %s\n' "$*" >&2
  exit 1
}

log() {
  printf 'seed-golden-master: %s\n' "$*" >&2
}

usage() {
  sed -n '4p' "$0" | sed 's/^# //' >&2
  exit 2
}

as_of=$(date +%F)
positional=()
while (($# > 0)); do
  case $1 in
    --as-of)
      (($# >= 2)) || usage
      as_of=$2
      shift 2
      ;;
    --as-of=*)
      as_of=${1#--as-of=}
      shift
      ;;
    -h | --help) usage ;;
    *)
      positional+=("$1")
      shift
      ;;
  esac
done
((${#positional[@]} >= 2 && ${#positional[@]} <= 3)) || usage

base_url=${positional[0]%/}
cookie_jar=${positional[1]}
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
fixture=${positional[2]:-$script_dir/../golden-master-board.json}

[[ $as_of =~ ^[0-9]{4}-[0-9]{2}-[0-9]{2}$ ]] || die "--as-of must be YYYY-MM-DD, got '$as_of'"
[[ -r $cookie_jar ]] || die "cookie jar not readable: $cookie_jar"
[[ -r $fixture ]] || die "fixture not readable: $fixture"
jq -e '.board_name and (.columns | length > 0) and (.tasks | type == "array")' \
  "$fixture" >/dev/null || die "fixture lacks board_name/columns/tasks: $fixture"

# Day offset between the capture date and --as-of, computed in UTC.
shift_days=$(jq -n --arg a "$as_of" --arg c "$CAPTURE_DATE" \
  '(($a | strptime("%Y-%m-%d") | mktime) - ($c | strptime("%Y-%m-%d") | mktime)) / 86400 | floor')
log "as-of $as_of: shifting fixture dates by $shift_days day(s)"

# Call the API with the session cookie; print the body, fail on non-2xx.
api() {
  local method=$1 path=$2 body=${3:-}
  local args=(-sS -X "$method" -b "$cookie_jar" -H 'Accept: application/json'
    -w '\n%{http_code}')
  if [[ -n $body ]]; then
    args+=(-H 'Content-Type: application/json' --data-binary "$body")
  fi
  local out code
  out=$(curl "${args[@]}" "$base_url$path") || die "$method $path: curl failed"
  code=${out##*$'\n'}
  out=${out%$'\n'*}
  [[ $code == 2?? ]] || die "$method $path -> HTTP $code: $out"
  printf '%s' "$out"
}

# Shift a fixture date ("YYYY-MM-DD" or "YYYY-MM-DDTHH:MM") by shift_days;
# returns the form ChipFlow reads in server-local time.
shift_due() {
  jq -nr --arg d "$1" --argjson n "$shift_days" '
    ($d | split("T")) as [$day, $time]
    | (($day | strptime("%Y-%m-%d") | mktime) + $n * 86400 | strftime("%Y-%m-%d"))
      + (if $time then " " + $time else "" end)'
}

# ---- Board ----------------------------------------------------------------

board_name=$(jq -r '.board_name' "$fixture")
resp=$(api POST /api/boards "$(jq -nc --arg n "$board_name" '{name: $n}')")
board_id=$(jq -r '.id' <<<"$resp")
log "created board '$board_name' ($board_id)"

# ---- Columns --------------------------------------------------------------

columns=$(api GET "/api/boards/$board_id/columns")
(($(jq 'length' <<<"$columns") == 1)) || die "new board should start with one column"
seed_column_id=$(jq -r '.[0].id' <<<"$columns")
column_count=$(jq '.columns | length' "$fixture")

for ((i = 0; i < column_count; i++)); do
  spec=$(jq -c ".columns[$i]" "$fixture")
  if ((i == 0)); then
    api PATCH "/api/columns/$seed_column_id" \
      "$(jq -c '{name} + (if .wip_limit then {wip_limit} else {wip_limit: null} end)' <<<"$spec")" \
      >/dev/null
  else
    api POST /api/columns \
      "$(jq -c --arg b "$board_id" '{board_id: $b, name} + (if .wip_limit then {wip_limit} else {} end)' <<<"$spec")" \
      >/dev/null
  fi
done

columns=$(api GET "/api/boards/$board_id/columns")
jq -e --slurpfile f "$fixture" '[.[].name] == [$f[0].columns[].name]' <<<"$columns" >/dev/null ||
  die "column order mismatch after creation: $(jq -c '[.[].name]' <<<"$columns")"
done_column=$(jq -r '.[-1].id' <<<"$columns")
first_column=$(jq -r '.[0].id' <<<"$columns")
api PATCH "/api/columns/$done_column" '{"is_done":true}' >/dev/null
log "columns: $(jq -r '[.[].name] | join(" | ")' <<<"$columns") (Done = last)"

# ---- Colors ---------------------------------------------------------------

colors=$(api GET "/api/boards/$board_id/colors")
color_map=$(jq -c --slurpfile f "$fixture" '
  [$f[0].colors[] as $want
   | {key: $want,
      value: (map(select((.label | ascii_downcase) == ($want | ascii_downcase)
                         or .value == ($want | ascii_downcase))) | first | .id)}]
  | from_entries' <<<"$colors")
missing=$(jq -r 'to_entries | map(select(.value == null) | .key) | join(", ")' <<<"$color_map")
[[ -z $missing ]] || die "fixture colors not in the board palette: $missing"

while read -r extra; do
  api DELETE "/api/boards/$board_id/colors/$extra" >/dev/null
done < <(jq -r --argjson keep "$color_map" '.[] | select(.id as $id | [$keep[]] | index($id) | not) | .id' <<<"$colors")

order=1
while read -r color_id; do
  api PATCH "/api/boards/$board_id/colors/$color_id" "{\"sort_order\":$order}" >/dev/null
  order=$((order + 1))
done < <(jq -r --slurpfile f "$fixture" '. as $m | $f[0].colors[] | $m[.]' <<<"$color_map")
log "palette: $(jq -r --slurpfile f "$fixture" '$f[0].colors | join(", ")' <<<"$color_map")"

# ---- Members --------------------------------------------------------------

members=$(api GET /api/members)

# Resolve an assignee name to a user id (single-admin fallback, see header).
member_id() {
  jq -r --arg n "$1" '
    (map(select(.username == $n)) | first | .id)
    // (if length == 1 then .[0].id else empty end)' <<<"$members"
}

# ---- Tasks ----------------------------------------------------------------

task_count=$(jq '.tasks | length' "$fixture")
log_date=$(shift_due "$(jq -nr --arg c "$CAPTURE_DATE" '$c | strptime("%Y-%m-%d") | mktime - 86400 | strftime("%Y-%m-%d")')")
done_name=$(jq -r '.[-1].name' <<<"$columns")
done_position=0

for ((i = 0; i < task_count; i++)); do
  task=$(jq -c ".tasks[$i]" "$fixture")
  name=$(jq -r '.name' <<<"$task")
  column_name=$(jq -r '.column' <<<"$task")
  column_id=$(jq -r --arg n "$column_name" 'map(select(.name == $n)) | first | .id // empty' <<<"$columns")
  [[ -n $column_id ]] || die "task '$name': unknown column '$column_name'"
  color_id=$(jq -r --arg c "$(jq -r '.color' <<<"$task")" '.[$c] // empty' <<<"$color_map")
  [[ -n $color_id ]] || die "task '$name': color not in palette"

  create_column=$column_id
  [[ $column_name != "$done_name" ]] || create_column=$first_column

  due=""
  if jq -e '.due' <<<"$task" >/dev/null; then
    due=$(shift_due "$(jq -r '.due' <<<"$task")")
  fi
  body=$(jq -c --arg col "$create_column" --arg color "$color_id" --arg due "$due" '
    {name, column_id: $col, color_id: $color}
    + (if .estimate_hours then {estimate_hours} else {} end)
    + (if $due != "" then {due_at: $due} else {} end)' <<<"$task")
  task_id=$(api POST /api/tasks "$body" | jq -r '.id')

  assignee_id=""
  if jq -e '.assignee' <<<"$task" >/dev/null; then
    assignee_id=$(member_id "$(jq -r '.assignee' <<<"$task")")
    [[ -n $assignee_id ]] || die "task '$name': no user for assignee"
  fi
  patch=$(jq -c --arg m "$assignee_id" '
    {labels: (.labels // [])}
    + (if .description then {description} else {} end)
    + (if $m != "" then {member_ids: [$m]} else {} end)' <<<"$task")
  api PATCH "/api/tasks/$task_id" "$patch" >/dev/null

  while read -r subtask; do
    sub_id=$(api POST "/api/tasks/$task_id/subtasks" "$(jq -c '{name}' <<<"$subtask")" | jq -r '.id')
    if jq -e '.done' <<<"$subtask" >/dev/null; then
      api PATCH "/api/tasks/$task_id/subtasks/$sub_id" '{"done":true}' >/dev/null
    fi
  done < <(jq -c '.subtasks // [] | .[]' <<<"$task")

  while read -r comment; do
    api POST "/api/tasks/$task_id/comments" "$(jq -c '{body: .}' <<<"$comment")" >/dev/null
  done < <(jq -c '.comments // [] | .[]' <<<"$task")

  minutes=$(jq '(.logged_hours // 0) * 60 | round' <<<"$task")
  if ((minutes > 0)); then
    ((minutes <= 15 * 60)) || die "task '$name': logged_hours must fit in one day"
    api POST /api/time/manual "$(jq -nc --arg t "$task_id" --arg d "$log_date" --argjson m "$minutes" '
      {task_id: $t, date: $d, from: "09:00",
       to: (("0\(9 + ($m / 60 | floor))" | .[-2:]) + ":" + ("0\($m % 60)" | .[-2:]))}')" >/dev/null
  fi

  if [[ $create_column != "$column_id" ]]; then
    api POST "/api/tasks/$task_id/move" \
      "{\"column_id\":\"$column_id\",\"position\":$done_position}" >/dev/null
    done_position=$((done_position + 1))
  fi
  log "task $((i + 1))/$task_count: $name"
done

# ---- Report ---------------------------------------------------------------

log "fixture fields the API cannot reproduce exactly:"
log "  kanbanflow_board_id, kanbanflow_url: KanbanFlow metadata, no ChipFlow field"
log "  assignee: only registered users can be assigned; names map to the single admin"
log "  board members (header avatars): no API, derived from registered users"
log "  comments: author and timestamp are not in the fixture; posted now as the session user"
log "  logged_hours: the fixture has no log date; entry placed at $log_date 09:00"
log "  Done column: completion time is the seed time, not the capture time"
printf '%s\n' "$board_id"
