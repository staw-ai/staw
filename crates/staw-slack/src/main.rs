//! The `staw-slack` binary: load the configuration, check what the bot
//! depends on, serve Socket Mode until SIGINT or SIGTERM.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use staw_slack::Bot;
use staw_slack::config::{self, Loaded};
use tracing_subscriber::EnvFilter;

/// Staw's bot for Slack: answers questions in a direct message, from the
/// decisions that the asking person can see in Staw.
#[derive(Parser)]
#[command(name = "staw-slack", version)]
struct Args {
    /// A configuration file, merged over /etc/staw/slack.toml and
    /// ./staw-slack.toml. $STAW_SLACK_CONFIG names one too.
    #[arg(long, value_name = "FILE")]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let config = match config::load(args.config.as_deref()) {
        // Off by default: not an error, nothing to run.
        Ok(Loaded::Off(message)) => {
            eprintln!("{message}");
            return ExitCode::SUCCESS;
        }
        Ok(Loaded::On(config)) => *config,
        Err(e) => {
            eprintln!("staw-slack: {e}");
            return ExitCode::FAILURE;
        }
    };
    let filter = match EnvFilter::try_new(&config.log) {
        Ok(filter) => filter,
        Err(e) => {
            eprintln!("staw-slack: [log] filter is not usable: {e}");
            return ExitCode::FAILURE;
        }
    };
    tracing_subscriber::fmt().with_env_filter(filter).init();
    tracing::info!(sources = ?config.sources, "configuration loaded");

    let bot = match Bot::start(config).await {
        Ok(bot) => bot,
        Err(e) => {
            tracing::error!(error = %e, "staw-slack did not start");
            eprintln!("staw-slack: {e}");
            return ExitCode::FAILURE;
        }
    };
    tokio::select! {
        () = bot.run() => ExitCode::FAILURE,
        () = shutdown() => {
            tracing::info!("staw-slack stopping");
            ExitCode::SUCCESS
        }
    }
}

/// Resolves on SIGINT (ctrl-c) or SIGTERM (`docker stop`, systemd).
async fn shutdown() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install ctrl-c handler")
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
