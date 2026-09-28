//! The WhatsApp Cloud API: sending text messages from the business number.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

const API_URL: &str = "https://graph.facebook.com/v26.0";
// The Cloud API's error code for a free-form message sent more than 24 hours
// after the recipient last wrote to us.
const OUTSIDE_WINDOW: i64 = 131047;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    access_token: String,
    phone_number_id: String,
}

impl Client {
    pub fn new(access_token: String, phone_number_id: String) -> Self {
        // IPv4 only: sends from the Hetzner VM's IPv6 address fail with
        // (#131005) Access denied, while the same token works over IPv4.
        let http = reqwest::Client::builder()
            .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
            .build()
            .expect("building the HTTP client");
        Self {
            http,
            access_token,
            phone_number_id,
        }
    }

    /// Sends `text` to `to` (digits with country code) and returns WhatsApp's message ID.
    pub async fn send_text(&self, to: &str, text: &str) -> Result<String> {
        let body = json!({
            "messaging_product": "whatsapp",
            "to": to,
            "type": "text",
            "text": { "body": text }
        });
        let resp = self
            .http
            .post(format!("{API_URL}/{}/messages", self.phone_number_id))
            .bearer_auth(&self.access_token)
            .json(&body)
            .send()
            .await
            .context("sending to the WhatsApp API")?;
        let status = resp.status();
        let reply: Value = resp
            .json()
            .await
            .context("reading the WhatsApp API's reply")?;
        if !status.is_success() {
            let message = reply["error"]["message"]
                .as_str()
                .unwrap_or("unknown error");
            if reply["error"]["code"].as_i64() == Some(OUTSIDE_WINDOW) {
                bail!(
                    "WhatsApp only delivers free-form messages within 24 hours of the contact's last message to this number, and that window has closed; they need to message it first ({message})"
                );
            }
            bail!("WhatsApp API returned {status}: {message}");
        }
        let id = reply["messages"][0]["id"]
            .as_str()
            .context("WhatsApp API reply had no message ID")?;
        Ok(id.to_string())
    }
}
