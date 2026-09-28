#!/bin/bash
# Nightly backup to another machine over rsync+SSH. The server is stopped for the copy so the
# database file and its WAL are consistent; downtime is a few seconds, and Caddy
# holds requests (including Meta's webhook deliveries) until it's back.
set -euo pipefail
. /etc/whatsapp/backup.env   # BACKUP_TARGET=user@host, optionally BACKUP_SSH_PORT=22
KEEP_DAYS=30
SSH=(ssh -p "${BACKUP_SSH_PORT:-22}" -i /etc/whatsapp/backup_key -o BatchMode=yes)
stamp=$(date -u +%Y-%m-%dT%H%MZ)
work=$(mktemp -d)
trap 'rm -rf "$work"; systemctl start whatsapp' EXIT

systemctl stop whatsapp
mkdir "$work/db"
cp -a /var/lib/whatsapp/. "$work/db/"
systemctl start whatsapp

tar -C "$work" -czf "$work/whatsapp-$stamp.tar.gz" db
rsync -e "${SSH[*]}" "$work/whatsapp-$stamp.tar.gz" "$BACKUP_TARGET:backups/"

# Prune old backups.
cutoff=$(date -u -d "-$KEEP_DAYS days" +%Y-%m-%d)
"${SSH[@]}" "$BACKUP_TARGET" ls backups | while read -r f; do
  if [[ $f =~ ^whatsapp-([0-9]{4}-[0-9]{2}-[0-9]{2}) ]] && [[ ${BASH_REMATCH[1]} < $cutoff ]]; then
    "${SSH[@]}" "$BACKUP_TARGET" rm "backups/$f"
  fi
done
# Tell the server when the last backup succeeded; GET /health reports its age.
date -u +%Y-%m-%dT%H:%M:%SZ > /var/lib/whatsapp/last-backup
chown whatsapp:whatsapp /var/lib/whatsapp/last-backup
echo "backed up whatsapp-$stamp.tar.gz"
