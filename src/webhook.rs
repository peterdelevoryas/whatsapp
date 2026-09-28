//! `/webhook`: incoming messages from the WhatsApp Cloud API. Meta signs each
//! delivery with the app secret; unsigned or badly signed requests are
//! rejected. Messages from anyone who isn't a contact are dropped without a
//! reply, so a stranger can't tell the number is live. Contacts' text messages
//! are forwarded to the agent's input endpoint, if one is configured.

use std::{
    collections::{HashSet, VecDeque},
    sync::{Arc, Mutex},
};

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
};
use hmac::{Hmac, KeyInit, Mac};

use crate::contacts::Contacts;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;

// Meta redelivers a message if it doesn't see our 200 in time; remember this
// many recent message IDs so the agent gets each one once.
const SEEN_CAPACITY: usize = 1000;

#[derive(Clone)]
pub struct Webhook(Arc<Inner>);

struct Inner {
    app_secret: String,
    verify_token: String,
    contacts: Arc<Contacts>,
    agent: Option<Agent>,
    http: reqwest::Client,
    seen: Mutex<Seen>,
}

pub struct Agent {
    /// Where incoming messages are POSTed.
    pub url: String,
    /// Bearer token for the agent's input endpoint.
    pub token: String,
}

#[derive(Default)]
struct Seen {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl Seen {
    /// Records `id`; false if it was already seen.
    fn insert(&mut self, id: &str) -> bool {
        if !self.ids.insert(id.to_string()) {
            return false;
        }
        self.order.push_back(id.to_string());
        if self.order.len() > SEEN_CAPACITY {
            let oldest = self.order.pop_front().unwrap();
            self.ids.remove(&oldest);
        }
        true
    }
}

impl Webhook {
    pub fn new(
        app_secret: String,
        verify_token: String,
        contacts: Arc<Contacts>,
        agent: Option<Agent>,
    ) -> Self {
        Self(Arc::new(Inner {
            app_secret,
            verify_token,
            contacts,
            agent,
            http: reqwest::Client::new(),
            seen: Mutex::new(Seen::default()),
        }))
    }
}

#[derive(Deserialize)]
pub struct VerifyParams {
    #[serde(rename = "hub.mode")]
    mode: Option<String>,
    #[serde(rename = "hub.verify_token")]
    verify_token: Option<String>,
    #[serde(rename = "hub.challenge")]
    challenge: Option<String>,
}

/// `GET /webhook`: Meta's one-time check when the callback URL is saved.
pub async fn verify(
    State(Webhook(w)): State<Webhook>,
    Query(p): Query<VerifyParams>,
) -> Result<String, StatusCode> {
    if p.mode.as_deref() != Some("subscribe") || p.verify_token.as_deref() != Some(&w.verify_token)
    {
        tracing::warn!("webhook verification rejected");
        return Err(StatusCode::FORBIDDEN);
    }
    tracing::info!("webhook verified");
    p.challenge.ok_or(StatusCode::BAD_REQUEST)
}

/// `POST /webhook`: a delivery from Meta. Answers right away; forwarding to
/// the agent happens in the background, since an agent turn can take minutes.
pub async fn receive(
    State(Webhook(w)): State<Webhook>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok());
    if !signature_valid(&w.app_secret, &body, signature) {
        tracing::warn!(signed = signature.is_some(), "webhook signature rejected");
        return StatusCode::UNAUTHORIZED;
    }
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(e) => {
            tracing::warn!("webhook body isn't JSON: {e}");
            return StatusCode::BAD_REQUEST;
        }
    };
    for message in messages(&payload) {
        let from = message["from"].as_str().unwrap_or_default();
        let id = message["id"].as_str().unwrap_or_default();
        let kind = message["type"].as_str().unwrap_or_default();
        let Some(contact) = w.contacts.by_number(from) else {
            // Only the last digits: enough to recognize a number, without
            // keeping strangers' full numbers in the logs.
            let suffix = &from[from.len().saturating_sub(4)..];
            tracing::warn!(
                kind,
                sender_suffix = suffix,
                sender_digits = from.len(),
                "dropped message from a sender who isn't a contact"
            );
            continue;
        };
        if !w.seen.lock().unwrap().insert(id) {
            tracing::info!(id, "ignored redelivered message");
            continue;
        }
        let Some(text) = message["text"]["body"].as_str() else {
            tracing::info!(id, kind, "ignored non-text message");
            continue;
        };
        let input = json!({
            "channel": "whatsapp",
            "sender": from,
            "sender_name": contact.name,
            "message_id": id,
            "text": text,
        });
        let w = w.clone();
        tokio::spawn(async move { forward(&w, input).await });
    }
    StatusCode::OK
}

/// Every incoming message in a delivery. Deliveries also carry status updates
/// (sent, delivered, read) for our own messages; those are skipped.
fn messages(payload: &Value) -> Vec<&Value> {
    let mut out = Vec::new();
    for entry in payload["entry"].as_array().into_iter().flatten() {
        for change in entry["changes"].as_array().into_iter().flatten() {
            for message in change["value"]["messages"].as_array().into_iter().flatten() {
                out.push(message);
            }
        }
    }
    out
}

async fn forward(w: &Inner, input: Value) {
    let id = input["message_id"].as_str().unwrap_or_default().to_string();
    let Some(agent) = &w.agent else {
        tracing::info!(%id, "received message; no agent configured, so not forwarded");
        return;
    };
    match post_to_agent(w, agent, &input).await {
        Ok(()) => tracing::info!(%id, "forwarded message to the agent"),
        Err(e) => tracing::error!(%id, "forwarding to the agent failed: {e}"),
    }
}

async fn post_to_agent(w: &Inner, agent: &Agent, input: &Value) -> reqwest::Result<()> {
    let resp = w
        .http
        .post(&agent.url)
        .bearer_auth(&agent.token)
        .json(input)
        .send()
        .await?;
    resp.error_for_status()?;
    Ok(())
}

/// Checks `X-Hub-Signature-256: sha256=<hex>`, an HMAC-SHA256 of the raw body
/// keyed with the app secret. The comparison is constant-time.
fn signature_valid(app_secret: &str, body: &[u8], header: Option<&str>) -> bool {
    let Some(hex_sig) = header.and_then(|h| h.strip_prefix("sha256=")) else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_sig) else {
        return false;
    };
    let mut mac =
        Hmac::<Sha256>::new_from_slice(app_secret.as_bytes()).expect("HMAC takes any key");
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
    }

    #[test]
    fn signatures() {
        let body = br#"{"entry":[]}"#;
        let good = sign("secret", body);
        assert!(signature_valid("secret", body, Some(&good)));
        assert!(!signature_valid("other", body, Some(&good)));
        assert!(!signature_valid("secret", b"tampered", Some(&good)));
        assert!(!signature_valid("secret", body, None));
        assert!(!signature_valid("secret", body, Some("sha256=zz")));
    }

    #[test]
    fn finds_messages_and_skips_statuses() {
        let payload = json!({"entry": [{"changes": [
            {"value": {"messages": [{"from": "1", "id": "a", "type": "text", "text": {"body": "hi"}}]}},
            {"value": {"statuses": [{"id": "b", "status": "read"}]}}
        ]}]});
        let found = messages(&payload);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["id"], "a");
    }

    #[test]
    fn seen_forgets_oldest() {
        let mut seen = Seen::default();
        assert!(seen.insert("a"));
        assert!(!seen.insert("a"));
        for i in 0..SEEN_CAPACITY {
            seen.insert(&i.to_string());
        }
        assert!(seen.insert("a"));
    }
}
