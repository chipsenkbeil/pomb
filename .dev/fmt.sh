#!/usr/bin/env bash
# Format the workspace with nightly rustfmt, which the unstable rustfmt.toml options need,
# then drop the trailing period from each // one-liner.
set -euo pipefail

# shellcheck source-path=SCRIPTDIR source-path=SCRIPTDIR/.. source=lint-lib.sh
. "$(dirname "${BASH_SOURCE[0]}")/lint-lib.sh"

cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo +nightly fmt

while IFS= read -r file; do
    fix_comment_periods "$file"
done < <(lint_files '*.rs')
