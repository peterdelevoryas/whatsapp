use std::sync::Arc;

use axum::http::request::Parts;
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{
        tool::Extension,
        wrapper::{Json, Parameters},
    },
    model::{Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{auth, contacts::Contacts, whatsapp};

// WhatsApp's limit on a text message body.
const MAX_TEXT_CHARS: usize = 4096;

// Followed by the contact list.
const INSTRUCTIONS: &str = "\
A WhatsApp phone number. whatsapp_send sends a text message from it to one of \
its contacts, by name; it can't message anyone else. Contacts: ";

#[derive(Clone)]
pub struct WhatsAppServer {
    whatsapp: whatsapp::Client,
    contacts: Arc<Contacts>,
}

#[derive(Deserialize, JsonSchema)]
pub struct SendParams {
    /// The contact's name, as listed in the server instructions.
    pub to: String,
    /// The message, up to 4096 characters. Plain text; WhatsApp renders
    /// *bold*, _italic_, and `code`.
    pub text: String,
}

#[derive(Serialize, JsonSchema)]
pub struct Sent {
    /// WhatsApp's ID for the message.
    pub message_id: String,
}

#[tool_router]
impl WhatsAppServer {
    pub fn new(whatsapp: whatsapp::Client, contacts: Arc<Contacts>) -> Self {
        Self { whatsapp, contacts }
    }

    #[tool(
        description = "Send a WhatsApp text message to a contact, by name. Only \
            contacts can be messaged. WhatsApp only delivers these within 24 hours of \
            the contact's last message to this number; outside that window this fails \
            and says so.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        )
    )]
    async fn whatsapp_send(
        &self,
        Parameters(p): Parameters<SendParams>,
        Extension(parts): Extension<Parts>,
    ) -> Result<Json<Sent>, ErrorData> {
        let client = parts
            .extensions
            .get::<auth::Client>()
            .ok_or_else(|| ErrorData::internal_error("request was not authenticated", None))?;
        let chars = p.text.chars().count();
        if chars == 0 || chars > MAX_TEXT_CHARS {
            return Err(ErrorData::invalid_params(
                format!("text must be 1 to {MAX_TEXT_CHARS} characters (got {chars})"),
                None,
            ));
        }
        let Some(contact) = self.contacts.by_name(&p.to) else {
            return Err(ErrorData::invalid_params(
                format!(
                    "{:?} isn't a contact; contacts: {}",
                    p.to,
                    self.contacts.names().join(", ")
                ),
                None,
            ));
        };
        match self.whatsapp.send_text(&contact.number, &p.text).await {
            Ok(message_id) => {
                tracing::info!(source = %client.source, to = %contact.name, %message_id, chars, "sent");
                Ok(Json(Sent { message_id }))
            }
            Err(e) => {
                tracing::warn!(source = %client.source, to = %contact.name, "send failed: {e:#}");
                Err(ErrorData::internal_error(format!("{e:#}"), None))
            }
        }
    }
}

#[tool_handler]
impl ServerHandler for WhatsAppServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("whatsapp", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "{INSTRUCTIONS}{}.",
                self.contacts.names().join(", ")
            ))
    }
}
