#!/usr/bin/env bash
for ch in stable nightly; do
  printf '%-8s %s\n' "$ch" "$(journalctl -u "pomb-$ch" --no-pager -q | grep "setup code" | tail -n 1 | sed 's/.*setup code/setup code/')"
done
