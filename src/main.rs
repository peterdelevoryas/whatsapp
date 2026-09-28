mod auth;
mod server;
mod webhook;
mod whatsapp;

use std::{collections::HashSet, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::never::NeverSessionManager,
};
use tokio::signal::unix::{SignalKind, signal};

const USAGE: &str = "\
usage:
  whatsapp serve            run the MCP server
  whatsapp token <source>   mint a client token

environment:
  WHATSAPP_ACCESS_TOKEN     Cloud API access token (required)
  WHATSAPP_PHONE_NUMBER_ID  the sending number's ID (required)
  WHATSAPP_OWNER            the user's number, digits with country code; the only recipient (required)
  WHATSAPP_APP_SECRET       Meta app secret, for checking webhook signatures (required)
  WHATSAPP_VERIFY_TOKEN     shared secret for Meta's webhook verification (required)
  WHATSAPP_ALLOWED_SENDERS  comma-separated numbers whose messages are accepted (default: WHATSAPP_OWNER)
  WHATSAPP_AGENT_URL        where to forward incoming messages (default: don't forward, just log)
  WHATSAPP_AGENT_TOKEN      bearer token for WHATSAPP_AGENT_URL
  WHATSAPP_TOKENS           client tokens file (default: tokens)
  WHATSAPP_ADDR             listen address (default: 127.0.0.1:8751)
  WHATSAPP_ALLOWED_HOSTS    comma-separated Host headers to accept (default: localhost)";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "whatsapp=info".into()),
        )
        .init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["serve"] => serve().await,
        ["token", source] => {
            if source.is_empty() || source.contains(char::is_whitespace) {
                bail!("source must be a single word, e.g. claude-code");
            }
            let token = auth::generate();
            eprintln!("Token (give this to the client; it is not stored):\n{token}\n");
            eprintln!("Append this line to the tokens file:");
            println!("{} {source}", auth::hash(&token));
            Ok(())
        }
        _ => {
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    }
}

async fn serve() -> Result<()> {
    let env = |k: &str, default: &str| std::env::var(k).unwrap_or_else(|_| default.to_string());
    let required = |k: &str| std::env::var(k).with_context(|| format!("{k} not set"));
    let access_token = required("WHATSAPP_ACCESS_TOKEN")?;
    let phone_number_id = required("WHATSAPP_PHONE_NUMBER_ID")?;
    let owner = required("WHATSAPP_OWNER")?;
    if owner.is_empty() || !owner.chars().all(|c| c.is_ascii_digit()) {
        bail!("WHATSAPP_OWNER must be digits with country code, no + (e.g. 16505551234)");
    }
    let app_secret = required("WHATSAPP_APP_SECRET")?;
    let verify_token = required("WHATSAPP_VERIFY_TOKEN")?;
    let mut allowed_senders = HashSet::new();
    for number in env("WHATSAPP_ALLOWED_SENDERS", &owner).split(',') {
        let number = number.trim();
        if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
            bail!("WHATSAPP_ALLOWED_SENDERS: {number:?} isn't digits with country code");
        }
        allowed_senders.insert(number.to_string());
    }
    let agent = match std::env::var("WHATSAPP_AGENT_URL") {
        Ok(url) => Some(webhook::Agent {
            url,
            token: required("WHATSAPP_AGENT_TOKEN")?,
        }),
        Err(_) => None,
    };
    let tokens_path = PathBuf::from(env("WHATSAPP_TOKENS", "tokens"));
    let addr = env("WHATSAPP_ADDR", "127.0.0.1:8751");
    let mut allowed_hosts = Vec::new();
    for host in env("WHATSAPP_ALLOWED_HOSTS", "localhost,127.0.0.1,::1").split(',') {
        let host = host.trim();
        if !host.is_empty() {
            allowed_hosts.push(host.to_string());
        }
    }

    let whatsapp = whatsapp::Client::new(access_token, phone_number_id);
    tracing::info!(
        allowed_senders = allowed_senders.len(),
        forwarding = agent.is_some(),
        "webhook configured"
    );
    let webhook = webhook::Webhook::new(app_secret, verify_token, allowed_senders, agent);
    let tokens = auth::Tokens::load(&tokens_path)?;

    let mcp = StreamableHttpService::new(
        move || Ok(server::WhatsAppServer::new(whatsapp.clone(), owner.clone())),
        // No sessions: every request stands alone, so a restart never strands a
        // connected client.
        Arc::new(NeverSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_allowed_hosts(allowed_hosts)
            .with_legacy_session_mode(false)
            .with_json_response(true),
    );
    let app = axum::Router::new()
        .nest_service("/mcp", mcp)
        .layer(axum::middleware::from_fn_with_state(
            tokens.clone(),
            auth::middleware,
        ))
        // Unauthenticated, for an external monitor; outside the auth layer.
        .route("/health", axum::routing::get(|| async { "ok\n" }))
        // Authenticated by Meta's signature instead of a bearer token.
        .route(
            "/webhook",
            axum::routing::get(webhook::verify)
                .post(webhook::receive)
                .with_state(webhook),
        );

    // `systemctl reload whatsapp` re-reads the tokens file without a restart.
    let mut hangup = signal(SignalKind::hangup())?;
    tokio::spawn(async move {
        while hangup.recv().await.is_some() {
            match tokens.reload() {
                Ok(n) => tracing::info!(tokens = n, "reloaded tokens"),
                Err(e) => tracing::error!("reloading tokens (keeping the old ones): {e:#}"),
            }
        }
    });

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("listening on http://{addr}/mcp");
    axum::serve(listener, app)
        // Finish in-flight requests on SIGTERM (systemctl stop/restart) or Ctrl-C.
        .with_graceful_shutdown(async {
            let mut term = signal(SignalKind::terminate()).expect("installing SIGTERM handler");
            tokio::select! {
                _ = term.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
