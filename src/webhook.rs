//! `/webhook`: incoming messages from the WhatsApp Cloud API. Meta signs each
//! delivery with the app secret; unsigned or badly signed requests are
//! rejected. Messages from anyone who isn't a contact are dropped without a
//! reply, so a stranger can't tell the number is live. Contacts' messages are
//! logged, and text messages are forwarded to the agent's input endpoint, if
//! one is configured; once the agent has them, they're marked read. Delivery
//! statuses for messages this number sent are logged too.

use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
};
use hmac::{Hmac, KeyInit, Mac};

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Sha256;

use crate::{
    contacts::Contacts,
    store::{self, Incoming, Store},
    whatsapp,
};

#[derive(Clone)]
pub struct Webhook(Arc<Inner>);

struct Inner {
    app_secret: String,
    verify_token: String,
    contacts: Arc<Contacts>,
    agent: Option<Agent>,
    store: Store,
    whatsapp: whatsapp::Client,
    http: reqwest::Client,
}

pub struct Agent {
    /// Where incoming messages are POSTed.
    pub url: String,
    /// Bearer token for the agent's input endpoint.
    pub token: String,
}

impl Webhook {
    pub fn new(
        app_secret: String,
        verify_token: String,
        contacts: Arc<Contacts>,
        agent: Option<Agent>,
        store: Store,
        whatsapp: whatsapp::Client,
    ) -> Self {
        Self(Arc::new(Inner {
            app_secret,
            verify_token,
            contacts,
            agent,
            store,
            whatsapp,
            http: reqwest::Client::new(),
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

/// `POST /webhook`: a delivery from Meta. Answers once everything is logged;
/// forwarding to the agent happens in the background, since an agent turn can
/// take minutes. If logging fails, Meta gets a 500 and redelivers later.
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
        let text = message["text"]["body"].as_str();
        // Set when the contact quoted a message in their reply.
        let reply_to = message["context"]["id"].as_str();
        let at = message["timestamp"]
            .as_str()
            .and_then(store::from_unix)
            .unwrap_or_else(store::now);
        let incoming = Incoming {
            id,
            contact: from,
            kind,
            text,
            at,
            reply_to,
        };
        match w.store.record_incoming(incoming).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::info!(id, "ignored redelivered message");
                continue;
            }
            Err(e) => {
                tracing::error!(id, "logging message failed: {e:#}");
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
        }
        let Some(text) = text else {
            tracing::info!(id, kind, "logged non-text message; not forwarded");
            continue;
        };
        let mut input = json!({
            "channel": "whatsapp",
            "sender": from,
            "sender_name": contact.name,
            "message_id": id,
            "text": text,
        });
        if let Some(reply_to) = reply_to {
            // Include what was quoted, so the agent needn't look it up.
            let quoted = match w.store.find(from, reply_to).await {
                Ok(Some(m)) => m.text,
                Ok(None) => None,
                Err(e) => {
                    tracing::warn!(id, "looking up quoted message failed: {e:#}");
                    None
                }
            };
            input["reply_to"] = json!({ "message_id": reply_to, "text": quoted });
        }
        let w = w.clone();
        tokio::spawn(async move { forward(&w, input).await });
    }
    for status in statuses(&payload) {
        let id = status["id"].as_str().unwrap_or_default();
        let state = status["status"].as_str().unwrap_or_default();
        let at = status["timestamp"]
            .as_str()
            .and_then(store::from_unix)
            .unwrap_or_else(store::now);
        let error = status["errors"][0]["title"]
            .as_str()
            .or(status["errors"][0]["message"].as_str());
        if let Err(e) = w.store.record_status(id, state, at, error).await {
            tracing::error!(id, "logging status failed: {e:#}");
            return StatusCode::INTERNAL_SERVER_ERROR;
        }
        if state == "failed" {
            tracing::warn!(id, error = error.unwrap_or_default(), "message failed");
        }
    }
    StatusCode::OK
}

/// Every incoming message in a delivery.
fn messages(payload: &Value) -> Vec<&Value> {
    values(payload, "messages")
}

/// Every status update (sent, delivered, read, failed) for messages this
/// number sent.
fn statuses(payload: &Value) -> Vec<&Value> {
    values(payload, "statuses")
}

fn values<'a>(payload: &'a Value, field: &str) -> Vec<&'a Value> {
    let mut out = Vec::new();
    for entry in payload["entry"].as_array().into_iter().flatten() {
        for change in entry["changes"].as_array().into_iter().flatten() {
            for value in change["value"][field].as_array().into_iter().flatten() {
                out.push(value);
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
    if let Err(e) = post_to_agent(w, agent, &input).await {
        tracing::error!(%id, "forwarding to the agent failed: {e}");
        return;
    }
    tracing::info!(%id, "forwarded message to the agent");
    // The agent has it now: show the contact blue ticks.
    let sender = input["sender"].as_str().unwrap_or_default();
    if let Err(e) = mark_read(w, sender, &id).await {
        tracing::warn!(%id, "marking read failed: {e:#}");
    }
}

async fn mark_read(w: &Inner, contact: &str, id: &str) -> anyhow::Result<()> {
    w.whatsapp.mark_read(id, false).await?;
    w.store.mark_read_through(contact, id).await?;
    Ok(())
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
    fn separates_messages_and_statuses() {
        let payload = json!({"entry": [{"changes": [
            {"value": {"messages": [{"from": "1", "id": "a", "type": "text", "text": {"body": "hi"}}]}},
            {"value": {"statuses": [{"id": "b", "status": "read"}]}}
        ]}]});
        let found = messages(&payload);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["id"], "a");
        let found = statuses(&payload);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["id"], "b");
    }
}
