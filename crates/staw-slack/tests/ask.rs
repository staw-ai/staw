//! Step 1, "Ask Staw from Slack", end to end: the bot against a stub
//! Slack (Web API and Socket Mode), a stub model, and a real Staw on a
//! fresh database (testcontainers — needs Docker).
//!
//! One test per line of the step's "Done when", plus the App Home's
//! Disconnect and the reply to a request to record.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use staw_slack::store::Key;
use staw_slack::texts;
use staw_storage::UserId;

/// The thread every test asks in: a person's line, an app's line, an
/// earlier line of the bot itself, and three requests.
const ROOT: &str = "1700000000.000001";
const ALICE_ASKS: &str = "1700000000.000004";
const ALICE_ASKS_AGAIN: &str = "1700000000.000005";
const BOB_ASKS: &str = "1700000000.000006";

const QUESTION: &str = "<@U0STAWBOT> what did we decide about the queue?";

struct World {
    staw: Staw,
    slack: Slack,
    alice: (UserId, String),
    bob: (UserId, String),
    project: String,
    decision: String,
}

/// Staw with Alice's group and project, a decision she can see and one
/// in Bob's own group that she cannot; Slack with one thread.
async fn world() -> World {
    let staw = Staw::start().await;
    let slack = Slack::start().await;
    let alice = staw.user("alice", "Alice").await;
    let bob = staw.user("bob", "Bob").await;
    let (_, project) = staw.project(&alice.1, "platform", "queue-service").await;
    let decision = staw
        .decision(
            &alice.1,
            &project,
            "Use Postgres for the job queue",
            "Jobs live in a Postgres table, so the queue needs no Redis.",
        )
        .await;
    let (_, elsewhere) = staw.project(&bob.1, "bob's own", "side-project").await;
    staw.decision(
        &bob.1,
        &elsewhere,
        "Use Redis for the side queue",
        "The side project keeps its queue in Redis.",
    )
    .await;
    slack.thread(
        MAPPED,
        vec![
            line(ROOT, BOB, "we need a queue for the import jobs"),
            json!({
                "type": "message", "ts": "1700000000.000002", "bot_id": "B0DEPLOY1",
                "subtype": "bot_message", "bot_profile": { "name": "Deploy" },
                "text": "build 42 passed",
            }),
            json!({
                "type": "message", "ts": "1700000000.000003", "user": BOT_USER,
                "bot_id": BOT_ID, "text": "an earlier answer of the bot",
            }),
            line(ALICE_ASKS, ALICE, QUESTION),
            line(ALICE_ASKS_AGAIN, ALICE, "<@U0STAWBOT> and the queue again?"),
            line(BOB_ASKS, BOB, "<@U0STAWBOT> what about the queue?"),
        ],
    );
    World {
        staw,
        slack,
        alice,
        bob,
        project,
        decision,
    }
}

impl World {
    /// Connect `user` the way a finished device grant would: a connector
    /// token in Staw, the refresh token in the bot's store.
    async fn connected(
        &self,
        slack_user: &'static str,
        token: &str,
        name: &str,
    ) -> (String, (&'static str, staw_slack::store::Person)) {
        let client = format!("Slack {WORKSPACE}: @{}", name.to_lowercase());
        let (id, secret) = self.staw.connector(token, &client).await;
        (id, (slack_user, person(&secret, name, &client)))
    }

    fn ask(&self, event: &str, user: &str, ts: &str) -> String {
        self.slack.event(
            event,
            mention(MAPPED, user, ts, Some(ROOT), QUESTION),
            false,
        )
    }
}

/// The text between `start` and the next `end`.
fn between<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let from = text
        .find(start)
        .unwrap_or_else(|| panic!("{start:?} in {text:?}"))
        + start.len();
    let to = text[from..].find(end).unwrap() + from;
    &text[from..to]
}

/// An unconnected person gets the connect message and no answer; after
/// the device grant completes, the next request is answered.
#[tokio::test]
async fn an_unconnected_person_connects_then_is_answered() {
    let w = world().await;
    let model = Model::start(librarian("queue")).await;
    let running = bot(&w.slack, &w.staw, &model, &w.project).await;

    w.ask("Ev-connect-1", ALICE, ALICE_ASKS);
    let dms = w.slack.wait_dms(ALICE, 1).await;
    let code = between(&dms[0], "approve the code *", "*").to_string();
    let link = between(&dms[0], "open <", "|this link>");
    assert_eq!(link, format!("{}/#/pair/{code}", w.staw.url));
    settle().await;
    assert_eq!(w.slack.dms(ALICE).len(), 1, "no answer before connecting");
    assert!(model.requests().is_empty(), "the model is not called");
    assert!(w.staw.mcp_calls().is_empty(), "nothing is read from staw");

    // The approval page names the Slack account.
    let (status, grant) = w
        .staw
        .rest(&w.alice.1, "GET", &format!("/api/v1/devices/{code}"), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{grant}");
    assert_eq!(grant["client_name"], "Slack Acme: @alice");

    // Alice approves; the bot polls and says whom she connected as.
    let (status, _) = w
        .staw
        .rest(
            &w.alice.1,
            "POST",
            &format!("/api/v1/devices/{code}"),
            Some(json!({ "approve": true })),
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let dms = w.slack.wait_dms(ALICE, 2).await;
    assert_eq!(dms[1], texts::connected("Alice"));

    // The refresh token is stored for this Slack account, and Staw lists
    // the token under the Slack account's name.
    let stored = running
        .bot
        .people()
        .store()
        .get(&Key::new(TEAM, ALICE))
        .unwrap();
    assert_eq!(stored.name, "Alice");
    assert_eq!(stored.client, "Slack Acme: @alice");
    let (_, tokens) = w.staw.rest(&w.alice.1, "GET", "/api/v1/tokens", None).await;
    assert!(
        tokens["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t["label"] == "connector:Slack Acme: @alice"),
        "{tokens}"
    );

    // The next request is answered.
    w.ask("Ev-connect-2", ALICE, ALICE_ASKS_AGAIN);
    let dms = w.slack.wait_dms(ALICE, 3).await;
    assert!(dms[2].contains("Use Postgres for the job queue"), "{dms:?}");
    assert!(
        w.staw.mcp_calls().iter().all(|c| c.user == Some(w.alice.0)),
        "{:?}",
        w.staw.mcp_calls()
    );
}

/// A question gets an answer in a direct message, built from a decision
/// that the person can see, and the thread gets only the reaction. Every
/// MCP call of the request runs as the person who asked.
#[tokio::test]
async fn a_question_is_answered_in_private_and_the_thread_gets_a_reaction() {
    let w = world().await;
    let (_, alice) = w.connected(ALICE, &w.alice.1, "Alice").await;
    let model = Model::start(librarian("queue")).await;
    let _running = bot_with(&w.slack, &w.staw, &model, &w.project, &scratch(), &[alice]).await;

    let envelope = w.ask("Ev-answer", ALICE, ALICE_ASKS);
    let dms = w.slack.wait_dms(ALICE, 1).await;
    let link = format!("{}/#/decision/{}", w.staw.url, w.decision);
    assert!(dms[0].contains("Use Postgres for the job queue"), "{dms:?}");
    assert!(dms[0].contains(&link), "{dms:?}");
    assert!(dms[0].contains("Jobs live in a Postgres table"), "{dms:?}");
    assert!(!dms[0].contains("Redis for the side queue"), "{dms:?}");
    settle().await;

    // The thread: one reaction on the request. Every post went to Alice
    // in private; no channel received any text.
    assert_eq!(
        w.slack.reactions(),
        [(MAPPED.into(), ALICE_ASKS.into(), "eyes".into())]
    );
    let posts = w.slack.posts();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert!(
        posts.iter().all(|(channel, _)| channel == ALICE),
        "{posts:?}"
    );
    assert!(w.slack.acks().contains(&envelope));

    // Staw saw Alice, and only reads, from the client staw-slack.
    let calls = w.staw.mcp_calls();
    assert!(calls.iter().all(|c| c.user == Some(w.alice.0)), "{calls:?}");
    let tools: Vec<&str> = calls.iter().filter_map(|c| c.tool.as_deref()).collect();
    assert_eq!(tools, ["project_list", "decision_search", "decision_get"]);
    assert!(
        calls
            .iter()
            .any(|c| c.method.as_deref() == Some("initialize")
                && c.client.as_deref() == Some("staw-slack")),
        "{calls:?}"
    );

    // The model read Staw's own instructions and the thread: numbered,
    // the app's line marked, the bot's own line left out, every page read.
    let first = &model.requests()[0];
    let system = first["messages"][0]["content"].as_str().unwrap();
    assert!(
        system.contains("Staw stores the decision history"),
        "{system}"
    );
    assert!(system.contains("\"queue-service\""), "{system}");
    let thread = first["messages"][1]["content"].as_str().unwrap();
    assert!(
        thread.contains("[0] bob: we need a queue for the import jobs"),
        "{thread}"
    );
    assert!(
        thread.contains("[1] Deploy (app): build 42 passed"),
        "{thread}"
    );
    assert!(
        thread.contains("[3] alice (request): @staw what did we decide about the queue?"),
        "{thread}"
    );
    assert!(
        thread.contains("[5] bob: @staw what about the queue?"),
        "{thread}"
    );
    assert!(!thread.contains("an earlier answer of the bot"), "{thread}");
    let tools: Vec<&str> = first["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(tools, ["search_decisions", "get_decision", "answer"]);
}

/// A person who does not see the mapped project gets a direct message
/// that says so, and the model is not called.
#[tokio::test]
async fn a_person_without_the_project_is_told_and_the_model_is_not_called() {
    let w = world().await;
    let (_, bob) = w.connected(BOB, &w.bob.1, "Bob").await;
    let model = Model::start(librarian("queue")).await;
    let _running = bot_with(&w.slack, &w.staw, &model, &w.project, &scratch(), &[bob]).await;

    w.ask("Ev-no-access", BOB, BOB_ASKS);
    let dms = w.slack.wait_dms(BOB, 1).await;
    assert_eq!(dms, [texts::NO_PROJECT_ACCESS]);
    settle().await;
    assert!(model.requests().is_empty(), "the model is not called");
    assert_eq!(w.slack.dms(BOB).len(), 1);
    let calls = w.staw.mcp_calls();
    assert!(calls.iter().all(|c| c.user == Some(w.bob.0)), "{calls:?}");
    let tools: Vec<&str> = calls.iter().filter_map(|c| c.tool.as_deref()).collect();
    assert_eq!(tools, ["project_list"]);
}

/// An unmapped channel and a channel shared with another organization
/// get a short direct message and nothing else.
#[tokio::test]
async fn unmapped_and_shared_channels_get_a_short_message_and_nothing_else() {
    let w = world().await;
    let (_, alice) = w.connected(ALICE, &w.alice.1, "Alice").await;
    let model = Model::start(librarian("queue")).await;
    let _running = bot_with(&w.slack, &w.staw, &model, &w.project, &scratch(), &[alice]).await;

    let unmapped = w.slack.event(
        "Ev-unmapped",
        mention(UNMAPPED, ALICE, "1700000001.000001", None, QUESTION),
        false,
    );
    assert_eq!(w.slack.wait_dms(ALICE, 1).await, [texts::UNMAPPED_CHANNEL]);
    let shared = w.slack.event(
        "Ev-shared",
        mention(MAPPED, ALICE, ALICE_ASKS, Some(ROOT), QUESTION),
        true,
    );
    let dms = w.slack.wait_dms(ALICE, 2).await;
    assert_eq!(dms[1], texts::SHARED_CHANNEL);
    settle().await;

    assert_eq!(w.slack.posts().len(), 2);
    assert!(w.slack.reactions().is_empty(), "no reaction");
    let methods: Vec<String> = w.slack.calls().into_iter().map(|c| c.method).collect();
    assert!(
        methods.iter().all(
            |m| ["auth.test", "apps.connections.open", "chat.postMessage"].contains(&m.as_str())
        ),
        "{methods:?}"
    );
    assert!(w.staw.mcp_calls().is_empty(), "nothing is read from staw");
    assert!(model.requests().is_empty(), "the model is not called");
    let acks = w.slack.acks();
    assert!(acks.contains(&unmapped) && acks.contains(&shared));
}

/// The same event twice is handled once.
#[tokio::test]
async fn the_same_event_twice_is_handled_once() {
    let w = world().await;
    let (_, alice) = w.connected(ALICE, &w.alice.1, "Alice").await;
    let model = Model::start(librarian("queue")).await;
    let _running = bot_with(&w.slack, &w.staw, &model, &w.project, &scratch(), &[alice]).await;

    let first = w.ask("Ev-twice", ALICE, ALICE_ASKS);
    let again = w.ask("Ev-twice", ALICE, ALICE_ASKS);
    w.slack.wait_dms(ALICE, 1).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(w.slack.dms(ALICE).len(), 1);
    assert_eq!(w.slack.reactions().len(), 1);
    assert_eq!(model.requests().len(), 3, "one loop: search, read, answer");
    let acks = w.slack.acks();
    assert!(
        acks.contains(&first) && acks.contains(&again),
        "both deliveries are acknowledged"
    );
}

/// An expired access token is refreshed and the request succeeds —
/// whether the bot sees the expiry coming or Staw turns the token away.
#[tokio::test]
async fn an_expired_access_token_is_refreshed() {
    let w = world().await;
    let (_, alice) = w.connected(ALICE, &w.alice.1, "Alice").await;
    let model = Model::start(librarian("queue")).await;
    let running = bot_with(&w.slack, &w.staw, &model, &w.project, &scratch(), &[alice]).await;
    let key = Key::new(TEAM, ALICE);
    let refreshes = |w: &World| {
        w.staw
            .calls()
            .iter()
            .filter(|c| c.grant.as_deref() == Some("refresh_token"))
            .count()
    };

    // Staw turns the token away although the bot thinks it is good.
    running.bot.people().set_access(
        &key,
        &w.staw.expired_access(w.alice.0),
        Duration::from_secs(3600),
    );
    w.ask("Ev-expired-1", ALICE, ALICE_ASKS);
    let dms = w.slack.wait_dms(ALICE, 1).await;
    assert!(dms[0].contains("Use Postgres for the job queue"), "{dms:?}");
    assert_eq!(refreshes(&w), 1);
    let calls = w.staw.mcp_calls();
    assert_eq!(
        calls.iter().filter(|c| c.user.is_none()).count(),
        1,
        "one turned-away handshake: {calls:?}"
    );
    assert!(
        calls
            .iter()
            .filter(|c| c.user.is_some())
            .all(|c| c.user == Some(w.alice.0)),
        "{calls:?}"
    );

    // The token runs out: the bot refreshes before it uses it.
    running
        .bot
        .people()
        .set_access(&key, "cvg_spent", Duration::ZERO);
    let before = w.staw.mcp_calls().len();
    w.ask("Ev-expired-2", ALICE, ALICE_ASKS_AGAIN);
    let dms = w.slack.wait_dms(ALICE, 2).await;
    assert!(dms[1].contains("Use Postgres for the job queue"), "{dms:?}");
    assert_eq!(refreshes(&w), 2);
    let calls = w.staw.mcp_calls();
    assert!(
        calls[before..].iter().all(|c| c.user == Some(w.alice.0)),
        "{calls:?}"
    );
}

/// A revoked refresh token leads to the connect message.
#[tokio::test]
async fn a_revoked_refresh_token_leads_to_the_connect_message() {
    let w = world().await;
    let (token_id, alice) = w.connected(ALICE, &w.alice.1, "Alice").await;
    let model = Model::start(librarian("queue")).await;
    let running = bot_with(&w.slack, &w.staw, &model, &w.project, &scratch(), &[alice]).await;
    let (status, _) = w
        .staw
        .rest(
            &w.alice.1,
            "DELETE",
            &format!("/api/v1/tokens/{token_id}"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    w.ask("Ev-revoked", ALICE, ALICE_ASKS);
    let dms = w.slack.wait_dms(ALICE, 1).await;
    assert!(dms[0].contains("approve the code"), "{dms:?}");
    settle().await;
    assert_eq!(w.slack.dms(ALICE).len(), 1, "no answer");
    assert!(model.requests().is_empty());
    assert!(w.staw.mcp_calls().is_empty());
    assert!(
        running
            .bot
            .people()
            .store()
            .get(&Key::new(TEAM, ALICE))
            .is_none(),
        "the revoked token is forgotten"
    );
}

/// The App Home shows whom a person is connected as; Disconnect deletes
/// the token and says how to revoke it in Staw.
#[tokio::test]
async fn the_app_home_disconnects() {
    let w = world().await;
    let (_, alice) = w.connected(ALICE, &w.alice.1, "Alice").await;
    let model = Model::start(librarian("queue")).await;
    let running = bot_with(&w.slack, &w.staw, &model, &w.project, &scratch(), &[alice]).await;

    w.slack.event(
        "Ev-home",
        json!({ "type": "app_home_opened", "user": ALICE, "channel": "D0ALICE01", "tab": "home", "event_ts": "1" }),
        false,
    );
    wait(|| w.slack.views().len() == 1, "the app home").await;
    let (user, view) = &w.slack.views()[0];
    assert_eq!(user, ALICE);
    let home = view.to_string();
    assert!(home.contains("connected to Staw as *Alice*"), "{home}");
    assert!(home.contains("\"action_id\":\"disconnect\""), "{home}");

    w.slack.interactive(json!({
        "type": "block_actions",
        "user": { "id": ALICE, "team_id": TEAM },
        "team": { "id": TEAM, "domain": "acme" },
        "actions": [{ "action_id": "disconnect", "block_id": "b", "type": "button", "action_ts": "1" }],
        "container": { "type": "view", "view_id": "V1" },
    }));
    let dms = w.slack.wait_dms(ALICE, 1).await;
    assert_eq!(
        dms[0],
        texts::disconnected(&format!("{}/#/settings", w.staw.url), "Slack Acme: @alice")
    );
    assert!(
        running
            .bot
            .people()
            .store()
            .get(&Key::new(TEAM, ALICE))
            .is_none()
    );
    wait(|| w.slack.views().len() == 2, "the app home again").await;
    assert!(
        w.slack.views()[1]
            .1
            .to_string()
            .contains("not connected to Staw"),
        "{:?}",
        w.slack.views()
    );
}

/// A request to record gets the fixed reply: recording is the next
/// version.
#[tokio::test]
async fn a_request_to_record_gets_the_fixed_reply() {
    let w = world().await;
    let (_, alice) = w.connected(ALICE, &w.alice.1, "Alice").await;
    let model = Model::start(std::sync::Arc::new(|_| {
        tool_call("answer", json!({ "text": "Recorded it!", "record": true }))
    }))
    .await;
    let _running = bot_with(&w.slack, &w.staw, &model, &w.project, &scratch(), &[alice]).await;

    w.slack.event(
        "Ev-record",
        mention(
            MAPPED,
            ALICE,
            ALICE_ASKS,
            Some(ROOT),
            "<@U0STAWBOT> record: we use Postgres for the queue",
        ),
        false,
    );
    assert_eq!(w.slack.wait_dms(ALICE, 1).await, [texts::RECORD_LATER]);
    let tools: Vec<String> = w
        .staw
        .mcp_calls()
        .into_iter()
        .filter_map(|c| c.tool)
        .collect();
    assert_eq!(tools, ["project_list"], "nothing is written to staw");
}
