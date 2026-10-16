#!/usr/bin/env python3
"""Count ChipFlow parity-tracker defects by status.

Usage: tracker_count.py <path-to-kf-parity-defects.md>

Parse contract (mirrors the repo AGENTS.md — keep in sync):
- For each `### KF-NNN` section, the FIRST occurrence of the ID wins
  (duplicate sections exist and never override the first).
- A section ends at the next `###` header of any kind.
- The authoritative state is the FIRST `- Status:` line in the section
  (newest lines are prepended). Whitespace before the bullet is tolerated;
  the state token is captured generically and anchored at the start of
  the status value, then classified.
- OPEN / IN_PROGRESS / BUILT / VERIFIED / DEPLOYED / RELEASED are the
  counted states. FIXED / CLOSED / INVALID are tallied separately and
  excluded from the counts, as are sections with no status line.

Stdlib only. Exit code 0 on success, 2 on usage/read errors.
"""

import re
import sys

COUNTED = ["OPEN", "IN_PROGRESS", "BUILT", "VERIFIED", "DEPLOYED", "RELEASED"]
EXCLUDED = ["FIXED", "CLOSED", "INVALID"]


def parse_tracker(text):
    counts = {state: [] for state in COUNTED}
    excluded = {state: 0 for state in EXCLUDED}
    other_states = {}
    no_status = []
    seen = set()
    # Split into sections on ### headers; first occurrence of each KF wins.
    parts = re.split(r"(?m)^###\s+", text)
    for part in parts[1:]:
        m = re.match(r"(KF-\d+)\b", part)
        if not m:
            continue
        kid = m.group(1)
        if kid in seen:
            continue
        seen.add(kid)
        sm = re.search(r"(?m)^\s*-?\s*Status:\s*([A-Z_]+)", part)
        if not sm:
            no_status.append(kid)
            continue
        state = sm.group(1)
        if state in counts:
            counts[state].append(kid)
        elif state in excluded:
            excluded[state] += 1
        else:
            other_states[state] = other_states.get(state, 0) + 1
    return counts, excluded, other_states, no_status, len(seen)


def main(argv):
    if len(argv) != 2:
        print(__doc__.strip().splitlines()[0], file=sys.stderr)
        print("usage: tracker_count.py <path-to-kf-parity-defects.md>",
              file=sys.stderr)
        return 2
    try:
        with open(argv[1], encoding="utf-8") as f:
            text = f.read()
    except OSError as e:
        print(f"error: cannot read {argv[1]}: {e}", file=sys.stderr)
        return 2

    counts, excluded, other_states, no_status, total = parse_tracker(text)
    for state in COUNTED:
        ids = counts[state]
        suffix = f" ({' '.join(ids)})" if ids else ""
        print(f"{state}: {len(ids)}{suffix}")
    excl = " ".join(f"{s}={excluded[s]}" for s in EXCLUDED)
    print(f"excluded: {excl}")
    if other_states:
        rest = " ".join(f"{s}={n}" for s, n in sorted(other_states.items()))
        print(f"other states: {rest}")
    print(f"no status line: {len(no_status)}")
    print(f"total KF sections (first occurrences): {total}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
