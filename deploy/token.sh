#!/bin/bash
# Mint a client token on the production server and store it in the macOS
# keychain (service "whatsapp-mcp-token", account = source). The server keeps
# only the token's hash. Re-running for the same source adds a second token;
# remove the old line from /etc/whatsapp/tokens to revoke it.
#
# Usage: deploy/token.sh <source>
# Read it back: security find-generic-password -s whatsapp-mcp-token -a <source> -w
set -euo pipefail
. "$(dirname "$0")/lib.sh"
[ $# -eq 1 ] || { sed -n 's/^# \{0,1\}//; 2,8p' "$0" >&2; exit 2; }
source=$1

token=$(ssh "$HOST" bash -s -- "$source" <<'REMOTE'
set -euo pipefail
err=$(mktemp); trap 'rm -f $err' EXIT
line=$(whatsapp token "$1" 2>"$err") || { cat "$err" >&2; exit 1; }
echo "$line" >> /etc/whatsapp/tokens
systemctl reload whatsapp
sed -n 2p "$err"
REMOTE
)
security add-generic-password -U -s whatsapp-mcp-token -a "$source" -w "$token"
echo "minted token for $source; stored in keychain (whatsapp-mcp-token / $source)" >&2
