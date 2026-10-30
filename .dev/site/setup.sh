#!/usr/bin/env bash
# Runs on the box as root, from /root/pomb-box. Idempotent.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
changes=0
export DEBIAN_FRONTEND=noninteractive

changed() {
  echo "changed: $*"
  changes=$((changes + 1))
}

# put <src> <dst> <mode> <owner:group>: install the file when it differs
put() {
  if [[ -f "$2" ]] && cmp -s "$1" "$2" &&
    [[ "$(stat -c '%a %U:%G' "$2")" == "$3 $4" ]]; then
    return
  fi
  install -D -m "$3" -o "${4%%:*}" -g "${4##*:}" "$1" "$2"
  changed "$2"
}

ensure_dir() {
  local mode="$1" owner="$2" dir="$3"
  if [[ -d "$dir" && "$(stat -c '%a %U:%G' "$dir")" == "$mode $owner" ]]; then
    return
  fi
  install -d -m "$mode" -o "${owner%%:*}" -g "${owner##*:}" "$dir"
  changed "$dir"
}

echo "== packages"
apt-get update -qq
pending=$(apt-get -s full-upgrade | grep -c '^Inst' || true)
if ((pending > 0)); then
  apt-get -y -qq -o Dpkg::Options::=--force-confold full-upgrade
  changed "full-upgrade ($pending packages)"
fi
for pkg in unattended-upgrades curl jq ca-certificates; do
  if ! dpkg -s "$pkg" >/dev/null 2>&1; then
    apt-get -y -qq install "$pkg"
    changed "installed $pkg"
  fi
done
if dpkg -s netcat-openbsd >/dev/null 2>&1; then
  apt-get -y -qq purge netcat-openbsd
  changed "purged netcat-openbsd"
fi
put "$here/apt/20auto-upgrades" /etc/apt/apt.conf.d/20auto-upgrades 644 root:root
put "$here/apt/52pomb-unattended" /etc/apt/apt.conf.d/52pomb-unattended 644 root:root

echo "== swap"
if [[ -z "$(swapon --noheadings --show)" ]]; then
  fallocate -l 1G /swapfile
  chmod 600 /swapfile
  mkswap /swapfile >/dev/null
  grep -q '^/swapfile ' /etc/fstab || echo '/swapfile none swap sw 0 0' >>/etc/fstab
  swapon /swapfile
  changed "swapfile"
else
  echo "swap present, nothing to add"
fi

echo "== sshd"
if [[ ! -s /root/.ssh/authorized_keys ]]; then
  echo "refusing to harden sshd: /root/.ssh/authorized_keys is empty" >&2
  exit 1
fi
if ! cmp -s "$here/sshd/10-pomb.conf" /etc/ssh/sshd_config.d/10-pomb.conf 2>/dev/null; then
  prev=""
  if [[ -f /etc/ssh/sshd_config.d/10-pomb.conf ]]; then
    prev=$(mktemp)
    cp /etc/ssh/sshd_config.d/10-pomb.conf "$prev"
  fi
  install -D -m 644 "$here/sshd/10-pomb.conf" /etc/ssh/sshd_config.d/10-pomb.conf
  if sshd -t; then
    systemctl reload ssh
    changed "sshd hardening"
  else
    if [[ -n "$prev" ]]; then
      cp "$prev" /etc/ssh/sshd_config.d/10-pomb.conf
    else
      rm -f /etc/ssh/sshd_config.d/10-pomb.conf
    fi
    echo "sshd -t failed, reverted" >&2
    exit 1
  fi
fi
sshd -T | grep -Ei '^(passwordauthentication|kbdinteractiveauthentication|permitrootlogin) '

echo "== user and directories"
if ! id pomb >/dev/null 2>&1; then
  useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin pomb
  changed "user pomb"
fi
ensure_dir 755 root:root /var/lib/pomb
ensure_dir 755 root:root /opt/pomb
ensure_dir 755 root:root /etc/pomb
declare -A port=([stable]=3001 [nightly]=3002)
declare -A url=([stable]=https://pomb.app [nightly]=https://nightly.pomb.app)
for ch in stable nightly; do
  ensure_dir 750 pomb:pomb "/var/lib/pomb/$ch"
  ensure_dir 750 pomb:pomb "/var/lib/pomb/$ch/backups"
  ensure_dir 755 root:root "/opt/pomb/$ch"
  ensure_dir 755 root:root "/opt/pomb/$ch/releases"
done

for ch in stable nightly; do
  for f in "/var/lib/pomb/$ch"/pomb.redb.failed-*; do
    if [[ -f "$f" && "$(stat -c '%a %U:%G' "$f")" != "640 pomb:pomb" ]]; then
      chown pomb:pomb "$f"
      chmod 640 "$f"
      changed "$f"
    fi
  done
done

echo "== env files and units"
tmp=$(mktemp -d)
for ch in stable nightly; do
  sed -e "s|@CH@|$ch|g" -e "s|@PORT@|${port[$ch]}|g" -e "s|@URL@|${url[$ch]}|g" \
    "$here/env.in" >"$tmp/$ch.env"
  put "$tmp/$ch.env" "/etc/pomb/$ch.env" 640 root:pomb
  sed -e "s|@CH@|$ch|g" "$here/systemd/pomb@.service.in" >"$tmp/pomb-$ch.service"
  put "$tmp/pomb-$ch.service" "/etc/systemd/system/pomb-$ch.service" 644 root:root
done
rm -rf "$tmp"
put "$here/systemd/pomb-update@.service" /etc/systemd/system/pomb-update@.service 644 root:root
put "$here/systemd/pomb-update@.timer" /etc/systemd/system/pomb-update@.timer 644 root:root
put "$here/pomb-update" /usr/local/sbin/pomb-update 755 root:root
systemctl daemon-reload

echo "== state"
systemctl is-enabled pomb-stable pomb-nightly pomb-update@stable.timer pomb-update@nightly.timer || true
echo "cloudflared: $(systemctl is-active cloudflared)"
echo "provision: $changes change(s)"
