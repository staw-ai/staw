//! Rule 7, and the checks at start: with no configuration the binary
//! says what is missing and exits; a configuration that is present but
//! unusable is an error that says why; at start the bot checks the
//! Slack tokens and that Staw answers. No Docker needed.

mod common;

use std::process::Command;

use common::{MAPPED, Slack, scratch};
use staw_slack::{Bot, StartError};

/// Run the binary in an empty directory with no `STAW_SLACK_*` variable.
fn staw_slack(dir: &std::path::Path, args: &[&str]) -> (Option<i32>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_staw-slack"));
    command.current_dir(dir).args(args);
    for (key, _) in std::env::vars() {
        if key.starts_with("STAW_SLACK") {
            command.env_remove(key);
        }
    }
    let output = command.output().unwrap();
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn with_no_configuration_the_binary_says_what_is_missing_and_exits() {
    if std::path::Path::new(staw_slack::config::SYSTEM_FILE).exists() {
        eprintln!(
            "skipped: this machine has {}",
            staw_slack::config::SYSTEM_FILE
        );
        return;
    }
    let dir = scratch();
    let dir = dir.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();

    let (code, stderr) = staw_slack(dir, &[]);
    assert_eq!(code, Some(0), "off is not a failure: {stderr}");
    assert!(stderr.contains("staw-slack is off"), "{stderr}");
    for key in [
        "[slack] bot_token",
        "[slack] app_token",
        "[staw] url",
        "[model]",
        "[[channels]]",
        "[store] path",
    ] {
        assert!(stderr.contains(key), "{key} in {stderr}");
    }

    // Present but incomplete: an error that names what is missing.
    std::fs::write(
        dir.join("staw-slack.toml"),
        "[slack]\nbot_token = \"xoxb-1\"\n",
    )
    .unwrap();
    let (code, stderr) = staw_slack(dir, &[]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("incomplete"), "{stderr}");
    assert!(
        stderr.contains("[slack] app_token") && !stderr.contains("[slack] bot_token"),
        "{stderr}"
    );

    // A named file that does not exist fails loudly.
    let (code, stderr) = staw_slack(dir, &["--config", "nowhere.toml"]);
    assert_eq!(code, Some(1), "{stderr}");
    assert!(stderr.contains("nowhere.toml"), "{stderr}");
    let _ = std::fs::remove_dir_all(dir);
}

fn config(
    slack: &Slack,
    bot_token: &str,
    app_token: &str,
    staw: &str,
) -> staw_slack::config::Config {
    staw_slack::config::parse(&format!(
        r#"
        [slack]
        bot_token = "{bot_token}"
        app_token = "{app_token}"
        api_url = "{api}"
        [staw]
        url = "{staw}"
        [model]
        provider = "openai"
        model = "stub"
        base_url = "http://127.0.0.1:9/v1/"
        [[channels]]
        channel = "{MAPPED}"
        project = "01M2P205AK2GMNWSS0BQGM6EN9"
        [store]
        path = "{store}"
        "#,
        api = slack.api,
        store = scratch().display(),
    ))
    .unwrap()
}

#[tokio::test]
async fn the_start_checks_the_slack_tokens_and_staw() {
    let slack = Slack::start().await;
    // A port nothing listens on.
    let nowhere = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}", listener.local_addr().unwrap())
    };

    let refused = Bot::start(config(&slack, "xoxb-wrong", "xapp-test", &nowhere)).await;
    assert!(
        matches!(refused, Err(StartError::BotToken(_))),
        "{:?}",
        refused.err()
    );
    let message = Bot::start(config(&slack, "xoxb-wrong", "xapp-test", &nowhere))
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(
        message.contains("[slack] bot_token") && message.contains("invalid_auth"),
        "{message}"
    );

    let refused = Bot::start(config(&slack, "xoxb-test", "xapp-wrong", &nowhere)).await;
    assert!(
        matches!(refused, Err(StartError::AppToken(_))),
        "{:?}",
        refused.err()
    );

    let refused = Bot::start(config(&slack, "xoxb-test", "xapp-test", &nowhere)).await;
    let Err(StartError::Staw { url, .. }) = refused else {
        panic!("staw is not answering: {:?}", refused.err());
    };
    assert_eq!(url, nowhere);
}
