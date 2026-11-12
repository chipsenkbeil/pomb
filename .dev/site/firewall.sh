set -euo pipefail
if ! systemctl is-active --quiet cloudflared; then
  echo "Refusing to close port 22. The cloudflared unit is not active." >&2
  exit 1
fi
if ! ss -ltn | awk '$4 == "127.0.0.1:22" || $4 == "0.0.0.0:22" { found = 1 } END { exit !found }'; then
  echo "Refusing to close port 22. The sshd daemon does not listen on 127.0.0.1." >&2
  exit 1
fi
want=$(
  cat <<'RULES'
#!/usr/sbin/nft -f

flush ruleset

table inet filter {
  chain input {
    type filter hook input priority filter;
    tcp dport 22 ct state new iifname != "lo" drop
  }
  chain forward {
    type filter hook forward priority filter;
  }
  chain output {
    type filter hook output priority filter;
  }
}
RULES
)
if [[ "$(cat /etc/nftables.conf 2>/dev/null)" != "$want" ]]; then
  printf '%s\n' "$want" >/etc/nftables.conf
  echo "Changed /etc/nftables.conf."
fi
nft -c -f /etc/nftables.conf
nft -f /etc/nftables.conf
systemctl enable nftables 2>&1 | tail -n 1
echo "== ruleset"
nft list chain inet filter input
