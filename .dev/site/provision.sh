#!/usr/bin/env bash
# Provision and harden the box over SSH; idempotent: dev site provision
# local
set -euo pipefail

# shellcheck source-path=SCRIPTDIR source=__common.sh
source "$(dirname "$0")/__common.sh"
site="$(pomb_site_dir)"
pomb_load_host
admin_ip="$(pomb_pass_line Agents/Linode/pomb.app-admin-ip)" || exit 1
if [[ ! "$admin_ip" =~ ^[0-9A-Fa-f.:/]+$ ]]; then
  echo "site: the password store entry Agents/Linode/pomb.app-admin-ip is not an address" >&2
  exit 1
fi

stage="$(mktemp -d /tmp/pomb-site.XXXXXX)"
sock="$(mktemp -u /tmp/pomb-ssh.XXXXXX)"
trap 'ssh -o ControlPath="$sock" -O exit "$POMB_HOST" 2>/dev/null || true; rm -rf "$stage"' EXIT

(cd "$site" && cp -R apt sshd systemd env.in pomb-update setup.sh "$stage/")
conf="$(<"$stage/sshd/10-pomb.conf")"
printf '%s\n' "${conf//@ADMIN_IP@/$admin_ip}" >"$stage/sshd/10-pomb.conf"

ssh "${POMB_SSH_OPTS[@]}" -o ControlMaster=yes -o ControlPath="$sock" -fN "$POMB_HOST"
via_master() { ssh -o ControlPath="$sock" "$POMB_HOST" "$@"; }

COPYFILE_DISABLE=1 tar -C "$stage" -cf - . | via_master 'rm -rf /root/pomb-box && mkdir -p /root/pomb-box && tar -C /root/pomb-box -xf -'
via_master 'bash /root/pomb-box/setup.sh'

if ! ssh "${POMB_SSH_OPTS[@]}" -o ControlPath=none "$POMB_HOST" true; then
  echo "provision: fresh login failed, rolling back sshd hardening" >&2
  via_master 'rm -f /etc/ssh/sshd_config.d/10-pomb.conf && sshd -t && systemctl reload ssh'
  exit 1
fi
echo "provision: fresh key login ok"
