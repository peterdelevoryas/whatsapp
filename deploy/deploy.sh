#!/bin/bash
# Build on the VM and (re)install. Usage: deploy/deploy.sh [host]
set -euo pipefail
. "$(dirname "$0")/lib.sh"
HOST=${1:-$HOST}
cd "$(dirname "$0")/.."
ssh "$HOST" mkdir -p /root/src/whatsapp
rsync -az --delete --exclude target --exclude .git --exclude /.env --exclude /tokens --exclude /deploy/config ./ "$HOST:/root/src/whatsapp/"
ssh "$HOST" bash -se -- "$WHATSAPP_DOMAIN" "$WHATSAPP_PRIVATE_IP" <<'REMOTE'
set -euo pipefail
DOMAIN=$1
PRIVATE_IP=$2
cd /root/src/whatsapp
~/.cargo/bin/cargo build --release --locked
install -m 0755 target/release/whatsapp /usr/local/bin/whatsapp
install -m 0755 deploy/backup.sh /usr/local/bin/whatsapp-backup
install -m 0644 deploy/whatsapp-backup.service deploy/whatsapp-backup.timer /etc/systemd/system/
# The domain and private IP live in the untracked deploy/config, not in the repo.
sed "s/@DOMAIN@/$DOMAIN/g; s/@PRIVATE_IP@/$PRIVATE_IP/g" deploy/whatsapp.service > /etc/systemd/system/whatsapp.service
# The edge terminates TLS for the domain and proxies here over the private
# network.
ufw allow from 10.0.0.0/16 to any port 8751 proto tcp >/dev/null
touch /etc/whatsapp/tokens && chown root:whatsapp /etc/whatsapp/tokens && chmod 0640 /etc/whatsapp/tokens
[ -f /etc/whatsapp/env ] || { echo "missing /etc/whatsapp/env; run deploy/secrets.sh first" >&2; exit 1; }
systemctl daemon-reload
systemctl enable --now whatsapp whatsapp-backup.timer
systemctl restart whatsapp
systemctl --no-pager --lines=5 status whatsapp
REMOTE
