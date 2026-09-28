# whatsapp

A WhatsApp proxy for a personal agent, on the official WhatsApp Cloud API. It does two things:

- **Send:** an MCP server with one tool, `whatsapp_send(text)`, which messages the owner. The recipient is fixed in
  config, so a leaked client token can only message you.
- **Receive:** a webhook for incoming messages. It checks Meta's `X-Hub-Signature-256`, drops senders that aren't on
  the allowlist (without replying, so strangers can't tell the number is live), and forwards text messages to the
  agent's input endpoint as `{"channel": "whatsapp", "sender", "message_id", "text"}`.

The agent never talks to WhatsApp directly; it only sees incoming messages and an MCP send tool.

## Run locally

```sh
cargo build --release
# Mint a token per client; the tokens file stores only hashes.
./target/release/whatsapp token claude-code >> tokens
set -a; . ./.env; set +a     # see below
./target/release/whatsapp serve   # http://127.0.0.1:8751/mcp
```

Environment (`.env`, untracked):

| Variable | |
|---|---|
| `WHATSAPP_ACCESS_TOKEN` | Cloud API token (a system user's, so it doesn't expire) |
| `WHATSAPP_PHONE_NUMBER_ID` | the sending number's ID |
| `WHATSAPP_OWNER` | your number, digits with country code; the only recipient |
| `WHATSAPP_APP_SECRET` | the Meta app's secret, for webhook signatures |
| `WHATSAPP_VERIFY_TOKEN` | any random string; entered in the webhook setup form |
| `WHATSAPP_ALLOWED_SENDERS` | comma-separated numbers whose messages are accepted (default: the owner) |
| `WHATSAPP_AGENT_URL`, `WHATSAPP_AGENT_TOKEN` | where to forward incoming messages; unset means log only |

`WHATSAPP_TOKENS` (default `tokens`), `WHATSAPP_ADDR` (default `127.0.0.1:8751`), and `WHATSAPP_ALLOWED_HOSTS`
(default localhost) configure the server itself.

## Meta setup

1. Create an app at developers.facebook.com with the WhatsApp use case. The free test number works for messaging
   yourself; add your number as a recipient.
2. For a permanent token, add a system user in Business settings, assign it the app and the WhatsApp account, and
   generate a token that never expires with `whatsapp_business_messaging` and `whatsapp_business_management`.
3. Configure the webhook: callback URL `https://<your domain>/webhook`, your verify token, and subscribe to
   `messages`.
4. Subscribe the app to your WhatsApp Business Account, or no messages arrive:
   `curl -X POST -H "Authorization: Bearer $WHATSAPP_ACCESS_TOKEN" https://graph.facebook.com/v26.0/<WABA ID>/subscribed_apps`

## Production

Runs on any Linux server you can SSH into as root (set up with `deploy/cloud-init.yaml`, written for Ubuntu 24.04),
behind Caddy for HTTPS. Its hostname lives in `deploy/config` (untracked; copy `deploy/config.example`).

- Deploy: `deploy/deploy.sh` (rsyncs source, builds on the VM, installs, restarts). The server is session-less and
  Caddy holds requests for up to 15s while it restarts, so deploys don't interrupt clients.
- Secrets: `deploy/secrets.sh` copies `.env` to `/etc/whatsapp/env` and restarts; rerun after changing any secret.
- Tokens: `deploy/token.sh <source>` mints a token on the server (which stores only its hash), reloads the tokens
  file without a restart, and saves the token in the macOS keychain (`whatsapp-mcp-token` / `<source>`). Revoke by
  deleting the line from `/etc/whatsapp/tokens` and running `systemctl reload whatsapp`.
- Health: `GET /health` (no auth) returns `ok`.

Outgoing requests to the Cloud API use IPv4 only: sends from some IPv6 addresses (seen from a Hetzner VM) fail with
`(#131005) Access denied` even though the same token works over IPv4.
