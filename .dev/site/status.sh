#!/usr/bin/env bash
# Show service states, current releases, last update results and disk use
# local
set -euo pipefail

# shellcheck source-path=SCRIPTDIR source=__common.sh
source "$(dirname "$0")/__common.sh"

pomb_ssh bash -s <<'REMOTE'
set -u
units="pomb-stable.service pomb-nightly.service pomb-update@stable.timer pomb-update@nightly.timer cloudflared.service"
echo "== units (enabled / active)"
for u in $units; do
  printf '%-30s %-10s %s\n' "$u" "$(systemctl is-enabled "$u" 2>&1)" "$(systemctl is-active "$u" 2>&1)"
done
echo "== current release"
for ch in stable nightly; do
  cur=$(readlink "/opt/pomb/$ch/current" 2>/dev/null || true)
  printf '%-8s %s\n' "$ch" "${cur:-none}"
done
echo "== last update"
for ch in stable nightly; do
  printf '%-8s %s\n' "$ch" "$(cat "/var/lib/pomb/last-update-$ch" 2>/dev/null || echo never)"
done
echo "== newest pre-update backup"
for ch in stable nightly; do
  newest=$(find "/var/lib/pomb/$ch/backups" -maxdepth 1 -type f -name 'pomb-*-before-*.redb' 2>/dev/null | sort | tail -n 1)
  if [[ -n "$newest" ]]; then
    printf '%-8s %s %s\n' "$ch" "$(basename "$newest")" "$(du -h "$newest" | cut -f1)"
  else
    printf '%-8s none\n' "$ch"
  fi
done
echo "== Pomb's own backups (newest file, journal lines mentioning backup)"
for ch in stable nightly; do
  own=$(find "/var/lib/pomb/$ch/backups" -maxdepth 1 -type f ! -name '*-before-*' 2>/dev/null | sort | tail -n 1)
  own_name=${own:+$(basename "$own")}
  printf '%-8s %s, journal lines: %s\n' "$ch" "${own_name:-none}" "$(journalctl -u "pomb-$ch" --no-pager -q 2>/dev/null | grep -ci backup)"
done
echo "== setup code in the journal (read it with journalctl -u pomb-<channel>)"
for ch in stable nightly; do
  if journalctl -u "pomb-$ch" --no-pager -q | grep -q "setup code"; then
    printf '%-8s %s\n' "$ch" present
  else
    printf '%-8s %s\n' "$ch" absent
  fi
done
echo "== listening"
ss -ltn | awk 'NR==1 || /:300[12]\b/'
echo "== disk"
df -h / | tail -n 1
du -sh /opt/pomb /var/lib/pomb 2>/dev/null
REMOTE
