//! The Slack Web API, the few methods the bot calls.
//!
//! Checked against docs.slack.dev on 2026-11-17:
//!
//! - `auth.test` (no scope): who the bot token is — `team`, `team_id`,
//!   `user_id`, `bot_id`.
//! - `apps.connections.open` (app-level token in the `Authorization`
//!   header, `connections:write`): a fresh Socket Mode URL.
//! - `conversations.replies` (`channels:history`, `groups:history`):
//!   cursor-paged, `limit` under 1,000 starts paging and 200 is the
//!   recommended page; the parent comes first.
//! - `chat.postMessage` (`chat:write`): a user id as `channel` posts to
//!   that user's App Home Messages tab, opening the direct message. Text
//!   is best kept under 4,000 characters.
//! - `reactions.add` (`reactions:write`): the emoji name without colons.
//! - `users.info` (`users:read`).
//! - `views.publish` (no scope): the App Home tab.
//!
//! A failure arrives as HTTP 200 with `ok: false` and an `error` code; a
//! rate limit as HTTP 429 with `Retry-After` in seconds, which is
//! honored a few times before giving up.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

/// How many times a rate-limited call waits and tries again.
const RETRIES: usize = 3;

/// The longest `Retry-After` the bot waits inside one request.
const LONGEST_WAIT: Duration = Duration::from_secs(30);

/// One page of thread replies (Slack recommends at most 200).
const PAGE: &str = "200";

/// A failed Web API call.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Slack answered `ok: false`.
    #[error("slack {method} refused: {code}")]
    Refused { method: &'static str, code: String },
    /// Slack kept answering 429.
    #[error("slack {method} is rate limited")]
    RateLimited { method: &'static str },
    /// The request did not complete, or the answer was not JSON.
    #[error("slack {method} failed: {source}")]
    Http {
        method: &'static str,
        #[source]
        source: reqwest::Error,
    },
}

impl Error {
    /// The `error` code Slack gave, when it gave one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Error::Refused { code, .. } => Some(code),
            _ => None,
        }
    }
}

/// The Web API client: one root URL and the two tokens.
#[derive(Clone)]
pub struct Slack {
    http: reqwest::Client,
    api: String,
    bot_token: String,
    app_token: String,
}

/// What `auth.test` says about the bot token.
#[derive(Debug, Clone, Deserialize)]
pub struct Identity {
    /// The workspace's name.
    pub team: String,
    pub team_id: String,
    /// The bot user's name.
    pub user: String,
    /// The bot user's id: mentions of the bot read `<@user_id>`.
    pub user_id: String,
    /// The bot's id: messages the bot posted carry it.
    #[serde(default)]
    pub bot_id: Option<String>,
}

/// A message of a thread, as `conversations.replies` returns it.
#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub ts: String,
    #[serde(default)]
    pub text: String,
    /// The author, when a person (or the bot user) posted it.
    #[serde(default)]
    pub user: Option<String>,
    /// Present when an app posted it.
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    /// An app's display name on a legacy integration post.
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub bot_profile: Option<BotProfile>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BotProfile {
    #[serde(default)]
    pub name: Option<String>,
}

/// A person, as `users.info` describes them.
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: String,
    /// The handle.
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub real_name: Option<String>,
    #[serde(default)]
    pub profile: Profile,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Profile {
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub real_name: Option<String>,
}

impl User {
    /// The name people see: the display name, else the real name, else
    /// the handle.
    pub fn shown(&self) -> String {
        [
            self.profile.display_name.as_deref(),
            self.profile.real_name.as_deref(),
            self.real_name.as_deref(),
            Some(self.name.as_str()),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|n| !n.is_empty())
        .unwrap_or(self.id.as_str())
        .to_string()
    }
}

/// How a call is sent.
enum Body {
    /// `GET` with these query parameters.
    Query(Vec<(&'static str, String)>),
    /// `POST` form-encoded (also for calls with no arguments).
    Form(Vec<(&'static str, String)>),
    /// `POST` JSON.
    Json(Value),
}

impl Slack {
    pub fn new(api: &str, bot_token: &str, app_token: &str) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("a reqwest client with a timeout builds"),
            api: api.trim_end_matches('/').to_string(),
            bot_token: bot_token.to_string(),
            app_token: app_token.to_string(),
        }
    }

    /// Who the bot token is — the start-up check of the token.
    pub async fn auth_test(&self) -> Result<Identity, Error> {
        let value = self
            .call("auth.test", &self.bot_token, Body::Form(Vec::new()))
            .await?;
        serde_json::from_value(value).map_err(|_| Error::Refused {
            method: "auth.test",
            code: "unexpected_response".into(),
        })
    }

    /// A fresh Socket Mode URL — with the app-level token, so this is
    /// also its start-up check.
    pub async fn open_socket(&self) -> Result<String, Error> {
        let value = self
            .call(
                "apps.connections.open",
                &self.app_token,
                Body::Form(Vec::new()),
            )
            .await?;
        value["url"]
            .as_str()
            .map(str::to_string)
            .ok_or(Error::Refused {
                method: "apps.connections.open",
                code: "no_url".into(),
            })
    }

    /// Every message of the thread that `ts` belongs to, oldest first,
    /// every page.
    pub async fn replies(&self, channel: &str, ts: &str) -> Result<Vec<Message>, Error> {
        let mut messages = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut query = vec![
                ("channel", channel.to_string()),
                ("ts", ts.to_string()),
                ("limit", PAGE.to_string()),
            ];
            if let Some(cursor) = cursor.take() {
                query.push(("cursor", cursor));
            }
            let mut page = self
                .call("conversations.replies", &self.bot_token, Body::Query(query))
                .await?;
            let batch: Vec<Message> =
                serde_json::from_value(page["messages"].take()).map_err(|_| Error::Refused {
                    method: "conversations.replies",
                    code: "unexpected_response".into(),
                })?;
            messages.extend(batch);
            match page["response_metadata"]["next_cursor"].as_str() {
                Some(next) if !next.is_empty() => cursor = Some(next.to_string()),
                _ => break,
            }
        }
        Ok(messages)
    }

    /// A message to one person, in the bot's direct message with them.
    pub async fn post_to_person(&self, user: &str, text: &str) -> Result<(), Error> {
        self.call(
            "chat.postMessage",
            &self.bot_token,
            Body::Json(json!({
                "channel": user,
                "text": text,
                "unfurl_links": false,
                "unfurl_media": false,
            })),
        )
        .await
        .map(drop)
    }

    /// A reaction on one message. Reacting twice is not an error.
    pub async fn react(&self, channel: &str, ts: &str, name: &str) -> Result<(), Error> {
        match self
            .call(
                "reactions.add",
                &self.bot_token,
                Body::Json(json!({ "channel": channel, "timestamp": ts, "name": name })),
            )
            .await
        {
            Err(e) if e.code() == Some("already_reacted") => Ok(()),
            other => other.map(drop),
        }
    }

    /// One person's profile.
    pub async fn user(&self, id: &str) -> Result<User, Error> {
        let mut value = self
            .call(
                "users.info",
                &self.bot_token,
                Body::Query(vec![("user", id.to_string())]),
            )
            .await?;
        serde_json::from_value(value["user"].take()).map_err(|_| Error::Refused {
            method: "users.info",
            code: "unexpected_response".into(),
        })
    }

    /// Replace one person's App Home tab.
    pub async fn publish_home(&self, user: &str, view: Value) -> Result<(), Error> {
        self.call(
            "views.publish",
            &self.bot_token,
            Body::Json(json!({ "user_id": user, "view": view })),
        )
        .await
        .map(drop)
    }

    /// One Web API call, with the rate-limit wait and the `ok` check.
    async fn call(&self, method: &'static str, token: &str, body: Body) -> Result<Value, Error> {
        let url = format!("{}/{method}", self.api);
        for _ in 0..=RETRIES {
            let request = match &body {
                Body::Query(query) => self.http.get(&url).query(query),
                Body::Form(form) => self.http.post(&url).form(form),
                Body::Json(value) => self.http.post(&url).json(value),
            };
            let response = request
                .bearer_auth(token)
                .send()
                .await
                .map_err(|source| Error::Http { method, source })?;
            if response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let wait = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .map(Duration::from_secs)
                    .unwrap_or(Duration::from_secs(1))
                    .min(LONGEST_WAIT);
                tracing::warn!(method, wait_secs = wait.as_secs(), "slack rate limit");
                tokio::time::sleep(wait).await;
                continue;
            }
            let value: Value = response
                .error_for_status()
                .map_err(|source| Error::Http { method, source })?
                .json()
                .await
                .map_err(|source| Error::Http { method, source })?;
            if value["ok"].as_bool() != Some(true) {
                return Err(Error::Refused {
                    method,
                    code: value["error"].as_str().unwrap_or("unknown").to_string(),
                });
            }
            return Ok(value);
        }
        Err(Error::RateLimited { method })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shown_name_prefers_the_display_name() {
        let user: User = serde_json::from_value(json!({
            "id": "U1", "name": "alice.k", "real_name": "Alice K",
            "profile": { "display_name": "", "real_name": "Alice Kay" },
        }))
        .unwrap();
        assert_eq!(user.shown(), "Alice Kay");
        let user: User = serde_json::from_value(json!({ "id": "U1", "name": "alice.k" })).unwrap();
        assert_eq!(user.shown(), "alice.k");
    }
}
