#!/bin/bash
# Build on the VM and (re)install. Usage: deploy/deploy.sh [host]
set -euo pipefail
. "$(dirname "$0")/lib.sh"
HOST=${1:-$HOST}
cd "$(dirname "$0")/.."
ssh "$HOST" mkdir -p /root/src/whatsapp
rsync -az --delete --exclude target --exclude .git --exclude /.env --exclude /tokens --exclude /deploy/config ./ "$HOST:/root/src/whatsapp/"
ssh "$HOST" bash -se -- "$WHATSAPP_DOMAIN" <<'REMOTE'
set -euo pipefail
DOMAIN=$1
cd /root/src/whatsapp
~/.cargo/bin/cargo build --release --locked
install -m 0755 target/release/whatsapp /usr/local/bin/whatsapp
# The domain lives in the untracked deploy/config, not in the repo.
sed "s/@DOMAIN@/$DOMAIN/g" deploy/whatsapp.service > /etc/systemd/system/whatsapp.service
sed "s/@DOMAIN@/$DOMAIN/g" deploy/Caddyfile > /etc/caddy/Caddyfile
touch /etc/whatsapp/tokens && chown root:whatsapp /etc/whatsapp/tokens && chmod 0640 /etc/whatsapp/tokens
[ -f /etc/whatsapp/env ] || { echo "missing /etc/whatsapp/env; run deploy/secrets.sh first" >&2; exit 1; }
systemctl daemon-reload
systemctl enable --now whatsapp
systemctl restart whatsapp
systemctl reload caddy
systemctl --no-pager --lines=5 status whatsapp
REMOTE
