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

use crate::{auth, whatsapp};

// WhatsApp's limit on a text message body.
const MAX_TEXT_CHARS: usize = 4096;

const INSTRUCTIONS: &str = "\
Sends the user WhatsApp messages. Use it to reach them when they aren't \
watching this session: a long task finished, a scheduled check-in, something \
that needs their attention. Messages always go to the user; there's no \
recipient to choose.";

#[derive(Clone)]
pub struct WhatsAppServer {
    whatsapp: whatsapp::Client,
    /// The only number this server sends to.
    owner: String,
}

#[derive(Deserialize, JsonSchema)]
pub struct SendParams {
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
    pub fn new(whatsapp: whatsapp::Client, owner: String) -> Self {
        Self { whatsapp, owner }
    }

    #[tool(
        description = "Send the user a WhatsApp message. It always goes to the user; \
            there's no recipient to choose. WhatsApp only delivers these within 24 \
            hours of the user's last message to this number; outside that window \
            this fails and says so.",
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
        match self.whatsapp.send_text(&self.owner, &p.text).await {
            Ok(message_id) => {
                tracing::info!(source = %client.source, %message_id, chars, "sent");
                Ok(Json(Sent { message_id }))
            }
            Err(e) => {
                tracing::warn!(source = %client.source, "send failed: {e:#}");
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
            .with_instructions(INSTRUCTIONS)
    }
}
