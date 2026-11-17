//! `staw-slack`: Staw's bot for Slack.
//!
//! A person mentions the bot in a thread of a channel that is mapped to a
//! Staw project and asks a question; the bot answers in a direct message,
//! from the decisions that this person can see in Staw. The thread gets
//! a reaction and nothing else.
//!
//! The bot is a separate process and one more MCP client of Staw
//! (decision `01M7K7770Y8SV2EDD2FZ3VE8C2`):
//!
//! - It reaches Slack through Socket Mode, an outbound WebSocket, so it
//!   needs no public URL ([`socket`]), and the Web API ([`slack`]).
//! - It reaches Staw only through `/mcp`, the OAuth endpoints, and two
//!   reads ([`staw`]) — always with the token of the person who asked
//!   ([`people`]); there is no shared token.
//! - Its own model reads the thread and the request in free text and
//!   answers with read-only Staw tools ([`agent`]). The model has no tool
//!   that writes to Staw.
//! - A small local file keeps people's refresh tokens ([`store`]).
//!
//! Off by default: with no configuration, the binary says what is
//! missing and exits ([`config`]).

pub mod agent;
pub mod bot;
pub mod config;
pub mod people;
pub mod slack;
mod socket;
pub mod staw;
pub mod store;
pub mod texts;
pub mod thread;

pub use bot::{Bot, StartError};
