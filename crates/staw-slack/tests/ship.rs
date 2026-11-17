//! What ships: the bot is a second binary in Staw's image and runs only
//! under its compose profile — `docker compose up` without the profile
//! starts what it started before the bot existed (needs the Docker CLI
//! with its compose plugin) — and the Slack app manifest asks for what
//! the bot uses.

use std::path::PathBuf;
use std::process::Command;

fn services(profile: Option<&str>) -> Vec<String> {
    let compose = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../compose.yaml");
    let mut command = Command::new("docker");
    command.arg("compose").arg("-f").arg(&compose);
    if let Some(profile) = profile {
        command.args(["--profile", profile]);
    }
    let output = command
        .args(["config", "--services"])
        .output()
        .expect("run `docker compose` (is Docker installed?)");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut services: Vec<String> = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    services.sort();
    services
}

#[test]
fn compose_starts_the_bot_only_under_its_profile() {
    assert_eq!(services(None), ["postgres", "staw"]);
    assert_eq!(services(Some("slack")), ["postgres", "staw", "staw-slack"]);
}

#[test]
fn the_image_builds_both_binaries() {
    let dockerfile =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Dockerfile"))
            .unwrap();
    assert!(
        dockerfile.contains("cargo build --release -p staw-server -p staw-slack"),
        "{dockerfile}"
    );
    assert!(
        dockerfile.contains("/usr/local/bin/staw-slack"),
        "{dockerfile}"
    );
}

/// The manifest people paste asks Slack for what the bot uses: the scopes
/// of the methods it calls and the events it handles, Socket Mode, and
/// interactivity for the App Home's button.
#[test]
fn the_manifest_asks_for_what_the_bot_uses() {
    let manifest =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("manifest.yaml"))
            .unwrap();
    for needed in [
        "- app_mentions:read",
        "- channels:history",
        "- groups:history",
        "- chat:write",
        "- reactions:write",
        "- users:read",
        "- app_mention",
        "- app_home_opened",
        "socket_mode_enabled: true",
        "home_tab_enabled: true",
        "messages_tab_enabled: true",
    ] {
        assert!(manifest.contains(needed), "{needed}");
    }
    assert!(
        manifest.contains("interactivity:\n    is_enabled: true"),
        "{manifest}"
    );
}
