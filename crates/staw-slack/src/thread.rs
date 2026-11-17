//! A Slack thread as the model reads it: numbered lines with speakers.
//!
//! - Lines are numbered by their position in the thread, from 0 (the
//!   parent). The bot's own messages are never part of what the model
//!   reads (rule 4); their positions are left out, not renumbered, so a
//!   line keeps its number as the thread grows.
//! - A line from an app is marked as such (rule 4), and the line that
//!   mentioned the bot is marked as the request.
//! - A thread longer than the budget is never cut in silence (rule 6):
//!   the model reads the first line and the newest lines that fit — the
//!   request among them — and both the model and the person are told
//!   which part was not read.

use std::collections::{BTreeSet, HashMap};

use crate::slack::{Identity, Message};

/// One line of the thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The position in the thread, from 0.
    pub position: usize,
    pub speaker: String,
    /// Posted by an app, not a person.
    pub app: bool,
    /// The line that mentioned the bot.
    pub request: bool,
    pub text: String,
}

/// What the model reads of one thread.
#[derive(Debug, Clone)]
pub struct Thread {
    /// The lines read, oldest first.
    pub lines: Vec<Line>,
    /// Positions not read for the budget, as inclusive ranges.
    pub skipped: Vec<(usize, usize)>,
    /// How many lines were skipped.
    pub skipped_count: usize,
    /// Lines whose text was cut to fit (a single line longer than a
    /// quarter of the budget).
    pub cut: usize,
}

impl Thread {
    /// The note for the person when part of the thread was not read.
    pub fn note(&self) -> Option<String> {
        (self.skipped_count > 0 || self.cut > 0).then(|| {
            let newest = self.lines.len().saturating_sub(1);
            crate::texts::not_read(newest, self.skipped_count, self.cut)
        })
    }

    /// The user turn: the thread, numbered and marked, then what was not
    /// read.
    pub fn render(&self) -> String {
        let mut out = String::from(
            "The thread, oldest first. Each line is \"[position] speaker: text\". \"(app)\" marks \
             a line that an app posted, not a person. \"(request)\" marks the line that mentioned \
             you: the person's request.\n\n",
        );
        for line in &self.lines {
            out.push_str(&render_line(line));
            out.push('\n');
        }
        if !self.skipped.is_empty() {
            let ranges: Vec<String> = self
                .skipped
                .iter()
                .map(|&(a, b)| match a == b {
                    true => a.to_string(),
                    false => format!("{a}-{b}"),
                })
                .collect();
            out.push_str(&format!(
                "\nNot read: lines {} (the thread is longer than your reading budget). The person \
                 is told so.\n",
                ranges.join(", ")
            ));
        }
        if self.cut > 0 {
            out.push_str(
                "\nSome very long lines are cut; \"[cut]\" marks where. The person is told so.\n",
            );
        }
        out
    }
}

fn render_line(line: &Line) -> String {
    let mut mark = String::new();
    if line.app {
        mark.push_str(" (app)");
    }
    if line.request {
        mark.push_str(" (request)");
    }
    format!(
        "[{}] {}{}: {}",
        line.position,
        line.speaker,
        mark,
        line.text.replace('\n', "\n    ")
    )
}

/// The user ids a thread names: its authors and the people its lines
/// mention, for the caller to resolve to names.
pub fn people(messages: &[Message]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for message in messages {
        if let Some(user) = &message.user {
            ids.insert(user.clone());
        }
        ids.extend(mentions(&message.text));
    }
    ids
}

/// `<@U…>` and `<@U…|name>` in a message's text.
fn mentions(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('>') else { break };
        let id = rest[..end].split('|').next().unwrap_or_default();
        if !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            found.push(id.to_string());
        }
        rest = &rest[end + 1..];
    }
    found
}

/// Replace `<@U…>` with `@name`.
fn named(text: &str, names: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<@") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('>') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &after[..end];
        let mut parts = inner.splitn(2, '|');
        let id = parts.next().unwrap_or_default();
        let label = parts.next();
        match names.get(id).map(String::as_str).or(label) {
            Some(name) => {
                out.push('@');
                out.push_str(name);
            }
            None => out.push_str(&rest[start..start + 2 + end + 1]),
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Build what the model reads from `conversations.replies`: the bot's own
/// messages dropped, apps marked, the request (`request_ts`) marked, and
/// at most `budget` characters.
pub fn build(
    messages: &[Message],
    request_ts: &str,
    request_text: &str,
    bot: &Identity,
    names: &HashMap<String, String>,
    budget: usize,
) -> Thread {
    let mut names = names.clone();
    names.insert(bot.user_id.clone(), bot.user.clone());
    let cap = budget / 4;
    let mut cut = 0;
    let mut lines: Vec<Line> = Vec::new();
    for (position, message) in messages.iter().enumerate() {
        let own = message.user.as_deref() == Some(bot.user_id.as_str())
            || (message.bot_id.is_some() && message.bot_id == bot.bot_id);
        if own {
            continue;
        }
        let app = message.bot_id.is_some() || message.subtype.as_deref() == Some("bot_message");
        let speaker = match (&message.user, app) {
            (Some(user), false) => names.get(user).cloned().unwrap_or_else(|| user.clone()),
            _ => message
                .bot_profile
                .as_ref()
                .and_then(|p| p.name.clone())
                .or_else(|| message.username.clone())
                .unwrap_or_else(|| "an app".into()),
        };
        let mut text = named(&message.text, &names);
        if text.chars().count() > cap {
            text = text.chars().take(cap).collect::<String>() + " [cut]";
            cut += 1;
        }
        lines.push(Line {
            position,
            speaker,
            app,
            request: message.ts == request_ts,
            text,
        });
    }
    // The mention should be in the thread; if Slack did not return it
    // (deleted meanwhile), the request still reaches the model.
    if !lines.iter().any(|l| l.request) {
        lines.push(Line {
            position: messages.len(),
            speaker: "the person who asked".into(),
            app: false,
            request: true,
            text: named(request_text, &names),
        });
    }

    let size = |line: &Line| render_line(line).chars().count() + 1;
    let total: usize = lines.iter().map(size).sum();
    if total <= budget {
        return Thread {
            lines,
            skipped: Vec::new(),
            skipped_count: 0,
            cut,
        };
    }
    // Keep the first line and the request, then the newest that fit.
    let mut keep = vec![false; lines.len()];
    let mut used = 0;
    for (i, line) in lines.iter().enumerate() {
        if i == 0 || line.request {
            keep[i] = true;
            used += size(line);
        }
    }
    for i in (1..lines.len()).rev() {
        if keep[i] {
            continue;
        }
        let cost = size(&lines[i]);
        if used + cost > budget {
            break;
        }
        keep[i] = true;
        used += cost;
    }
    let mut skipped: Vec<(usize, usize)> = Vec::new();
    let mut read = Vec::new();
    for (line, kept) in lines.into_iter().zip(keep) {
        if kept {
            read.push(line);
            continue;
        }
        match skipped.last_mut() {
            Some((_, end)) if *end + 1 == line.position => *end = line.position,
            _ => skipped.push((line.position, line.position)),
        }
    }
    let skipped_count = skipped.iter().map(|(a, b)| b - a + 1).sum();
    Thread {
        lines: read,
        skipped,
        skipped_count,
        cut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bot() -> Identity {
        Identity {
            team: "Acme".into(),
            team_id: "T1".into(),
            user: "staw".into(),
            user_id: "UBOT".into(),
            bot_id: Some("BBOT".into()),
        }
    }

    fn message(ts: &str, user: Option<&str>, text: &str) -> Message {
        Message {
            ts: ts.into(),
            text: text.into(),
            user: user.map(str::to_string),
            bot_id: None,
            subtype: None,
            username: None,
            bot_profile: None,
        }
    }

    fn names() -> HashMap<String, String> {
        [("U1", "Alice"), ("U2", "Bob")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn lines_are_numbered_marked_and_named() {
        let mut deploy = message("2", None, "build 42 passed");
        deploy.bot_id = Some("B9".into());
        deploy.bot_profile = Some(crate::slack::BotProfile {
            name: Some("Deploy".into()),
        });
        let mut own = message("3", Some("UBOT"), "an earlier answer");
        own.bot_id = Some("BBOT".into());
        let messages = vec![
            message("1", Some("U1"), "we need a queue"),
            deploy,
            own,
            message("4", Some("U2"), "<@UBOT> what did <@U1> decide?"),
        ];
        let thread = build(&messages, "4", "", &bot(), &names(), 10_000);
        let rendered: Vec<String> = thread.lines.iter().map(render_line).collect();
        assert_eq!(
            rendered,
            [
                "[0] Alice: we need a queue",
                "[1] Deploy (app): build 42 passed",
                "[3] Bob (request): @staw what did @Alice decide?",
            ],
            "the bot's own line is left out and its position is not reused"
        );
        assert_eq!(thread.note(), None);
        assert_eq!(
            people(&messages).into_iter().collect::<Vec<_>>(),
            ["U1", "U2", "UBOT"]
        );
    }

    #[test]
    fn a_long_thread_keeps_its_first_line_and_newest_lines_and_says_so() {
        let mut messages = vec![message("0", Some("U1"), "the question")];
        for n in 1..100 {
            messages.push(message(&n.to_string(), Some("U2"), &"x".repeat(90)));
        }
        messages.push(message("100", Some("U1"), "<@UBOT> so?"));
        let thread = build(&messages, "100", "", &bot(), &names(), 2_000);
        assert_eq!(thread.lines[0].position, 0);
        assert!(thread.lines.last().unwrap().request);
        let read_newest = thread.lines.len() - 1;
        assert_eq!(thread.skipped_count + read_newest + 1, 101);
        assert_eq!(thread.skipped, [(1, 100 - read_newest)]);
        let note = thread.note().expect("the person is told");
        assert!(
            note.contains(&format!("newest {read_newest} messages")),
            "{note}"
        );
        assert!(
            thread
                .render()
                .contains(&format!("Not read: lines 1-{}", 100 - read_newest))
        );
        let size: usize = thread.lines.iter().map(|l| render_line(l).len() + 1).sum();
        assert!(size <= 2_000);
    }

    #[test]
    fn a_vanished_request_still_reaches_the_model() {
        let messages = vec![message("1", Some("U1"), "the parent")];
        let thread = build(&messages, "9", "<@UBOT> hello", &bot(), &names(), 10_000);
        let last = thread.lines.last().unwrap();
        assert!(last.request);
        assert_eq!(last.text, "@staw hello");
    }

    #[test]
    fn a_huge_line_is_cut_and_the_cut_is_told() {
        let messages = vec![message("1", Some("U1"), &"y".repeat(5_000))];
        let thread = build(&messages, "1", "", &bot(), &names(), 4_000);
        assert_eq!(thread.cut, 1);
        assert!(thread.lines[0].text.ends_with("[cut]"));
        assert!(thread.note().is_some());
    }
}
