#!/usr/bin/env bash
# Run the test suite. nextest skips doctests, so they get their own step.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo nextest run --all-features
cargo test --doc --all-features
