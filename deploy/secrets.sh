#!/bin/bash
# Copy the local .env (Cloud API credentials and WHATSAPP_OWNER) to the server
# as /etc/whatsapp/env, then restart the service if it's installed. Run it again
# whenever the access token changes.
# Usage: deploy/secrets.sh
set -euo pipefail
. "$(dirname "$0")/lib.sh"
env="$(dirname "$0")/../.env"
[ -f "$env" ] || { echo "missing $env" >&2; exit 1; }
ssh "$HOST" 'install -m 0640 -o root -g whatsapp /dev/stdin /etc/whatsapp/env && if systemctl cat whatsapp >/dev/null 2>&1; then systemctl restart whatsapp; fi' < "$env"
echo "installed /etc/whatsapp/env on $HOST" >&2
