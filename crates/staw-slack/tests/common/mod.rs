//! The bot's world under test: a real Staw on a fresh Postgres
//! (testcontainers — needs Docker), a stub Slack (the Web API and a
//! Socket Mode WebSocket), and a stub model (an OpenAI-compatible
//! endpoint that follows a script).
//!
//! Staw runs its own `app()`, served over HTTP, behind one extra layer
//! that writes down every `/mcp` and `/oauth` request with the user its
//! bearer resolves to — resolved exactly as Staw's auth does — so a test
//! can say who each MCP call ran as.

#![allow(dead_code)] // each suite uses its own part of the harness

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::post;
use futures::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use staw_server::auth::{self, Sessions};
use staw_server::{Expert, app};
use staw_slack::Bot;
use staw_slack::store::{Key, Person};
use staw_storage::{AgentKind, Agents, Identity, NewAgent, Tokens, UserId, Users};
use staw_storage_postgres::PgStorage;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Staw's session key in the harness: access tokens are signed with it.
pub const SECRET: &str = "test-session-secret";

/// The Slack workspace.
pub const TEAM: &str = "T0ACME001";
pub const WORKSPACE: &str = "Acme";
/// The bot user, and the bot.
pub const BOT_USER: &str = "U0STAWBOT";
pub const BOT_ID: &str = "B0STAWBOT";
/// A channel mapped to the queue project, and one that is not mapped.
pub const MAPPED: &str = "C0QUEUE01";
pub const UNMAPPED: &str = "C0RANDOM1";
/// Slack accounts.
pub const ALICE: &str = "U0ALICE01";
pub const BOB: &str = "U0BOB0001";

const BOT_TOKEN: &str = "xoxb-test";
const APP_TOKEN: &str = "xapp-test";

/// How long a test waits for the bot to do something.
pub const PATIENCE: Duration = Duration::from_secs(30);

// ---- Staw ------------------------------------------------------------------

/// One request that reached Staw's `/mcp` or `/oauth`.
#[derive(Debug, Clone, Default)]
pub struct StawCall {
    pub path: String,
    /// Whom the bearer belongs to, as Staw's auth resolves it; `None`
    /// without a bearer, or with one that Staw turns away.
    pub user: Option<UserId>,
    /// The JSON-RPC method on `/mcp`.
    pub method: Option<String>,
    /// The tool, for `tools/call`.
    pub tool: Option<String>,
    /// The MCP client's name, for `initialize`.
    pub client: Option<String>,
    /// The `grant_type`, for `/oauth/token`.
    pub grant: Option<String>,
}

pub struct Staw {
    pub url: String,
    pub store: PgStorage,
    pub calls: Arc<Mutex<Vec<StawCall>>>,
    http: reqwest::Client,
    _node: ContainerAsync<Postgres>,
}

#[derive(Clone)]
struct Recorder {
    store: PgStorage,
    sessions: Sessions,
    calls: Arc<Mutex<Vec<StawCall>>>,
}

impl Staw {
    /// A fresh Postgres, migrated, and Staw's `app()` on a local port.
    pub async fn start() -> Self {
        let node = Postgres::default()
            .with_tag("16-alpine")
            .start()
            .await
            .expect("start postgres (is Docker running?)");
        let port = node.get_host_port_ipv4(5432).await.unwrap();
        let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
        let store = PgStorage::connect(&url).await.unwrap();
        store.migrate().await.unwrap();
        let agent = store
            .agent_ensure(NewAgent {
                kind: AgentKind::Model,
                name: "expert".into(),
            })
            .await
            .unwrap();
        // No expert jobs: Staw Expert stays quiet in this world.
        let expert = Expert::new(store.clone(), staw_expert::Registry::default(), agent);
        let calls: Arc<Mutex<Vec<StawCall>>> = Arc::default();
        let recorder = Recorder {
            store: store.clone(),
            sessions: Sessions::new(Some(SECRET)),
            calls: Arc::clone(&calls),
        };
        let router = app(
            store.clone(),
            Sessions::new(Some(SECRET)),
            None,
            None,
            None,
            expert,
            &Default::default(),
        )
        .layer(middleware::from_fn_with_state(recorder, record));
        let addr = serve(router).await;
        Self {
            url: format!("http://{addr}"),
            store,
            calls,
            http: reqwest::Client::new(),
            _node: node,
        }
    }

    /// A Staw user with a bearer token of their own.
    pub async fn user(&self, handle: &str, name: &str) -> (UserId, String) {
        let id = self
            .store
            .user_login(Identity {
                provider: "local".into(),
                subject: handle.into(),
                handle: handle.into(),
                name: name.into(),
            })
            .await
            .unwrap();
        let token = format!("cvg_test_{handle}");
        self.store
            .token_add(id, "test".into(), auth::hash(&token))
            .await
            .unwrap();
        (id, token)
    }

    /// One REST call with a bearer.
    pub async fn rest(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let request = self
            .http
            .request(method.parse().unwrap(), format!("{}{path}", self.url))
            .bearer_auth(token);
        let request = match body {
            Some(body) => request.json(&body),
            None => request,
        };
        let response = request.send().await.unwrap();
        let status = StatusCode::from_u16(response.status().as_u16()).unwrap();
        let text = response.text().await.unwrap();
        let value = serde_json::from_str(&text).unwrap_or(Value::Null);
        (status, value)
    }

    /// A shared group with one project, owned by `token`'s user →
    /// (group id, project id).
    pub async fn project(&self, token: &str, group: &str, project: &str) -> (String, String) {
        let (status, g) = self
            .rest(
                token,
                "POST",
                "/api/v1/groups",
                Some(json!({ "name": group, "kind": "shared" })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{g}");
        let group = g["id"].as_str().unwrap().to_string();
        let (status, p) = self
            .rest(
                token,
                "POST",
                "/api/v1/projects",
                Some(json!({ "group_id": group, "name": project })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{p}");
        (group, p["id"].as_str().unwrap().to_string())
    }

    /// An accepted decision → its id.
    pub async fn decision(&self, token: &str, project: &str, title: &str, summary: &str) -> String {
        let (status, d) = self
            .rest(
                token,
                "POST",
                "/api/v1/decisions",
                Some(json!({
                    "project_id": project, "status": "accepted",
                    "title": title, "summary": summary,
                    "context": null, "consequences": null,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{d}");
        d["id"].as_str().unwrap().to_string()
    }

    /// A connector-style token for `token`'s user, as the device grant
    /// would mint it → (token id, secret).
    pub async fn connector(&self, token: &str, label: &str) -> (String, String) {
        let (status, minted) = self
            .rest(
                token,
                "POST",
                "/api/v1/tokens",
                Some(json!({ "label": format!("connector:{label}") })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{minted}");
        (
            minted["id"].as_str().unwrap().to_string(),
            minted["token"].as_str().unwrap().to_string(),
        )
    }

    /// An access token Staw would issue, but expired an hour ago (well
    /// past the JWT library's leeway of a minute).
    pub fn expired_access(&self, user: UserId) -> String {
        Sessions::new(Some(SECRET)).sign(&json!({
            "sub": user.to_string(),
            "exp": time_now() - 3600,
            "typ": "s",
        }))
    }

    pub fn calls(&self) -> Vec<StawCall> {
        self.calls.lock().unwrap().clone()
    }

    /// The `/mcp` calls so far.
    pub fn mcp_calls(&self) -> Vec<StawCall> {
        self.calls()
            .into_iter()
            .filter(|c| c.path == "/mcp")
            .collect()
    }
}

fn time_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// The recording layer: who called which tool, as Staw's auth sees it.
async fn record(State(recorder): State<Recorder>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    if path != "/mcp" && !path.starts_with("/oauth/") {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20)
        .await
        .unwrap_or_default();
    let bearer = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    let user = match bearer {
        Some(secret) if secret.starts_with("cvg_") => recorder
            .store
            .token_user(&auth::hash(&secret))
            .await
            .ok()
            .flatten(),
        Some(jwt) => recorder.sessions.verify(&jwt),
        None => None,
    };
    let mut call = StawCall {
        path: path.clone(),
        user,
        ..StawCall::default()
    };
    if path == "/mcp" {
        if let Ok(rpc) = serde_json::from_slice::<Value>(&bytes) {
            call.method = rpc["method"].as_str().map(str::to_string);
            call.tool = rpc["params"]["name"].as_str().map(str::to_string);
            call.client = rpc["params"]["clientInfo"]["name"]
                .as_str()
                .map(str::to_string);
        }
    } else if path == "/oauth/token" {
        call.grant = url::form_urlencoded::parse(&bytes)
            .find(|(k, _)| k == "grant_type")
            .map(|(_, v)| v.into_owned());
    }
    recorder.calls.lock().unwrap().push(call);
    next.run(Request::from_parts(parts, Body::from(bytes)))
        .await
}

async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

// ---- Slack -----------------------------------------------------------------

/// One Web API call the bot made.
#[derive(Debug, Clone)]
pub struct SlackCall {
    pub method: String,
    pub token: String,
    pub args: Value,
}

#[derive(Default)]
struct SlackState {
    calls: Vec<SlackCall>,
    acks: Vec<String>,
    users: HashMap<String, Value>,
    /// "channel:root ts" → the thread's messages, oldest first.
    threads: HashMap<String, Vec<Value>>,
}

pub struct Slack {
    pub api: String,
    state: Arc<Mutex<SlackState>>,
    outbox: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
    envelopes: AtomicUsize,
}

impl Slack {
    /// The Web API on one port, Socket Mode on another.
    pub async fn start() -> Self {
        let state: Arc<Mutex<SlackState>> = Arc::default();
        let outbox: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>> = Arc::default();

        let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let socket_url = format!("ws://{}/link/?ticket=test", socket.local_addr().unwrap());
        {
            let state = Arc::clone(&state);
            let outbox = Arc::clone(&outbox);
            tokio::spawn(async move {
                while let Ok((stream, _)) = socket.accept().await {
                    let state = Arc::clone(&state);
                    let outbox = Arc::clone(&outbox);
                    tokio::spawn(async move { connection(stream, state, outbox).await });
                }
            });
        }

        let router = Router::new()
            .route("/api/{method}", post(web_api).get(web_api))
            .with_state((Arc::clone(&state), socket_url));
        let addr = serve(router).await;
        let slack = Self {
            api: format!("http://{addr}/api"),
            state,
            outbox,
            envelopes: AtomicUsize::new(0),
        };
        slack.user(ALICE, "alice", "Alice Archer");
        slack.user(BOB, "bob", "Bob Baker");
        slack
    }

    /// A person in the workspace.
    pub fn user(&self, id: &str, display: &str, real: &str) {
        self.state.lock().unwrap().users.insert(
            id.to_string(),
            json!({
                "id": id, "team_id": TEAM, "name": display.to_lowercase(),
                "real_name": real, "deleted": false, "is_bot": false,
                "profile": { "display_name": display, "real_name": real },
            }),
        );
    }

    /// A thread: its messages, the parent first.
    pub fn thread(&self, channel: &str, messages: Vec<Value>) {
        let root = messages[0]["ts"].as_str().unwrap().to_string();
        self.state
            .lock()
            .unwrap()
            .threads
            .insert(format!("{channel}:{root}"), messages);
    }

    /// Wait for the bot's Socket Mode connection.
    pub async fn connected(&self) {
        wait(
            || self.outbox.lock().unwrap().is_some(),
            "a socket mode connection",
        )
        .await;
    }

    /// Deliver an `events_api` envelope → its envelope id.
    pub fn event(&self, event_id: &str, event: Value, shared: bool) -> String {
        self.deliver(
            "events_api",
            json!({
                "token": "unused", "team_id": TEAM, "api_app_id": "A0STAWAPP",
                "type": "event_callback", "event_id": event_id, "event_time": 0,
                "is_ext_shared_channel": shared,
                "authorizations": [{ "team_id": TEAM, "user_id": BOT_USER, "is_bot": true }],
                "event": event,
            }),
        )
    }

    /// Deliver an `interactive` envelope → its envelope id.
    pub fn interactive(&self, payload: Value) -> String {
        self.deliver("interactive", payload)
    }

    fn deliver(&self, kind: &str, payload: Value) -> String {
        let n = self.envelopes.fetch_add(1, Ordering::SeqCst);
        let id = format!("envelope-{n}");
        let envelope = json!({
            "envelope_id": id, "type": kind, "payload": payload,
            "accepts_response_payload": false, "retry_attempt": 0, "retry_reason": "",
        });
        self.outbox
            .lock()
            .unwrap()
            .as_ref()
            .expect("the bot is connected")
            .send(envelope.to_string())
            .unwrap();
        id
    }

    pub fn calls(&self) -> Vec<SlackCall> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn acks(&self) -> Vec<String> {
        self.state.lock().unwrap().acks.clone()
    }

    /// `chat.postMessage` calls → (channel, text).
    pub fn posts(&self) -> Vec<(String, String)> {
        self.calls()
            .into_iter()
            .filter(|c| c.method == "chat.postMessage")
            .map(|c| {
                (
                    c.args["channel"].as_str().unwrap_or_default().to_string(),
                    c.args["text"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect()
    }

    /// Direct messages to one person, in order.
    pub fn dms(&self, user: &str) -> Vec<String> {
        self.posts()
            .into_iter()
            .filter(|(channel, _)| channel == user)
            .map(|(_, text)| text)
            .collect()
    }

    /// `reactions.add` calls → (channel, ts, name).
    pub fn reactions(&self) -> Vec<(String, String, String)> {
        self.calls()
            .into_iter()
            .filter(|c| c.method == "reactions.add")
            .map(|c| {
                let s = |k: &str| c.args[k].as_str().unwrap_or_default().to_string();
                (s("channel"), s("timestamp"), s("name"))
            })
            .collect()
    }

    /// `views.publish` calls → (user, view).
    pub fn views(&self) -> Vec<(String, Value)> {
        self.calls()
            .into_iter()
            .filter(|c| c.method == "views.publish")
            .map(|c| {
                (
                    c.args["user_id"].as_str().unwrap_or_default().to_string(),
                    c.args["view"].clone(),
                )
            })
            .collect()
    }

    /// Wait until `user` has received `count` direct messages → them.
    pub async fn wait_dms(&self, user: &str, count: usize) -> Vec<String> {
        wait(
            || self.dms(user).len() >= count,
            &format!("{count} direct message(s) to {user}"),
        )
        .await;
        self.dms(user)
    }
}

/// The Web API: records every call and answers like Slack does.
async fn web_api(
    State((state, socket_url)): State<(Arc<Mutex<SlackState>>, String)>,
    Path(method): Path<String>,
    request: Request,
) -> axum::Json<Value> {
    let (parts, body) = request.into_parts();
    let token = parts
        .headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_string();
    let json_body = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    let bytes = axum::body::to_bytes(body, 1 << 20)
        .await
        .unwrap_or_default();
    let mut args = if json_body {
        serde_json::from_slice(&bytes).unwrap_or(json!({}))
    } else {
        Value::Object(
            url::form_urlencoded::parse(&bytes)
                .map(|(k, v)| (k.into_owned(), Value::String(v.into_owned())))
                .collect(),
        )
    };
    if let Some(query) = parts.uri.query() {
        for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
            args[k.as_ref()] = Value::String(v.into_owned());
        }
    }
    let mut state = state.lock().unwrap();
    state.calls.push(SlackCall {
        method: method.clone(),
        token: token.clone(),
        args: args.clone(),
    });
    let refuse = |error: &str| axum::Json(json!({ "ok": false, "error": error }));
    let expected = match method.as_str() {
        "apps.connections.open" => APP_TOKEN,
        _ => BOT_TOKEN,
    };
    if token != expected {
        return refuse("invalid_auth");
    }
    axum::Json(match method.as_str() {
        "auth.test" => json!({
            "ok": true, "url": "https://acme.slack.com/", "team": WORKSPACE, "user": "staw",
            "team_id": TEAM, "user_id": BOT_USER, "bot_id": BOT_ID,
        }),
        "apps.connections.open" => json!({ "ok": true, "url": socket_url }),
        "conversations.replies" => {
            let channel = args["channel"].as_str().unwrap_or_default();
            let ts = args["ts"].as_str().unwrap_or_default();
            let Some(thread) = state.threads.iter().find_map(|(key, messages)| {
                (key.starts_with(&format!("{channel}:")) && messages.iter().any(|m| m["ts"] == ts))
                    .then_some(messages.clone())
            }) else {
                return refuse("thread_not_found");
            };
            // Pages of two, so that every read crosses pages.
            let offset: usize = args["cursor"]
                .as_str()
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            let page: Vec<Value> = thread.iter().skip(offset).take(2).cloned().collect();
            let next = offset + page.len();
            let more = next < thread.len();
            json!({
                "ok": true, "messages": page, "has_more": more,
                "response_metadata": { "next_cursor": if more { next.to_string() } else { String::new() } },
            })
        }
        "chat.postMessage" => {
            json!({ "ok": true, "channel": args["channel"], "ts": "1700000000.000100" })
        }
        "reactions.add" => json!({ "ok": true }),
        "users.info" => {
            let id = args["user"].as_str().unwrap_or_default();
            match state.users.get(id) {
                Some(user) => json!({ "ok": true, "user": user }),
                None => return refuse("user_not_found"),
            }
        }
        "views.publish" => json!({ "ok": true, "view": args["view"] }),
        _ => return refuse("unknown_method"),
    })
}

/// One Socket Mode connection: `hello`, then envelopes from the outbox,
/// and acknowledgements written down.
async fn connection(
    stream: tokio::net::TcpStream,
    state: Arc<Mutex<SlackState>>,
    outbox: Arc<Mutex<Option<mpsc::UnboundedSender<String>>>>,
) {
    let Ok(socket) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut write, mut read) = socket.split();
    let hello = json!({
        "type": "hello", "num_connections": 1,
        "debug_info": { "host": "stub", "approximate_connection_time": 3600 },
        "connection_info": { "app_id": "A0STAWAPP" },
    });
    if write.send(Message::text(hello.to_string())).await.is_err() {
        return;
    }
    let (sender, mut receiver) = mpsc::unbounded_channel::<String>();
    *outbox.lock().unwrap() = Some(sender);
    loop {
        tokio::select! {
            out = receiver.recv() => match out {
                Some(text) => if write.send(Message::text(text)).await.is_err() { return },
                None => return,
            },
            incoming = read.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(ack) = serde_json::from_str::<Value>(text.as_str())
                        && let Some(id) = ack["envelope_id"].as_str()
                    {
                        state.lock().unwrap().acks.push(id.to_string());
                    }
                }
                Some(Ok(_)) => {}
                _ => return,
            },
        }
    }
}

// ---- the model ---------------------------------------------------------------

/// What the stub model answers to one request body: an assistant
/// message.
pub type Script = Arc<dyn Fn(&Value) -> Value + Send + Sync>;

pub struct Model {
    pub base: String,
    requests: Arc<Mutex<Vec<Value>>>,
}

impl Model {
    pub async fn start(script: Script) -> Self {
        let requests: Arc<Mutex<Vec<Value>>> = Arc::default();
        let router = Router::new()
            .route(
                "/v1/chat/completions",
                post(
                    |State((requests, script)): State<(Arc<Mutex<Vec<Value>>>, Script)>,
                     body: String| async move {
                        let request: Value = serde_json::from_str(&body).unwrap();
                        let message = script(&request);
                        requests.lock().unwrap().push(request);
                        let finish = match message.get("tool_calls") {
                            Some(_) => "tool_calls",
                            None => "stop",
                        };
                        axum::Json(json!({
                            "id": "cmpl-1", "object": "chat.completion", "created": 0,
                            "model": "stub",
                            "choices": [{ "index": 0, "message": message, "finish_reason": finish }],
                            "usage": { "prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2 },
                        }))
                    },
                ),
            )
            .with_state((Arc::clone(&requests), script));
        let addr = serve(router).await;
        Self {
            base: format!("http://{addr}/v1/"),
            requests,
        }
    }

    /// The request bodies the model received.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().unwrap().clone()
    }
}

/// An assistant message that calls one tool.
pub fn tool_call(name: &str, arguments: Value) -> Value {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    let n = CALLS.fetch_add(1, Ordering::SeqCst);
    json!({
        "role": "assistant", "content": null,
        "tool_calls": [{
            "id": format!("call-{n}"), "type": "function",
            "function": { "name": name, "arguments": arguments.to_string() },
        }],
    })
}

/// The tool results the bot has sent back so far, parsed.
pub fn tool_results(request: &Value) -> Vec<Value> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["role"] == "tool")
        .map(|m| serde_json::from_str(m["content"].as_str().unwrap()).unwrap())
        .collect()
}

/// A model that searches, reads the best match, and answers with its
/// title and summary — so the answer can only come from a decision that
/// Staw showed to the asking person.
pub fn librarian(query: &'static str) -> Script {
    Arc::new(move |request| {
        let results = tool_results(request);
        match results.len() {
            0 => tool_call("search_decisions", json!({ "query": query })),
            1 => tool_call("get_decision", json!({ "id": results[0][0]["id"] })),
            _ => {
                let decision = results.last().unwrap();
                tool_call(
                    "answer",
                    json!({ "text": format!(
                        "The team decided: {} ({}). {}",
                        decision["title"].as_str().unwrap_or("nothing found"),
                        decision["link"].as_str().unwrap_or("no link"),
                        decision["summary"].as_str().unwrap_or_default(),
                    )}),
                )
            }
        }
    })
}

// ---- the bot -----------------------------------------------------------------

/// The bot, configured against the stubs and Staw, running.
pub struct Running {
    pub bot: Arc<Bot>,
    pub store: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(self.store.parent().unwrap());
    }
}

/// A fresh, private directory for the bot's store.
pub fn scratch() -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir()
        .join(format!(
            "staw-slack-test-{}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
        .join("people.json")
}

/// Start the bot with `project` mapped to [`MAPPED`], and wait for its
/// socket.
pub async fn bot(slack: &Slack, staw: &Staw, model: &Model, project: &str) -> Running {
    bot_with(slack, staw, model, project, &scratch(), &[]).await
}

/// [`bot`], with a store at `store` holding `people` before it starts.
pub async fn bot_with(
    slack: &Slack,
    staw: &Staw,
    model: &Model,
    project: &str,
    store: &std::path::Path,
    people: &[(&str, Person)],
) -> Running {
    {
        let seeded = staw_slack::store::Store::open(store).unwrap();
        for (user, person) in people {
            seeded.put(&Key::new(TEAM, *user), person.clone()).unwrap();
        }
    }
    let config = staw_slack::config::parse(&format!(
        r#"
        [slack]
        bot_token = "{BOT_TOKEN}"
        app_token = "{APP_TOKEN}"
        api_url = "{api}"

        [staw]
        url = "{staw}"

        [model]
        provider = "openai"
        model = "stub"
        base_url = "{model}"
        budget_secs = 60

        [[channels]]
        channel = "{MAPPED}"
        project = "{project}"

        [store]
        path = "{store}"
        "#,
        api = slack.api,
        staw = staw.url,
        model = model.base,
        store = store.display(),
    ))
    .unwrap();
    let bot = Bot::start(config).await.unwrap();
    let task = tokio::spawn(Arc::clone(&bot).run());
    slack.connected().await;
    Running {
        bot,
        store: store.to_path_buf(),
        task,
    }
}

/// A connected person for the store.
pub fn person(refresh_token: &str, name: &str, client: &str) -> Person {
    Person {
        refresh_token: refresh_token.to_string(),
        name: name.to_string(),
        client: client.to_string(),
    }
}

/// An `app_mention` event.
pub fn mention(channel: &str, user: &str, ts: &str, thread_ts: Option<&str>, text: &str) -> Value {
    let mut event = json!({
        "type": "app_mention", "user": user, "text": text, "ts": ts,
        "channel": channel, "event_ts": ts, "team": TEAM,
    });
    if let Some(thread_ts) = thread_ts {
        event["thread_ts"] = json!(thread_ts);
    }
    event
}

/// A thread message.
pub fn line(ts: &str, user: &str, text: &str) -> Value {
    json!({ "type": "message", "ts": ts, "user": user, "text": text, "thread_ts": "1700000000.000001" })
}

/// Poll `ready` until it holds, or fail the test after [`PATIENCE`].
pub async fn wait(ready: impl Fn() -> bool, what: &str) {
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while !ready() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Let the bot finish whatever it would still do, before asserting that
/// it did nothing more.
pub async fn settle() {
    tokio::time::sleep(Duration::from_millis(700)).await;
}
