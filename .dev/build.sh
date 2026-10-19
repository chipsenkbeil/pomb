#!/usr/bin/env bash
# Build the release binary.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo build --release --locked "$@"
