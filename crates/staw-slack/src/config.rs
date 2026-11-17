//! Configuration: TOML files and `STAW_SLACK_*` environment variables.
//!
//! Layers, weakest first, merged per key (the server's convention):
//!
//! 1. `/etc/staw/slack.toml`
//! 2. `./staw-slack.toml`
//! 3. the file named by `--config` or `$STAW_SLACK_CONFIG`, which must
//!    exist (a misspelled path fails loudly, it does not fall back)
//! 4. `STAW_SLACK_*` environment variables; nested keys use `__`
//!    (`STAW_SLACK_SLACK__BOT_TOKEN` → `slack.bot_token`)
//!
//! The bot is off by default (rule 7): with no file and no variable,
//! [`load`] returns [`Loaded::Off`] with what is missing, and the binary
//! exits without starting. A configuration that is present but unusable
//! is an [`Error`] that says why — every missing or wrong key at once,
//! not the first one serde trips on.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use config::{Environment, File, FileFormat};
use serde::Deserialize;
use staw_expert::{Endpoint, Provider};

/// The system-wide file.
pub const SYSTEM_FILE: &str = "/etc/staw/slack.toml";
/// The working directory's file — a development convenience.
pub const LOCAL_FILE: &str = "staw-slack.toml";
/// Names an explicit file (the `--config` flag wins over it).
pub const FILE_VARIABLE: &str = "STAW_SLACK_CONFIG";
/// The prefix of the environment layer.
const PREFIX: &str = "STAW_SLACK";

/// What [`load`] found.
#[derive(Debug)]
pub enum Loaded {
    /// Nothing is configured: the bot is off, and the message says what
    /// to set to turn it on.
    Off(String),
    /// A usable configuration.
    On(Box<Config>),
}

/// A configuration that exists but cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A file is missing, unreadable, or not TOML.
    #[error("{0}")]
    Read(String),
    /// Required keys are absent.
    #[error("the configuration is incomplete. Missing: {}", .0.join("; "))]
    Incomplete(Vec<String>),
    /// Keys are present with values that cannot work.
    #[error("the configuration is not usable: {}", .0.join("; "))]
    Invalid(Vec<String>),
}

/// The validated configuration.
#[derive(Clone)]
pub struct Config {
    pub slack: Slack,
    pub staw: Staw,
    pub model: Model,
    /// Slack channel id → Staw project id (`[[channels]]`).
    pub channels: BTreeMap<String, String>,
    /// The file that keeps people's Staw refresh tokens (`[store] path`).
    pub store: PathBuf,
    /// The tracing filter (`[log] filter`, default `info`).
    pub log: String,
    /// The files that existed and merged, weakest first — for the
    /// startup log.
    pub sources: Vec<String>,
}

/// `[slack]`.
#[derive(Clone)]
pub struct Slack {
    /// The bot token (`xoxb-…`): the Web API.
    pub bot_token: String,
    /// The app-level token with `connections:write` (`xapp-…`): Socket
    /// Mode.
    pub app_token: String,
    /// The Web API root, without a trailing slash. Slack's own unless a
    /// test (or a government cloud) says otherwise.
    pub api_url: String,
}

/// `[staw]`.
#[derive(Clone, Debug)]
pub struct Staw {
    /// Staw's base URL, without a trailing slash. People open its links
    /// too, so it is the URL a browser reaches Staw at.
    pub url: String,
}

/// `[model]`: an [`Endpoint`] plus the bot's budgets.
#[derive(Clone)]
pub struct Model {
    pub endpoint: Endpoint,
    /// Model calls per request, at most (`rounds`, default 8).
    pub rounds: usize,
    /// Time for the model's whole loop on one request (`budget_secs`,
    /// default 180).
    pub budget: Duration,
    /// How much thread text the model reads, in characters
    /// (`thread_chars`, default 60 000). Rule 6 decides what happens to
    /// the rest.
    pub thread_chars: usize,
}

impl std::fmt::Debug for Config {
    // By hand: the tokens and the model key never reach a log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("slack.api_url", &self.slack.api_url)
            .field("staw", &self.staw)
            .field("model", &self.model.endpoint.model)
            .field("channels", &self.channels)
            .field("store", &self.store)
            .field("sources", &self.sources)
            .finish_non_exhaustive()
    }
}

/// Load the layered configuration. `explicit` is the `--config` flag;
/// without it, `$STAW_SLACK_CONFIG` names the explicit file.
pub fn load(explicit: Option<&Path>) -> Result<Loaded, Error> {
    let explicit = explicit
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os(FILE_VARIABLE).map(PathBuf::from));
    let mut builder = config::Config::builder();
    let mut sources = Vec::new();
    for file in [SYSTEM_FILE, LOCAL_FILE] {
        if Path::new(file).is_file() {
            builder = builder.add_source(File::new(file, FileFormat::Toml));
            sources.push(file.to_string());
        }
    }
    if let Some(path) = explicit {
        if !path.is_file() {
            return Err(Error::Read(format!(
                "the configuration file {} does not exist",
                path.display()
            )));
        }
        builder = builder.add_source(File::from(path.as_path()).format(FileFormat::Toml));
        sources.push(path.display().to_string());
    }
    let variables: HashMap<String, String> = std::env::vars()
        .filter(|(key, _)| key.starts_with("STAW_SLACK_") && key != FILE_VARIABLE)
        .collect();
    if sources.is_empty() && variables.is_empty() {
        return Ok(Loaded::Off(off()));
    }
    let raw: Raw = builder
        .add_source(
            Environment::with_prefix(PREFIX)
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true)
                .source(Some(variables)),
        )
        .build()
        .and_then(config::Config::try_deserialize)
        .map_err(|e| Error::Read(format!("cannot read the configuration: {e}")))?;
    validate(raw, sources).map(|config| Loaded::On(Box::new(config)))
}

/// One TOML document, validated the same way — for tests and tools that
/// hold the configuration as text.
pub fn parse(toml: &str) -> Result<Config, Error> {
    let raw: Raw = config::Config::builder()
        .add_source(File::from_str(toml, FileFormat::Toml))
        .build()
        .and_then(config::Config::try_deserialize)
        .map_err(|e| Error::Read(format!("cannot read the configuration: {e}")))?;
    validate(raw, Vec::new())
}

/// What an operator must set, in the words the errors use.
const REQUIRED: [&str; 6] = [
    "[slack] bot_token, the bot token (xoxb-…)",
    "[slack] app_token, an app-level token with connections:write (xapp-…)",
    "[staw] url, the base URL of Staw",
    "[model] provider and model, the model that reads threads",
    "[[channels]], at least one Slack channel id mapped to a Staw project id",
    "[store] path, the file that keeps people's Staw tokens",
];

fn off() -> String {
    format!(
        "staw-slack is off: no configuration found. It reads {SYSTEM_FILE}, ./{LOCAL_FILE}, the \
         file named by --config or ${FILE_VARIABLE}, and STAW_SLACK_* environment variables. To \
         turn it on, set: {}. The \"Slack bot\" section of Staw's README shows a complete file.",
        REQUIRED.join("; ")
    )
}

// ---- the raw shape: everything optional, so that every gap is reported ----

#[derive(Deserialize, Default)]
struct Raw {
    #[serde(default)]
    slack: RawSlack,
    #[serde(default)]
    staw: RawStaw,
    #[serde(default)]
    model: RawModel,
    #[serde(default)]
    channels: Vec<RawChannel>,
    #[serde(default)]
    store: RawStore,
    #[serde(default)]
    log: RawLog,
}

#[derive(Deserialize, Default)]
struct RawSlack {
    bot_token: Option<String>,
    app_token: Option<String>,
    api_url: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawStaw {
    url: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawModel {
    provider: Option<Provider>,
    model: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    api_key_cmd: Option<String>,
    timeout_secs: Option<f64>,
    max_tokens: Option<u32>,
    rounds: Option<usize>,
    budget_secs: Option<f64>,
    thread_chars: Option<usize>,
}

#[derive(Deserialize)]
struct RawChannel {
    channel: Option<String>,
    project: Option<String>,
}

#[derive(Deserialize, Default)]
struct RawStore {
    path: Option<PathBuf>,
}

#[derive(Deserialize, Default)]
struct RawLog {
    filter: Option<String>,
}

/// The bot's own defaults. Staw Expert's (30 seconds, 1,024 tokens) are
/// sized for one-shot enrichment, not for a loop that reads a thread.
const TIMEOUT_SECS: f64 = 120.0;
const MAX_TOKENS: u32 = 4096;
const ROUNDS: usize = 8;
const BUDGET_SECS: f64 = 180.0;
const THREAD_CHARS: usize = 60_000;

fn validate(raw: Raw, sources: Vec<String>) -> Result<Config, Error> {
    let present = |value: &Option<String>| value.as_deref().is_some_and(|v| !v.trim().is_empty());
    let mut missing = Vec::new();
    if !present(&raw.slack.bot_token) {
        missing.push(REQUIRED[0]);
    }
    if !present(&raw.slack.app_token) {
        missing.push(REQUIRED[1]);
    }
    if !present(&raw.staw.url) {
        missing.push(REQUIRED[2]);
    }
    if raw.model.provider.is_none() || !present(&raw.model.model) {
        missing.push(REQUIRED[3]);
    }
    if raw.channels.is_empty() {
        missing.push(REQUIRED[4]);
    }
    if raw
        .store
        .path
        .as_deref()
        .is_none_or(|p| p.as_os_str().is_empty())
    {
        missing.push(REQUIRED[5]);
    }
    if !missing.is_empty() {
        return Err(Error::Incomplete(
            missing.into_iter().map(str::to_string).collect(),
        ));
    }

    let mut invalid = Vec::new();
    let bot_token = raw.slack.bot_token.unwrap_or_default().trim().to_string();
    if !bot_token.starts_with("xoxb-") {
        invalid.push("[slack] bot_token must be a bot token, which starts with xoxb-".to_string());
    }
    let app_token = raw.slack.app_token.unwrap_or_default().trim().to_string();
    if !app_token.starts_with("xapp-") {
        invalid.push(
            "[slack] app_token must be an app-level token, which starts with xapp-".to_string(),
        );
    }
    let api_url = raw
        .slack
        .api_url
        .unwrap_or_else(|| "https://slack.com/api".into());
    let api_url = base_url(&api_url, "[slack] api_url", &mut invalid);
    let staw_url = base_url(
        raw.staw.url.as_deref().unwrap_or_default(),
        "[staw] url",
        &mut invalid,
    );

    let model = raw.model;
    let timeout_secs = model.timeout_secs.unwrap_or(TIMEOUT_SECS);
    if !(timeout_secs > 0.0 && timeout_secs.is_finite()) {
        invalid.push("[model] timeout_secs must be a positive number of seconds".into());
    }
    let max_tokens = model.max_tokens.unwrap_or(MAX_TOKENS);
    if max_tokens == 0 {
        invalid.push("[model] max_tokens must be more than 0".into());
    }
    let rounds = model.rounds.unwrap_or(ROUNDS);
    if !(1..=32).contains(&rounds) {
        invalid.push("[model] rounds must be from 1 to 32".into());
    }
    let budget_secs = model.budget_secs.unwrap_or(BUDGET_SECS);
    if !(budget_secs > 0.0 && budget_secs.is_finite()) {
        invalid.push("[model] budget_secs must be a positive number of seconds".into());
    }
    let thread_chars = model.thread_chars.unwrap_or(THREAD_CHARS);
    if thread_chars < 1_000 {
        invalid.push("[model] thread_chars must be at least 1000".into());
    }

    let mut channels = BTreeMap::new();
    for (n, entry) in raw.channels.into_iter().enumerate() {
        let at = format!("[[channels]] entry {}", n + 1);
        let channel = entry.channel.unwrap_or_default().trim().to_string();
        let project = entry.project.unwrap_or_default().trim().to_uppercase();
        if !is_channel_id(&channel) {
            invalid.push(format!(
                "{at}: channel must be a Slack channel id such as C0123ABCD (in Slack: the \
                 channel's details, at the bottom), got \"{channel}\""
            ));
            continue;
        }
        if !is_ulid(&project) {
            invalid.push(format!(
                "{at}: project must be a Staw project id (26 characters), got \"{project}\""
            ));
            continue;
        }
        if channels.insert(channel.clone(), project).is_some() {
            invalid.push(format!("{at}: channel {channel} is mapped twice"));
        }
    }

    if !invalid.is_empty() {
        return Err(Error::Invalid(invalid));
    }
    Ok(Config {
        slack: Slack {
            bot_token,
            app_token,
            api_url,
        },
        staw: Staw { url: staw_url },
        model: Model {
            endpoint: Endpoint {
                provider: model.provider.expect("checked above"),
                model: model.model.unwrap_or_default().trim().to_string(),
                base_url: model.base_url,
                api_key: model.api_key,
                api_key_cmd: model.api_key_cmd,
                timeout_secs,
                max_tokens,
            },
            rounds,
            budget: Duration::from_secs_f64(budget_secs),
            thread_chars,
        },
        channels,
        store: raw.store.path.expect("checked above"),
        log: raw.log.filter.unwrap_or_else(|| "info".into()),
        sources,
    })
}

/// An http(s) URL without query or fragment, returned without its
/// trailing slash so that paths join with one.
fn base_url(value: &str, key: &str, invalid: &mut Vec<String>) -> String {
    let trimmed = value.trim().trim_end_matches('/');
    match url::Url::parse(trimmed) {
        Ok(url)
            if matches!(url.scheme(), "http" | "https")
                && url.host().is_some()
                && url.query().is_none()
                && url.fragment().is_none() =>
        {
            trimmed.to_string()
        }
        _ => {
            invalid.push(format!(
                "{key} must be an http or https URL with no query, got \"{value}\""
            ));
            String::new()
        }
    }
}

/// A public (`C…`) or private (`C…`, older `G…`) channel id. Direct
/// messages (`D…`) are never mapped.
fn is_channel_id(id: &str) -> bool {
    id.len() >= 9
        && matches!(id.as_bytes()[0], b'C' | b'G')
        && id
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// A ULID in Crockford's base32, as Staw prints ids.
fn is_ulid(id: &str) -> bool {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    id.len() == 26 && id.as_bytes()[0] <= b'7' && id.bytes().all(|b| ALPHABET.contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMPLETE: &str = r#"
        [slack]
        bot_token = "xoxb-1"
        app_token = "xapp-1"

        [staw]
        url = "https://staw.example.com/"

        [model]
        provider = "anthropic"
        model = "claude-sonnet-5-5"
        api_key = "k"

        [[channels]]
        channel = "C0123ABCD"
        project = "01m2p205ak2gmnwss0bqgm6en9"

        [store]
        path = "/var/lib/staw-slack/people.json"
    "#;

    #[test]
    fn a_complete_file_gets_the_bots_defaults() {
        let config = parse(COMPLETE).unwrap();
        assert_eq!(config.staw.url, "https://staw.example.com");
        assert_eq!(config.slack.api_url, "https://slack.com/api");
        assert_eq!(
            config.channels.get("C0123ABCD").map(String::as_str),
            Some("01M2P205AK2GMNWSS0BQGM6EN9")
        );
        assert_eq!(config.model.rounds, 8);
        assert_eq!(config.model.budget, Duration::from_secs(180));
        assert_eq!(config.model.endpoint.timeout_secs, 120.0);
        assert_eq!(config.model.endpoint.max_tokens, 4096);
        assert_eq!(config.log, "info");
    }

    #[test]
    fn every_missing_key_is_named_at_once() {
        let Err(Error::Incomplete(missing)) = parse("[slack]\nbot_token = \"xoxb-1\"\n") else {
            panic!("an incomplete file is an error");
        };
        assert_eq!(missing.len(), 5, "{missing:?}");
        assert!(missing.iter().any(|m| m.contains("app_token")));
        assert!(missing.iter().any(|m| m.contains("[[channels]]")));
        assert!(missing.iter().any(|m| m.contains("[store] path")));
    }

    #[test]
    fn wrong_values_say_why() {
        let broken = COMPLETE
            .replace("xoxb-1", "xoxp-1")
            .replace("C0123ABCD", "general")
            .replace("https://staw.example.com/", "staw.example.com");
        let Err(Error::Invalid(problems)) = parse(&broken) else {
            panic!("wrong values are an error");
        };
        assert!(problems.iter().any(|p| p.contains("xoxb-")), "{problems:?}");
        assert!(
            problems.iter().any(|p| p.contains("general")),
            "{problems:?}"
        );
        assert!(
            problems.iter().any(|p| p.contains("[staw] url")),
            "{problems:?}"
        );
    }

    #[test]
    fn a_channel_maps_to_one_project() {
        let twice = format!(
            "{COMPLETE}\n[[channels]]\nchannel = \"C0123ABCD\"\nproject = \"01M2P205AK2GMNWSS0BQGM6EN9\"\n"
        );
        let Err(Error::Invalid(problems)) = parse(&twice) else {
            panic!("a channel mapped twice is an error");
        };
        assert!(problems[0].contains("mapped twice"));
    }

    #[test]
    fn the_off_message_says_what_to_set() {
        let message = off();
        for item in REQUIRED {
            assert!(message.contains(item), "{item}");
        }
    }

    #[test]
    fn debug_output_keeps_secrets_out() {
        let dump = format!("{:?}", parse(COMPLETE).unwrap());
        assert!(
            !dump.contains("xoxb-1") && !dump.contains("xapp-1"),
            "{dump}"
        );
    }
}
