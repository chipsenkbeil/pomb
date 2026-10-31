#!/usr/bin/env bash
# Diff this tree's screenshots against a baseline revision on a ci browser runner.
# local
#
# Usage: dev regress [REV]     REV defaults to main on the remote of record
#
# Exports REV to .ci-baseline/, runs the ci regress job on the working copy,
# and removes .ci-baseline/ on every exit. Exits with the job's status. The
# report (diff images included) is kept as an artifact: `ci artifacts last`.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

# shellcheck source=.dev/__remote.sh
. .dev/__remote.sh

if [ -n "${1:-}" ]; then
  rev=$1
else
  remote=$(remote_of_record)
  git fetch -q "$remote" main
  rev=$remote/main
fi

if [ -e .ci-baseline ]; then
  echo "refusing to start: .ci-baseline/ already exists" >&2
  exit 1
fi

trap 'rm -rf .ci-baseline' EXIT
trap 'exit 130' INT TERM

git archive "$rev" --prefix=.ci-baseline/ | tar -x

status=0
ci run --local --job regress -f || status=$?

echo "report: ci artifacts last" >&2
exit "$status"
