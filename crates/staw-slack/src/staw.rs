//! Staw, as the bot may reach it: the OAuth endpoints (registration, the
//! device grant, refresh), `/mcp` with the asking person's token, and two
//! reads — `GET /api/v1/users/me` to say whom a person connected as, and
//! the health probe at start. Nothing else of the REST API.
//!
//! The device grant is the CLI's flow (`staw-cli/src/device.rs`): RFC
//! 8414 discovery, RFC 7591 registration of a device-only client, RFC
//! 8628 polling. Here every person gets their own client, named after
//! the Slack account, so that the token Staw issues is named after it
//! too (`connector:<client name>` in the person's Staw settings).

use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation, InitializeResult,
};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{RoleClient, ServiceExt};
use serde::Deserialize;
use serde_json::Value;

/// The MCP client's name. Staw names the agent of a write after it.
pub const CLIENT_NAME: &str = "staw-slack";

/// The device grant's `grant_type` URN (RFC 8628 §3.4).
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// The bound on one HTTP exchange with Staw.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// A Staw call that failed.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Staw was not reached, or answered with an unexpected status.
    #[error("staw {0} failed: {1}")]
    Http(&'static str, String),
    /// The OAuth server refused (RFC 6749 §5.2 `error`).
    #[error("staw refused {0}: {1}")]
    Refused(&'static str, String),
    /// Staw answered with something the bot cannot read.
    #[error("staw {0}: unexpected answer: {1}")]
    Shape(&'static str, String),
    /// An MCP session or tool call failed.
    #[error("staw mcp {0}: {1}")]
    Mcp(&'static str, String),
}

/// The OAuth endpoints, from Staw's RFC 8414 metadata.
#[derive(Debug, Clone)]
pub struct Oauth {
    register: String,
    device: String,
    token: String,
}

/// Staw: its base URL and HTTP clients.
#[derive(Clone)]
pub struct Staw {
    http: reqwest::Client,
    /// For `/mcp`: without idle pooling, as rmcp's own default client —
    /// reusing a connection whose last body was not read to the end
    /// stalls on delayed ACKs.
    mcp: reqwest::Client,
    base: String,
}

/// An opened device grant (RFC 8628 §3.2).
#[derive(Debug, Clone, Deserialize)]
pub struct Opened {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default = "five")]
    pub interval: u64,
}

fn five() -> u64 {
    5
}

impl Opened {
    /// The link a person opens: with the code in it when Staw offers one.
    pub fn link(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

/// A token grant: the access token and, from the device grant, the
/// refresh token.
#[derive(Debug, Clone, Deserialize)]
pub struct Grant {
    pub access_token: String,
    /// Seconds.
    pub expires_in: u64,
    #[serde(default)]
    pub refresh_token: Option<String>,
}

/// One poll of a device grant.
#[derive(Debug)]
pub enum Poll {
    Granted(Grant),
    Pending,
    SlowDown,
    Denied,
    Expired,
}

/// Why a refresh failed.
#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    /// The refresh token is unknown or revoked: the person must connect
    /// again.
    #[error("the refresh token was revoked")]
    Revoked,
    #[error(transparent)]
    Staw(#[from] Error),
}

/// `GET /api/v1/users/me`, the fields the bot shows.
#[derive(Debug, Clone, Deserialize)]
pub struct Me {
    pub name: String,
    #[serde(default)]
    pub handle: String,
}

#[derive(Deserialize)]
struct Discovery {
    registration_endpoint: Option<String>,
    token_endpoint: Option<String>,
    device_authorization_endpoint: Option<String>,
    #[serde(default)]
    grant_types_supported: Vec<String>,
}

#[derive(Deserialize)]
struct Refusal {
    error: String,
}

impl Staw {
    pub fn new(base: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .build()
                .expect("a reqwest client with a timeout builds"),
            mcp: reqwest::Client::builder()
                .timeout(HTTP_TIMEOUT)
                .pool_max_idle_per_host(0)
                .build()
                .expect("a reqwest client with a timeout builds"),
            base: base.trim_end_matches('/').to_string(),
        }
    }

    /// The base URL, for links people open.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// The start-up check: Staw answers its health probe.
    pub async fn health(&self) -> Result<(), Error> {
        let what = "health check";
        let value: Value = self
            .http
            .get(format!("{}/api/v1/healthz", self.base))
            .send()
            .await
            .map_err(|e| Error::Http(what, e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Http(what, e.to_string()))?
            .json()
            .await
            .map_err(|e| Error::Shape(what, e.to_string()))?;
        match value["status"].as_str() {
            Some("ok") => Ok(()),
            _ => Err(Error::Shape(what, "no status ok".into())),
        }
    }

    /// The OAuth endpoints. Staw must offer the device grant: it is how
    /// a person connects.
    pub async fn oauth(&self) -> Result<Oauth, Error> {
        let what = "oauth discovery";
        let discovery: Discovery = self
            .http
            .get(format!(
                "{}/.well-known/oauth-authorization-server",
                self.base
            ))
            .send()
            .await
            .map_err(|e| Error::Http(what, e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Http(what, e.to_string()))?
            .json()
            .await
            .map_err(|e| Error::Shape(what, e.to_string()))?;
        let missing = || {
            Error::Shape(
                what,
                "this Staw does not offer the OAuth device grant, which the bot needs to \
                 connect people"
                    .into(),
            )
        };
        if !discovery
            .grant_types_supported
            .iter()
            .any(|g| g == DEVICE_GRANT)
        {
            return Err(missing());
        }
        Ok(Oauth {
            register: discovery.registration_endpoint.ok_or_else(missing)?,
            device: discovery
                .device_authorization_endpoint
                .ok_or_else(missing)?,
            token: discovery.token_endpoint.ok_or_else(missing)?,
        })
    }

    /// Register a device-only client; its name is what Staw shows on the
    /// approval page and in the token's label.
    pub async fn register(&self, oauth: &Oauth, name: &str) -> Result<String, Error> {
        let what = "client registration";
        let value: Value = self
            .http
            .post(&oauth.register)
            .json(&serde_json::json!({
                "client_name": name,
                "grant_types": [DEVICE_GRANT, "refresh_token"],
            }))
            .send()
            .await
            .map_err(|e| Error::Http(what, e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Http(what, e.to_string()))?
            .json()
            .await
            .map_err(|e| Error::Shape(what, e.to_string()))?;
        value["client_id"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| Error::Shape(what, "no client_id".into()))
    }

    /// Open a device grant for `client_id`.
    pub async fn device(&self, oauth: &Oauth, client_id: &str) -> Result<Opened, Error> {
        let what = "device authorization";
        self.http
            .post(&oauth.device)
            .form(&[("client_id", client_id)])
            .send()
            .await
            .map_err(|e| Error::Http(what, e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Http(what, e.to_string()))?
            .json()
            .await
            .map_err(|e| Error::Shape(what, e.to_string()))
    }

    /// One poll of the token endpoint for a device grant.
    pub async fn poll(
        &self,
        oauth: &Oauth,
        client_id: &str,
        device_code: &str,
    ) -> Result<Poll, Error> {
        let response = self
            .http
            .post(&oauth.token)
            .form(&[
                ("grant_type", DEVICE_GRANT),
                ("device_code", device_code),
                ("client_id", client_id),
            ])
            .send()
            .await
            .map_err(|e| Error::Http("device poll", e.to_string()))?;
        if response.status().is_success() {
            return response
                .json()
                .await
                .map(Poll::Granted)
                .map_err(|e| Error::Shape("device poll", e.to_string()));
        }
        let refusal: Refusal = response
            .json()
            .await
            .map_err(|e| Error::Shape("device poll", e.to_string()))?;
        match refusal.error.as_str() {
            "authorization_pending" => Ok(Poll::Pending),
            "slow_down" => Ok(Poll::SlowDown),
            "access_denied" => Ok(Poll::Denied),
            "expired_token" => Ok(Poll::Expired),
            other => Err(Error::Refused("the device poll", other.to_string())),
        }
    }

    /// A new access token for a refresh token.
    pub async fn refresh(&self, oauth: &Oauth, refresh: &str) -> Result<Grant, RefreshError> {
        let response = self
            .http
            .post(&oauth.token)
            .form(&[("grant_type", "refresh_token"), ("refresh_token", refresh)])
            .send()
            .await
            .map_err(|e| Error::Http("token refresh", e.to_string()))?;
        if response.status().is_success() {
            return response
                .json()
                .await
                .map_err(|e| Error::Shape("token refresh", e.to_string()).into());
        }
        let status = response.status();
        match response.json::<Refusal>().await {
            Ok(refusal) if refusal.error == "invalid_grant" => Err(RefreshError::Revoked),
            Ok(refusal) => Err(Error::Refused("the token refresh", refusal.error).into()),
            Err(_) => Err(Error::Http("token refresh", status.to_string()).into()),
        }
    }

    /// Whom an access token belongs to.
    pub async fn me(&self, access: &str) -> Result<Me, Error> {
        let what = "users/me";
        self.http
            .get(format!("{}/api/v1/users/me", self.base))
            .bearer_auth(access)
            .send()
            .await
            .map_err(|e| Error::Http(what, e.to_string()))?
            .error_for_status()
            .map_err(|e| Error::Http(what, e.to_string()))?
            .json()
            .await
            .map_err(|e| Error::Shape(what, e.to_string()))
    }

    /// Open an MCP session as the owner of `access`. Staw's `/mcp` is
    /// stateless, so the session is a handshake and the calls that
    /// follow, each a POST with the same bearer.
    pub async fn mcp(&self, access: &str) -> Result<Mcp, Error> {
        let transport = StreamableHttpClientTransport::with_client(
            self.mcp.clone(),
            StreamableHttpClientTransportConfig::with_uri(format!("{}/mcp", self.base))
                .auth_header(access),
        );
        let info = ClientInfo::new(
            ClientCapabilities::default(),
            Implementation::new(CLIENT_NAME, env!("CARGO_PKG_VERSION")),
        );
        let service = tokio::time::timeout(HTTP_TIMEOUT, info.serve(transport))
            .await
            .map_err(|_| Error::Mcp("initialize", "timed out".into()))?
            .map_err(|e| Error::Mcp("initialize", e.to_string()))?;
        Ok(Mcp { service })
    }
}

/// One MCP session, as one person.
pub struct Mcp {
    service: RunningService<RoleClient, ClientInfo>,
}

/// A tool call that Staw refused: the message is Staw's, for the model
/// to read and recover from (a wrong id, say).
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Staw(#[from] Error),
}

impl Mcp {
    /// The server's instructions to agents, from the handshake.
    pub fn instructions(&self) -> Option<String> {
        self.service
            .peer_info()
            .and_then(|info: std::sync::Arc<InitializeResult>| info.instructions.clone())
    }

    /// Call one tool; its text result parsed as JSON.
    pub async fn call(&self, tool: &'static str, arguments: Value) -> Result<Value, ToolError> {
        let arguments = match arguments {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        let request = CallToolRequestParams::new(tool).with_arguments(arguments);
        let result = tokio::time::timeout(HTTP_TIMEOUT, self.service.call_tool(request))
            .await
            .map_err(|_| Error::Mcp(tool, "timed out".into()))?
            .map_err(|e| match e {
                rmcp::ServiceError::McpError(data) => ToolError::Refused(data.message.into()),
                other => Error::Mcp(tool, other.to_string()).into(),
            })?;
        let text: String = result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
            .collect();
        if result.is_error == Some(true) {
            return Err(ToolError::Refused(text));
        }
        serde_json::from_str(&text).map_err(|e| Error::Shape(tool, e.to_string()).into())
    }

    /// End the session (dropping it does the same, without waiting).
    pub async fn close(self) {
        let _ = self.service.cancel().await;
    }
}
