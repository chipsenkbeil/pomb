#!/usr/bin/env bash
# Run the test suite. nextest skips doctests, so they get their own step.
# Arguments go to nextest (`dev test -- -j 1 module`), and doctests run only
# when there are none.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo nextest run --all-features "$@"
if [ "$#" -eq 0 ]; then
    cargo test --doc --all-features
fi
