#!/usr/bin/env bash
# Format the workspace with nightly rustfmt, which the unstable rustfmt.toml options need.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."

cargo +nightly fmt
