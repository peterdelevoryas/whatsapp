# whatsapp

A WhatsApp relay: one WhatsApp number, on the official Cloud API, given to an agent as its own phone. It does two
things:

- **Send and read:** an MCP server. `whatsapp_send(to, text)` messages one of the number's contacts by name; it
  can't message anyone else, so a leaked client token can't be used to spam strangers. `whatsapp_conversations()`
  lists conversations (last message, and whether the 24-hour window for free-form messages is open), and
  `whatsapp_history(contact, limit, before)` pages through one.
- **Receive:** a webhook for incoming messages. It checks Meta's `X-Hub-Signature-256`, drops messages from anyone
  who isn't a contact (without replying, so strangers can't tell the number is live), and forwards text messages to
  the agent's input endpoint as `{"channel": "whatsapp", "sender", "sender_name", "message_id", "text"}`.

The Cloud API keeps no history you can query: incoming messages reach the webhook once, and sent messages exist only
in your own record. So the relay logs every message to and from its contacts, plus delivery statuses (sent,
delivered, read, failed) for what it sent, in an embedded [Turso](https://github.com/tursodatabase/turso) database.
Media messages are logged by kind only, without content.

The agent never talks to WhatsApp directly; it only sees incoming messages and MCP tools.

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
| `WHATSAPP_CONTACTS` | `name=number` pairs, comma-separated (numbers are digits with country code); the only people it talks to |
| `WHATSAPP_APP_SECRET` | the Meta app's secret, for webhook signatures |
| `WHATSAPP_VERIFY_TOKEN` | any random string; entered in the webhook setup form |
| `WHATSAPP_AGENT_URL`, `WHATSAPP_AGENT_TOKEN` | where to forward incoming messages; unset means log only |

`WHATSAPP_DB` (default `whatsapp.db`), `WHATSAPP_TOKENS` (default `tokens`), `WHATSAPP_ADDR` (default
`127.0.0.1:8751`), `WHATSAPP_ALLOWED_HOSTS` (default localhost), and `WHATSAPP_DISK_MAX_PERCENT` (default 85)
configure the server itself.

## Meta setup

1. Create an app at developers.facebook.com with the WhatsApp use case. The free test number works for messaging
   the agent's contacts; add each contact's number as a recipient.
2. For a permanent token, add a system user in Business settings, assign it the app and the WhatsApp account, and
   generate a token that never expires with `whatsapp_business_messaging` and `whatsapp_business_management`.
3. Configure the webhook: callback URL `https://<your domain>/webhook`, your verify token, and subscribe to
   `messages`.
4. Subscribe the app to your WhatsApp Business Account, or no messages arrive:
   `curl -X POST -H "Authorization: Bearer $WHATSAPP_ACCESS_TOKEN" https://graph.facebook.com/v26.0/<WABA ID>/subscribed_apps`

## Production

Runs on any Linux server you can SSH into as root (set up with `deploy/cloud-init.yaml`, written for Ubuntu 24.04),
behind Caddy for HTTPS. Its hostname lives in `deploy/config` (untracked; copy `deploy/config.example`).
The message log lives on a block-storage volume mounted at `/var/lib/whatsapp`, so the server itself is disposable;
the service won't start unless that path is a mount point, so a missing volume can't turn into an empty log.

- Deploy: `deploy/deploy.sh` (rsyncs source, builds on the VM, installs, restarts). The server is session-less and
  Caddy holds requests for up to 15s while it restarts, so deploys don't interrupt clients.
- Secrets: `deploy/secrets.sh` copies `.env` to `/etc/whatsapp/env` and restarts; rerun after changing any secret.
- Tokens: `deploy/token.sh <source>` mints a token on the server (which stores only its hash), reloads the tokens
  file without a restart, and saves the token in the macOS keychain (`whatsapp-mcp-token` / `<source>`). Revoke by
  deleting the line from `/etc/whatsapp/tokens` and running `systemctl reload whatsapp`.
- Health: `GET /health` (no auth) returns 200 when the database answers and its disk is under 85% full, 503 with
  the problems otherwise. It reveals no messages.
- The volume's ext4 filesystem is labeled `whatsapp-data` (`e2label <device> whatsapp-data` once, for a new
  volume), and `cloud-init.yaml` mounts that label at `/var/lib/whatsapp`. Moving to a new server: create it with
  `cloud-init.yaml`, run `deploy/secrets.sh` and copy `/etc/whatsapp/tokens` over, stop `whatsapp` on the old
  server, move the volume, reboot the new server so it mounts, and run `deploy/deploy.sh`.

Outgoing requests to the Cloud API use IPv4 only: sends from some IPv6 addresses (seen from a Hetzner VM) fail with
`(#131005) Access denied` even though the same token works over IPv4.
