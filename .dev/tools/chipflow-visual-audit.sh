#!/bin/bash
# ChipFlow visual audit - takes screenshots and runs structural checks
# Runs via cron, saves results for review

set -e
AUDIT_DIR="/tmp/chipflow-audit-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$AUDIT_DIR"

# Use test instance on :3200 (must be running)
if ! curl -s http://127.0.0.1:3200/api/v1/version > /dev/null; then
  echo "Test instance not running on :3200" > "$AUDIT_DIR/status.txt"
  exit 1
fi

# Structural checks (no sidebar visible, no toolbar)
HTML=$(curl -s -b /tmp/cf-cookies2.txt http://127.0.0.1:3200/b/$(cat /tmp/cf-bid.txt | cut -d= -f2) 2>/dev/null || echo "")

CHECKS=""

# Check 1: No persistent sidebar (should be hidden by CSS transform)
if echo "$HTML" | grep -q 'boards-sidebar.*open'; then
  CHECKS="${CHECKS}FAIL: Sidebar is open by default\n"
else
  CHECKS="${CHECKS}PASS: Sidebar hidden by default\n"
fi

# Check 2: No board-toolbar div
if echo "$HTML" | grep -q 'class="board-toolbar"'; then
  CHECKS="${CHECKS}FAIL: Old board-toolbar still present\n"
else
  CHECKS="${CHECKS}PASS: Old board-toolbar removed\n"
fi

# Check 3: Board name bar present (KF-173, restyled by KF-178 as
# .board-header-row with the board name in .board-bar-title)
if echo "$HTML" | grep -q 'board-header-row'; then
  CHECKS="${CHECKS}PASS: Board name bar present\n"
else
  CHECKS="${CHECKS}FAIL: Board name bar missing\n"
fi

# Check 4: No board tabs (KF-174)
if echo "$HTML" | grep -q 'board-tab-active'; then
  CHECKS="${CHECKS}FAIL: Board tabs still present\n"
else
  CHECKS="${CHECKS}PASS: Board tabs removed\n"
fi

echo -e "$CHECKS" > "$AUDIT_DIR/checks.txt"
echo "Audit complete: $AUDIT_DIR"
cat "$AUDIT_DIR/checks.txt"
