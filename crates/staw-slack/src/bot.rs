//! The bot: start-up checks, the events it handles, and the life of one
//! request.
//!
//! A request is one mention of the bot (`app_mention`). In order:
//!
//! 1. A channel shared with another organization, or one that is not in
//!    the channel map, gets a short direct message and nothing else.
//! 2. The mention gets a reaction: the only thing the bot puts in a
//!    channel (rule 2).
//! 3. A person who has not connected a Staw account gets the connect
//!    message — a link and a code — and the request ends there.
//! 4. Through `/mcp`, as the person, `project_list` must show the mapped
//!    project; when it does not, the person is told so and the model is
//!    not called.
//! 5. The thread is read from Slack, every page, and the model runs its
//!    loop with the person's MCP session.
//! 6. The answer goes to the person in a direct message.
//!
//! Every request ends in a direct message (rule 8); what went wrong
//! goes to the logs, apart from what the person reads (decision
//! `01M2Z18KZDMRZSEDX4KGPM3KXP`). The same event delivered twice is
//! handled once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::agent::{self, Outcome, Scope};
use crate::config::Config;
use crate::people::{self, Begun, People};
use crate::slack::{Identity, Slack};
use crate::staw::{Mcp, Staw};
use crate::store::{Key, Store};
use crate::texts;
use crate::thread;

/// Why the bot did not start. Each says which setting to look at.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("Slack refused the bot token ([slack] bot_token): {0}")]
    BotToken(crate::slack::Error),
    #[error("Slack refused the app-level token ([slack] app_token): {0}")]
    AppToken(crate::slack::Error),
    #[error("Staw at {url} is not usable ([staw] url): {source}")]
    Staw {
        url: String,
        #[source]
        source: crate::staw::Error,
    },
    #[error("the model is not usable ([model]): {0}")]
    Model(staw_expert::Error),
    #[error("the store is not usable ([store] path): {0}")]
    Store(crate::store::Error),
}

/// How long a Slack name is reused before it is looked up again.
const NAME_TTL: Duration = Duration::from_secs(3600);

/// How long an event id is remembered, to drop redeliveries.
const SEEN_TTL: Duration = Duration::from_secs(3600);

pub struct Bot {
    config: Config,
    slack: Slack,
    staw: Staw,
    me: Identity,
    model: staw_expert::Client,
    people: People,
    seen: Mutex<HashMap<String, Instant>>,
    names: Mutex<HashMap<String, (String, Instant)>>,
    first_socket: Mutex<Option<String>>,
}

impl Bot {
    /// Check everything the bot depends on, then build it: the Slack
    /// tokens (`auth.test`, `apps.connections.open`), Staw (its health
    /// probe and the OAuth device grant), the model's credentials, and
    /// the store.
    pub async fn start(config: Config) -> Result<Arc<Self>, StartError> {
        let slack = Slack::new(
            &config.slack.api_url,
            &config.slack.bot_token,
            &config.slack.app_token,
        );
        let me = slack.auth_test().await.map_err(StartError::BotToken)?;
        let socket = slack.open_socket().await.map_err(StartError::AppToken)?;
        let staw = Staw::new(&config.staw.url);
        let staw_error = |source| StartError::Staw {
            url: config.staw.url.clone(),
            source,
        };
        staw.health().await.map_err(staw_error)?;
        let oauth = staw.oauth().await.map_err(staw_error)?;
        let model = config.model.endpoint.client().map_err(StartError::Model)?;
        let store = Store::open(&config.store).map_err(StartError::Store)?;
        tracing::info!(
            workspace = %me.team,
            bot = %me.user,
            staw = %config.staw.url,
            model = %model.describe(),
            channels = config.channels.len(),
            "staw-slack started"
        );
        Ok(Arc::new(Self {
            people: People::new(store, staw.clone(), oauth),
            config,
            slack,
            staw,
            me,
            model,
            seen: Mutex::new(HashMap::new()),
            names: Mutex::new(HashMap::new()),
            first_socket: Mutex::new(Some(socket)),
        }))
    }

    /// Serve Socket Mode until the process ends.
    pub async fn run(self: Arc<Self>) {
        let first = self.first_socket.lock().expect("socket lock").take();
        crate::socket::run(self, first).await;
    }

    /// People's credentials (the store, the cached access tokens).
    pub fn people(&self) -> &People {
        &self.people
    }

    pub(crate) fn slack(&self) -> &Slack {
        &self.slack
    }

    /// Hand one acknowledged envelope to its handler, in a task of its
    /// own so that the socket keeps reading.
    pub(crate) fn dispatch(self: &Arc<Self>, kind: &str, payload: Value) {
        match kind {
            "events_api" => {
                let callback: Callback = match serde_json::from_value(payload) {
                    Ok(callback) => callback,
                    Err(e) => {
                        tracing::warn!(error = %e, "unreadable event");
                        return;
                    }
                };
                if !self.first_time(&callback.event_id) {
                    tracing::info!(event = %callback.event_id, "dropped a repeated event");
                    return;
                }
                match callback.event["type"].as_str() {
                    Some("app_mention") => {
                        let bot = Arc::clone(self);
                        tokio::spawn(async move { bot.mention(callback).await });
                    }
                    Some("app_home_opened") if callback.event["tab"] == "home" => {
                        let Some(user) = callback.event["user"].as_str() else {
                            return;
                        };
                        let key = Key::new(&callback.team_id, user);
                        let bot = Arc::clone(self);
                        tokio::spawn(async move { bot.publish_home(&key).await });
                    }
                    _ => {}
                }
            }
            "interactive" if payload["type"] == "block_actions" => {
                let team = payload["team"]["id"]
                    .as_str()
                    .or_else(|| payload["user"]["team_id"].as_str());
                let (Some(team), Some(user)) = (team, payload["user"]["id"].as_str()) else {
                    return;
                };
                let pressed = payload["actions"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|a| a["action_id"] == DISCONNECT));
                if pressed {
                    let key = Key::new(team, user);
                    let bot = Arc::clone(self);
                    tokio::spawn(async move { bot.disconnect(key).await });
                }
            }
            _ => {}
        }
    }

    /// Whether this event id is new; remembers it.
    fn first_time(&self, event_id: &str) -> bool {
        let mut seen = self.seen.lock().expect("seen lock");
        let now = Instant::now();
        if seen.len() > 1_000 {
            seen.retain(|_, at| now.duration_since(*at) < SEEN_TTL);
        }
        seen.insert(event_id.to_string(), now).is_none()
    }

    /// One request, from the mention to the direct message.
    async fn mention(self: Arc<Self>, callback: Callback) {
        let mention: Mention = match serde_json::from_value(callback.event) {
            Ok(mention) => mention,
            Err(e) => {
                tracing::warn!(error = %e, "unreadable app_mention");
                return;
            }
        };
        // A mention by an app, or by the bot itself, is not a request.
        let Some(user) = mention.user.clone() else {
            return;
        };
        if mention.bot_id.is_some() || user == self.me.user_id {
            return;
        }
        let key = Key::new(&callback.team_id, &user);
        tracing::info!(event = %callback.event_id, channel = %mention.channel, user = %user, "request");
        if callback.is_ext_shared_channel {
            self.tell(&user, texts::SHARED_CHANNEL).await;
            return;
        }
        let Some(project) = self.config.channels.get(&mention.channel).cloned() else {
            self.tell(&user, texts::UNMAPPED_CHANNEL).await;
            return;
        };
        if let Err(e) = self
            .slack
            .react(&mention.channel, &mention.ts, texts::ACK_REACTION)
            .await
        {
            tracing::warn!(error = %e, "could not acknowledge the request in the thread");
        }
        let reply = match self.ask(&key, &project, &mention).await {
            Ok(Some(reply)) => reply,
            // The connect message went out instead.
            Ok(None) => return,
            Err(failure) => failure.text().to_string(),
        };
        self.tell(&user, &reply).await;
    }

    /// The request as the person: their token, their MCP session, the
    /// model. `None` when the person has to connect first (the connect
    /// message is sent here).
    async fn ask(
        self: &Arc<Self>,
        key: &Key,
        project: &str,
        mention: &Mention,
    ) -> Result<Option<String>, Failure> {
        // Fresh enough to outlast the whole request.
        let margin = self.config.model.budget + Duration::from_secs(60);
        let Some(access) = self
            .people
            .access(key, margin, false)
            .await
            .map_err(Failure::staw)?
        else {
            self.connect(key).await;
            return Ok(None);
        };
        let mcp = match self.staw.mcp(&access).await {
            Ok(mcp) => mcp,
            Err(first) => {
                // Staw may have turned the cached token away (a restart
                // with a new signing key, a clock that disagrees): get a
                // fresh one, once.
                tracing::warn!(error = %first, "mcp session refused; refreshing the token once");
                let Some(access) = self
                    .people
                    .access(key, margin, true)
                    .await
                    .map_err(Failure::staw)?
                else {
                    self.connect(key).await;
                    return Ok(None);
                };
                self.staw.mcp(&access).await.map_err(Failure::staw)?
            }
        };
        let result = self.answer(&mcp, project, mention).await;
        mcp.close().await;
        result.map(Some)
    }

    async fn answer(&self, mcp: &Mcp, project: &str, mention: &Mention) -> Result<String, Failure> {
        let map = mcp
            .call("project_list", json!({}))
            .await
            .map_err(|e| Failure::staw(e.to_string()))?;
        let Some(scope) = find(&map, project) else {
            return Ok(texts::NO_PROJECT_ACCESS.to_string());
        };
        let thread = self.read_thread(mention).await.map_err(Failure::slack)?;
        let instructions = mcp.instructions().unwrap_or_default();
        let model = &self.config.model;
        let outcome = tokio::time::timeout(
            model.budget,
            agent::run(
                &self.model,
                mcp,
                &scope,
                &thread,
                &instructions,
                self.staw.base(),
                model.rounds,
            ),
        )
        .await
        .map_err(|_| Failure::TooLong)?
        .map_err(|e| match e {
            agent::Error::Staw(e) => Failure::staw(e),
            e => Failure::model(e),
        })?;
        Ok(match outcome {
            Outcome::Record => texts::RECORD_LATER.to_string(),
            Outcome::Answer(text) => match thread.note() {
                Some(note) => format!("{text}\n\n{note}"),
                None => text,
            },
        })
    }

    /// The thread that the mention belongs to, as the model reads it.
    async fn read_thread(&self, mention: &Mention) -> Result<thread::Thread, crate::slack::Error> {
        let root = mention.thread_ts.as_deref().unwrap_or(&mention.ts);
        let messages = self.slack.replies(&mention.channel, root).await?;
        let mut names = HashMap::new();
        for id in thread::people(&messages) {
            if id == self.me.user_id {
                continue;
            }
            names.insert(id.clone(), self.name(&id).await);
        }
        Ok(thread::build(
            &messages,
            &mention.ts,
            &mention.text,
            &self.me,
            &names,
            self.config.model.thread_chars,
        ))
    }

    /// The name Slack shows for a user; the id when it cannot be read.
    async fn name(&self, user: &str) -> String {
        if let Some((name, at)) = self.names.lock().expect("names lock").get(user)
            && at.elapsed() < NAME_TTL
        {
            return name.clone();
        }
        match self.slack.user(user).await {
            Ok(profile) => {
                let name = profile.shown();
                self.names
                    .lock()
                    .expect("names lock")
                    .insert(user.to_string(), (name.clone(), Instant::now()));
                name
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not read a slack user's name");
                user.to_string()
            }
        }
    }

    /// Start connecting the person, or remind them of the grant that is
    /// already open. The approval is awaited in a task of its own.
    async fn connect(self: &Arc<Self>, key: &Key) {
        let name = self.name(&key.user).await;
        let client = format!("Slack {}: @{}", self.me.team, name);
        match self.people.begin(key, &client).await {
            Ok(Begun::Already(pending)) => {
                self.tell(
                    &key.user,
                    &texts::connect(&pending.link, &pending.code, pending.minutes_left()),
                )
                .await;
            }
            Ok(Begun::Opened {
                client_id,
                client,
                opened,
                pending,
            }) => {
                self.tell(
                    &key.user,
                    &texts::connect(&pending.link, &pending.code, pending.minutes_left()),
                )
                .await;
                let bot = Arc::clone(self);
                let key = key.clone();
                tokio::spawn(async move {
                    let text = match bot.people.wait(&key, &client_id, &client, &opened).await {
                        people::Outcome::Connected(person) => texts::connected(&person.name),
                        people::Outcome::Denied => texts::CONNECT_DENIED.to_string(),
                        people::Outcome::Expired => texts::CONNECT_EXPIRED.to_string(),
                        people::Outcome::Failed => texts::CONNECT_FAILED.to_string(),
                    };
                    bot.tell(&key.user, &text).await;
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not start connecting a person");
                self.tell(&key.user, texts::CONNECT_FAILED).await;
            }
        }
    }

    /// The App Home tab: whom the person is connected as, and the button
    /// that disconnects.
    async fn publish_home(&self, key: &Key) {
        let view = match self.people.store().get(key) {
            Some(person) => json!({
                "type": "home",
                "blocks": [
                    section(&texts::home_connected(&person.name)),
                    {
                        "type": "actions",
                        "elements": [{
                            "type": "button",
                            "action_id": DISCONNECT,
                            "style": "danger",
                            "text": plain(texts::DISCONNECT_BUTTON),
                            "confirm": {
                                "title": plain(texts::DISCONNECT_CONFIRM_TITLE),
                                "text": plain(texts::DISCONNECT_CONFIRM_TEXT),
                                "confirm": plain(texts::DISCONNECT_CONFIRM_YES),
                                "deny": plain(texts::DISCONNECT_CONFIRM_NO),
                                "style": "danger",
                            },
                        }],
                    },
                ],
            }),
            None => json!({
                "type": "home",
                "blocks": [section(texts::HOME_NOT_CONNECTED)],
            }),
        };
        if let Err(e) = self.slack.publish_home(&key.user, view).await {
            tracing::warn!(error = %e, "could not publish the app home");
        }
    }

    /// Disconnect from the App Home: the token is deleted here, and the
    /// person is told how to revoke it in Staw.
    async fn disconnect(self: Arc<Self>, key: Key) {
        let text = match self.people.disconnect(&key).await {
            Ok(Some(person)) => {
                texts::disconnected(&format!("{}/#/settings", self.staw.base()), &person.client)
            }
            Ok(None) => texts::NOT_CONNECTED.to_string(),
            Err(e) => {
                tracing::error!(error = %e, "could not delete a token");
                texts::STAW_UNAVAILABLE.to_string()
            }
        };
        self.tell(&key.user, &text).await;
        self.publish_home(&key).await;
    }

    /// A direct message to one person, split where Slack would rather it
    /// were.
    async fn tell(&self, user: &str, text: &str) {
        for part in split(text, MESSAGE_CHARS) {
            if let Err(e) = self.slack.post_to_person(user, &part).await {
                tracing::error!(error = %e, user = %user, "could not send a direct message");
                return;
            }
        }
    }
}

/// The action id of the App Home's Disconnect button.
const DISCONNECT: &str = "disconnect";

/// Slack keeps message text best under 4,000 characters.
const MESSAGE_CHARS: usize = 3_900;

/// The outer event of an `events_api` envelope.
#[derive(Deserialize)]
struct Callback {
    team_id: String,
    event_id: String,
    #[serde(default)]
    is_ext_shared_channel: bool,
    event: Value,
}

/// The `app_mention` event, the fields the bot reads.
#[derive(Deserialize)]
struct Mention {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    bot_id: Option<String>,
    channel: String,
    ts: String,
    #[serde(default)]
    thread_ts: Option<String>,
    #[serde(default)]
    text: String,
}

/// What went wrong, as the person reads it; the cause is logged where it
/// happened.
enum Failure {
    Staw,
    Slack,
    Model,
    TooLong,
}

impl Failure {
    fn staw(error: impl std::fmt::Display) -> Self {
        tracing::warn!(error = %error, "a request failed in staw");
        Failure::Staw
    }

    fn slack(error: crate::slack::Error) -> Self {
        tracing::warn!(error = %error, "a request failed in slack");
        Failure::Slack
    }

    fn model(error: agent::Error) -> Self {
        tracing::warn!(error = %error, "a request failed in the model");
        Failure::Model
    }

    fn text(&self) -> &'static str {
        match self {
            Failure::Staw => texts::STAW_UNAVAILABLE,
            Failure::Slack => texts::SLACK_UNAVAILABLE,
            Failure::Model => texts::MODEL_UNAVAILABLE,
            Failure::TooLong => texts::TOO_LONG,
        }
    }
}

/// The mapped project in the person's `project_list`, with its group.
fn find(map: &Value, project: &str) -> Option<Scope> {
    map.as_array()?.iter().find_map(|group| {
        group["projects"].as_array()?.iter().find_map(|p| {
            (p["project_id"].as_str()? == project).then(|| Scope {
                project_id: project.to_string(),
                project: p["name"].as_str().unwrap_or(project).to_string(),
                group_id: group["group_id"].as_str().unwrap_or_default().to_string(),
                group: group["group_name"].as_str().unwrap_or_default().to_string(),
            })
        })
    })
}

fn section(text: &str) -> Value {
    json!({ "type": "section", "text": { "type": "mrkdwn", "text": text } })
}

fn plain(text: &str) -> Value {
    json!({ "type": "plain_text", "text": text })
}

/// Split text into parts of at most `limit` characters, at a paragraph,
/// then a line, then a space when one is near.
fn split(text: &str, limit: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > limit {
        let end = rest
            .char_indices()
            .nth(limit)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let window = &rest[..end];
        let cut = ["\n\n", "\n", " "]
            .iter()
            .filter_map(|sep| window.rfind(sep).filter(|&i| i > end / 2))
            .next()
            .unwrap_or(end);
        parts.push(rest[..cut].trim_end().to_string());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() {
        parts.push(rest.to_string());
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_text_splits_at_paragraphs() {
        let text = format!("{}\n\n{}", "a".repeat(30), "b".repeat(30));
        assert_eq!(split(&text, 40), ["a".repeat(30), "b".repeat(30)]);
        assert_eq!(split("short", 40), ["short"]);
        let parts = split(&"c".repeat(100), 40);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.chars().count() <= 40));
    }

    #[test]
    fn the_mapped_project_is_found_with_its_group() {
        let map = json!([
            { "group_id": "G1", "group_name": "platform", "projects": [
                { "project_id": "P1", "name": "web" },
                { "project_id": "P2", "name": "server" },
            ]},
        ]);
        let scope = find(&map, "P2").unwrap();
        assert_eq!(
            (scope.project.as_str(), scope.group_id.as_str()),
            ("server", "G1")
        );
        assert!(find(&map, "P3").is_none());
    }
}
