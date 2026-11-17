//! Socket Mode: an outbound WebSocket to Slack, so the bot needs no
//! public URL.
//!
//! Checked against docs.slack.dev ("Using Socket Mode") on 2026-11-17:
//! `apps.connections.open` gives a fresh `wss://` URL; Slack greets with
//! `hello`; every other message is an envelope
//! `{envelope_id, type, payload, accepts_response_payload}` (events add
//! `retry_attempt` and `retry_reason`) that must be acknowledged by
//! sending back `{"envelope_id": …}`; `disconnect` (`warning`,
//! `refresh_requested`, `link_disabled`) asks for a new connection, which
//! happens every few hours. That page names no deadline for the
//! acknowledgement; Slack's Bolt guides give three seconds. So each
//! envelope is acknowledged before it is handled, and handling runs in a
//! task of its own. Pings are answered by tungstenite while it reads.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt as _, StreamExt as _};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

use crate::bot::Bot;

/// An envelope, or Slack's own `hello` and `disconnect`.
#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    envelope_id: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
    #[serde(default)]
    retry_attempt: Option<u32>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error(transparent)]
    Slack(#[from] crate::slack::Error),
    #[error("websocket: {0}")]
    Socket(#[from] tokio_tungstenite::tungstenite::Error),
}

/// Keep a Socket Mode connection open for as long as the bot runs:
/// reconnect on `disconnect`, on a closed socket, and — after a pause
/// that grows to half a minute — on errors. `first` is a URL already
/// opened by the start-up check.
pub async fn run(bot: Arc<Bot>, first: Option<String>) {
    let connector = connector();
    let mut url = first;
    let mut pause = Duration::from_secs(1);
    loop {
        let result = match url.take() {
            Some(url) => connection(&bot, &url, &connector).await,
            None => match bot.slack().open_socket().await {
                Ok(url) => connection(&bot, &url, &connector).await,
                Err(e) => Err(e.into()),
            },
        };
        match result {
            Ok(()) => pause = Duration::from_secs(1),
            Err(e) => {
                tracing::warn!(error = %e, retry_in_secs = pause.as_secs(), "socket mode connection failed");
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// One connection, until Slack asks for a new one or it breaks.
async fn connection(bot: &Arc<Bot>, url: &str, connector: &Connector) -> Result<(), Error> {
    let (mut socket, _) =
        connect_async_tls_with_config(url, None, true, Some(connector.clone())).await?;
    while let Some(message) = socket.next().await {
        let text = match message? {
            Message::Text(text) => text,
            Message::Close(_) => return Ok(()),
            _ => continue,
        };
        let envelope: Envelope = match serde_json::from_str(text.as_str()) {
            Ok(envelope) => envelope,
            Err(e) => {
                tracing::warn!(error = %e, "unreadable socket mode message");
                continue;
            }
        };
        match envelope.kind.as_str() {
            "hello" => tracing::info!("socket mode connected"),
            "disconnect" => {
                tracing::info!(
                    reason = envelope.reason.as_deref().unwrap_or("none"),
                    "slack asked to reconnect"
                );
                return Ok(());
            }
            kind => {
                if let Some(id) = &envelope.envelope_id {
                    socket
                        .send(Message::text(json!({ "envelope_id": id }).to_string()))
                        .await?;
                }
                if let Some(attempt) = envelope.retry_attempt.filter(|a| *a > 0) {
                    tracing::debug!(attempt, "slack redelivered an envelope");
                }
                if let Some(payload) = envelope.payload {
                    bot.dispatch(kind, payload);
                }
            }
        }
    }
    Ok(())
}

/// TLS for `wss://`: the system's roots and an explicit crypto provider.
/// rustls has no process default when both of its providers are compiled
/// in, as they are in this workspace, and tungstenite would ask for one.
fn connector() -> Connector {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for error in &native.errors {
        tracing::warn!(error = %error, "a system root certificate could not be loaded");
    }
    let (added, ignored) = roots.add_parsable_certificates(native.certs);
    tracing::debug!(added, ignored, "system root certificates");
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("aws-lc-rs supports the default protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth();
    Connector::Rustls(Arc::new(config))
}
