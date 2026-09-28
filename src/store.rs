//! The message log. The Cloud API keeps no history you can query later: each
//! incoming message reaches the webhook once, and outgoing messages exist only
//! in our own record. So everything the number sends or receives to a contact
//! is logged here, along with delivery statuses for what it sent.

use anyhow::{Context, Result, bail};
use schemars::JsonSchema;
use serde::Serialize;
use turso::{Builder, Database, Value};

const TABLES: &str = "
CREATE TABLE IF NOT EXISTS messages (
  id        TEXT PRIMARY KEY,  -- WhatsApp's message ID
  contact   TEXT NOT NULL,     -- the other party's number
  direction TEXT NOT NULL,     -- 'in' or 'out'
  kind      TEXT NOT NULL,     -- text, image, audio, ...
  text      TEXT,
  at        TEXT NOT NULL,     -- RFC 3339, UTC, second precision
  status    TEXT,              -- outgoing only: sent, delivered, read, failed
  status_at TEXT,
  error     TEXT,              -- why delivery failed
  source    TEXT               -- outgoing only: the client that sent it
);
CREATE INDEX IF NOT EXISTS messages_contact_at ON messages (contact, at);
";

const COLUMNS: &str = "id, direction, kind, text, at, status, status_at, error";

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq)]
pub struct Message {
    /// WhatsApp's message ID.
    pub id: String,
    /// `in` (from the contact) or `out` (from this number).
    pub direction: String,
    /// `text`, or a media type (image, audio, document, ...) whose content isn't stored.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// When it was sent (RFC 3339).
    pub at: String,
    /// Outgoing only: sent, delivered, read, or failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_at: Option<String>,
    /// Outgoing only: why delivery failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub struct Incoming<'a> {
    pub id: &'a str,
    pub contact: &'a str,
    pub kind: &'a str,
    pub text: Option<&'a str>,
    pub at: String,
}

#[derive(Clone)]
pub struct Store {
    db: Database,
}

impl Store {
    pub async fn open(path: &str) -> Result<Self> {
        let db = Builder::new_local(path)
            .build()
            .await
            .with_context(|| format!("opening database {path}"))?;
        let conn = db.connect()?;
        conn.execute_batch(TABLES)
            .await
            .context("creating tables")?;
        drop(conn);
        Ok(Self { db })
    }

    fn conn(&self) -> Result<turso::Connection> {
        let conn = self.db.connect()?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    pub async fn count(&self) -> Result<u64> {
        let mut rows = self
            .conn()?
            .query("SELECT count(*) FROM messages", ())
            .await?;
        let row = rows.next().await?.context("count returned no rows")?;
        match row.get_value(0)? {
            Value::Integer(n) => Ok(n as u64),
            other => bail!("count: expected an integer, got {other:?}"),
        }
    }

    /// Logs a message from a contact. Returns false if it was already logged:
    /// Meta redelivers messages it isn't sure we received.
    pub async fn record_incoming(&self, m: Incoming<'_>) -> Result<bool> {
        let changed = self
            .conn()?
            .execute(
                "INSERT OR IGNORE INTO messages (id, contact, direction, kind, text, at) \
                 VALUES (?1, ?2, 'in', ?3, ?4, ?5)",
                vec![
                    Value::Text(m.id.to_string()),
                    Value::Text(m.contact.to_string()),
                    Value::Text(m.kind.to_string()),
                    opt_text(m.text),
                    Value::Text(m.at),
                ],
            )
            .await?;
        Ok(changed > 0)
    }

    /// Logs a text message this number sent, as `sent`.
    pub async fn record_outgoing(
        &self,
        id: &str,
        contact: &str,
        text: &str,
        source: &str,
    ) -> Result<()> {
        let now = now();
        self.conn()?
            .execute(
                "INSERT OR IGNORE INTO messages \
                 (id, contact, direction, kind, text, at, status, status_at, source) \
                 VALUES (?1, ?2, 'out', 'text', ?3, ?4, 'sent', ?4, ?5)",
                vec![
                    Value::Text(id.to_string()),
                    Value::Text(contact.to_string()),
                    Value::Text(text.to_string()),
                    Value::Text(now),
                    Value::Text(source.to_string()),
                ],
            )
            .await?;
        Ok(())
    }

    /// Applies a delivery status to a message this number sent. Statuses can
    /// arrive out of order, so one never moves a message backwards (a late
    /// `delivered` doesn't undo `read`). Unknown message IDs are ignored.
    pub async fn record_status(
        &self,
        id: &str,
        status: &str,
        at: String,
        error: Option<&str>,
    ) -> Result<()> {
        let Some(new_rank) = status_rank(status) else {
            return Ok(());
        };
        let conn = self.conn()?;
        let mut rows = conn
            .query(
                "SELECT status FROM messages WHERE id = ?1 AND direction = 'out'",
                vec![Value::Text(id.to_string())],
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(());
        };
        let current = text(&row, 0)?;
        drop(rows);
        let current_rank = current.as_deref().and_then(status_rank).unwrap_or(0);
        if new_rank <= current_rank {
            return Ok(());
        }
        conn.execute(
            "UPDATE messages SET status = ?2, status_at = ?3, error = ?4 WHERE id = ?1",
            vec![
                Value::Text(id.to_string()),
                Value::Text(status.to_string()),
                Value::Text(at),
                opt_text(error),
            ],
        )
        .await?;
        Ok(())
    }

    /// The most recent message with `contact`, either direction.
    pub async fn last_message(&self, contact: &str) -> Result<Option<Message>> {
        let mut rows = self
            .conn()?
            .query(
                format!(
                    "SELECT {COLUMNS} FROM messages WHERE contact = ?1 \
                     ORDER BY at DESC, id DESC LIMIT 1"
                ),
                vec![Value::Text(contact.to_string())],
            )
            .await?;
        match rows.next().await? {
            Some(row) => Ok(Some(row_to_message(&row)?)),
            None => Ok(None),
        }
    }

    /// When `contact` last wrote to this number.
    pub async fn last_received_at(&self, contact: &str) -> Result<Option<String>> {
        let mut rows = self
            .conn()?
            .query(
                "SELECT max(at) FROM messages WHERE contact = ?1 AND direction = 'in'",
                vec![Value::Text(contact.to_string())],
            )
            .await?;
        match rows.next().await? {
            Some(row) => text(&row, 0),
            None => Ok(None),
        }
    }

    /// Up to `limit` messages with `contact`, newest first, older than the
    /// message `before` (a message ID from a previous page) if given.
    pub async fn history(
        &self,
        contact: &str,
        before: Option<&str>,
        limit: u32,
    ) -> Result<Vec<Message>> {
        let conn = self.conn()?;
        let mut sql = format!("SELECT {COLUMNS} FROM messages WHERE contact = ?1");
        let mut params = vec![Value::Text(contact.to_string())];
        if let Some(before) = before {
            let mut rows = conn
                .query(
                    "SELECT at FROM messages WHERE id = ?1 AND contact = ?2",
                    vec![
                        Value::Text(before.to_string()),
                        Value::Text(contact.to_string()),
                    ],
                )
                .await?;
            let Some(row) = rows.next().await? else {
                bail!("no message {before:?} in this conversation");
            };
            let at = text(&row, 0)?.context("message has no time")?;
            drop(rows);
            sql.push_str(" AND (at < ?2 OR (at = ?2 AND id < ?3))");
            params.push(Value::Text(at));
            params.push(Value::Text(before.to_string()));
        }
        sql.push_str(&format!(" ORDER BY at DESC, id DESC LIMIT {limit}"));
        let mut rows = conn.query(sql, params).await?;
        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            out.push(row_to_message(&row)?);
        }
        Ok(out)
    }
}

fn status_rank(status: &str) -> Option<u8> {
    match status {
        "sent" => Some(1),
        "delivered" => Some(2),
        "read" => Some(3),
        "failed" => Some(4),
        _ => None,
    }
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Converts a Cloud API timestamp (Unix seconds, as a string) to the log's format.
pub fn from_unix(timestamp: &str) -> Option<String> {
    let secs: i64 = timestamp.parse().ok()?;
    let at = chrono::DateTime::from_timestamp(secs, 0)?;
    Some(at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

fn row_to_message(row: &turso::Row) -> Result<Message> {
    Ok(Message {
        id: required(row, 0)?,
        direction: required(row, 1)?,
        kind: required(row, 2)?,
        text: text(row, 3)?,
        at: required(row, 4)?,
        status: text(row, 5)?,
        status_at: text(row, 6)?,
        error: text(row, 7)?,
    })
}

fn text(row: &turso::Row, i: usize) -> Result<Option<String>> {
    match row.get_value(i)? {
        Value::Text(s) => Ok(Some(s)),
        Value::Null => Ok(None),
        other => bail!("column {i}: expected text, got {other:?}"),
    }
}

fn required(row: &turso::Row, i: usize) -> Result<String> {
    text(row, i)?.with_context(|| format!("column {i} is null"))
}

fn opt_text(s: Option<&str>) -> Value {
    match s {
        Some(s) => Value::Text(s.to_string()),
        None => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> Store {
        let dir = std::env::temp_dir().join(format!(
            "whatsapp-store-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let _ = std::fs::remove_file(&path);
        Store::open(path.to_str().unwrap()).await.unwrap()
    }

    fn incoming<'a>(id: &'a str, text: &'a str, at: &str) -> Incoming<'a> {
        Incoming {
            id,
            contact: "15551234567",
            kind: "text",
            text: Some(text),
            at: at.to_string(),
        }
    }

    #[tokio::test]
    async fn logs_both_directions_and_pages_history() -> Result<()> {
        let s = store().await;
        assert!(
            s.record_incoming(incoming("a", "hi", "2026-09-28T10:00:00Z"))
                .await?
        );
        // Redelivery is recognized.
        assert!(
            !s.record_incoming(incoming("a", "hi", "2026-09-28T10:00:00Z"))
                .await?
        );
        s.record_incoming(incoming("b", "there", "2026-09-28T10:00:00Z"))
            .await?;
        s.record_outgoing("c", "15551234567", "hello", "test")
            .await?;
        assert_eq!(s.count().await?, 3);

        let page = s.history("15551234567", None, 2).await?;
        assert_eq!(page.len(), 2);
        assert_eq!(page[0].id, "c");
        assert_eq!(page[0].status.as_deref(), Some("sent"));
        assert_eq!(page[1].id, "b");
        // Same timestamp as `b`: the ID breaks the tie, so nothing is skipped.
        let next = s.history("15551234567", Some("b"), 2).await?;
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].id, "a");

        assert_eq!(
            s.last_received_at("15551234567").await?.as_deref(),
            Some("2026-09-28T10:00:00Z")
        );
        assert_eq!(s.last_message("15551234567").await?.unwrap().id, "c");
        assert!(s.last_message("19999999999").await?.is_none());
        assert!(s.history("15551234567", Some("nope"), 2).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn statuses_only_move_forward() -> Result<()> {
        let s = store().await;
        s.record_outgoing("m", "15551234567", "hello", "test")
            .await?;
        s.record_status("m", "read", "2026-09-28T10:02:00Z".into(), None)
            .await?;
        s.record_status("m", "delivered", "2026-09-28T10:01:00Z".into(), None)
            .await?;
        let m = s.last_message("15551234567").await?.unwrap();
        assert_eq!(m.status.as_deref(), Some("read"));
        s.record_status("m", "failed", "2026-09-28T10:03:00Z".into(), Some("oops"))
            .await?;
        let m = s.last_message("15551234567").await?.unwrap();
        assert_eq!(m.status.as_deref(), Some("failed"));
        assert_eq!(m.error.as_deref(), Some("oops"));
        // Unknown messages and statuses are ignored.
        s.record_status("other", "read", "2026-09-28T10:04:00Z".into(), None)
            .await?;
        s.record_status("m", "weird", "2026-09-28T10:04:00Z".into(), None)
            .await?;
        Ok(())
    }

    #[test]
    fn converts_unix_timestamps() {
        assert_eq!(
            from_unix("1790632800").as_deref(),
            Some("2026-09-28T22:00:00Z")
        );
        assert!(from_unix("soon").is_none());
    }
}
