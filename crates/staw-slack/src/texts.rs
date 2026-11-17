//! Every fixed text the bot writes, in one table, in English (rule 9).
//!
//! Answers are the model's; everything else a person reads — connect
//! messages, refusals, errors, the App Home — comes from here, so the
//! wording is reviewed in one place and never improvised at a call site.
//! Values that come from people (names) are escaped for Slack's mrkdwn
//! before they are put in.

/// The reaction that acknowledges a request in the thread. It is the
/// only thing the bot puts in a channel in this version.
pub const ACK_REACTION: &str = "eyes";

/// The channel is shared with another organization (Slack Connect).
pub const SHARED_CHANNEL: &str = "This channel is shared with another organization, so I don't \
     work in it. Ask me in a channel of your own workspace.";

/// The channel is not in the bot's channel map.
pub const UNMAPPED_CHANNEL: &str = "This channel is not connected to a Staw project, so I can't \
     answer here. Ask the person who runs the Staw bot to connect it.";

/// The person's Staw account does not see the mapped project.
pub const NO_PROJECT_ACCESS: &str = "Your Staw account can't see the project that this channel is \
     connected to, so I can't answer from it. Ask the owner of that project's group in Staw to \
     add you.";

/// The person asked to record a decision; recording is step 2.
pub const RECORD_LATER: &str = "Recording decisions from Slack comes in the next version. For \
     now I can answer questions about the decisions in Staw.";

/// Staw could not be reached, or refused in a way the person cannot fix.
pub const STAW_UNAVAILABLE: &str = "Staw did not answer, so I could not finish your request. \
     Try again in a few minutes.";

/// The thread could not be read from Slack.
pub const SLACK_UNAVAILABLE: &str = "I could not read this thread from Slack, so I could not \
     finish your request. Try again in a few minutes.";

/// The model failed or gave nothing usable.
pub const MODEL_UNAVAILABLE: &str = "The model that I use did not answer, so I could not finish \
     your request. Try again in a few minutes.";

/// The request ran out of its time budget.
pub const TOO_LONG: &str = "Your request took too long, so I stopped it. Try a narrower \
     question.";

/// Pressing Disconnect while not connected.
pub const NOT_CONNECTED: &str = "You are not connected to Staw, so there was nothing to \
     disconnect.";

/// The approval was denied on the pair screen.
pub const CONNECT_DENIED: &str =
    "Connecting to Staw was denied. Mention me again in a thread to start over.";

/// The code expired before anyone approved it.
pub const CONNECT_EXPIRED: &str = "The code expired before it was approved. Mention me again in \
     a thread to get a new one.";

/// Connecting failed for a reason the person cannot fix.
pub const CONNECT_FAILED: &str =
    "I could not connect you to Staw. Try again in a few minutes by mentioning me in a thread.";

/// The button on the App Home.
pub const DISCONNECT_BUTTON: &str = "Disconnect";

/// The confirmation dialog of the button.
pub const DISCONNECT_CONFIRM_TITLE: &str = "Disconnect from Staw?";
pub const DISCONNECT_CONFIRM_TEXT: &str =
    "I will delete your Staw token. You can connect again later.";
pub const DISCONNECT_CONFIRM_YES: &str = "Disconnect";
pub const DISCONNECT_CONFIRM_NO: &str = "Cancel";

/// The App Home of a person who has not connected.
pub const HOME_NOT_CONNECTED: &str = "You are not connected to Staw. Mention me in a thread of a \
     channel that is connected to a Staw project, and I will send you a link to connect.";

/// The connect message: where to approve, the code, and how long it lives.
pub fn connect(link: &str, code: &str, minutes: u64) -> String {
    format!(
        "To answer you, I need to see what your Staw account sees. Connect your account: open \
         {link} and approve the code *{code}*. The code expires in {minutes} {unit}.",
        link = slack_link(link, "this link"),
        code = escape(code),
        unit = if minutes == 1 { "minute" } else { "minutes" },
    )
}

/// After the approval.
pub fn connected(name: &str) -> String {
    format!(
        "Connected as *{}*. Mention me in the thread again to ask.",
        escape(name)
    )
}

/// After Disconnect: the token is gone here, and how to revoke it in Staw.
pub fn disconnected(settings: &str, token: &str) -> String {
    format!(
        "Disconnected: I deleted your Staw token. To revoke it in Staw too, open {} and revoke \
         the token `connector:{}`.",
        slack_link(settings, "your Staw settings"),
        escape(token),
    )
}

/// The App Home of a connected person.
pub fn home_connected(name: &str) -> String {
    format!(
        "You are connected to Staw as *{}*. I answer you with what this account can see in \
         Staw.",
        escape(name)
    )
}

/// Rule 6: what part of a long thread the model did not read. `skipped`
/// whole messages were left out; `cut` very long ones were read only in
/// part.
pub fn not_read(read_newest: usize, skipped: usize, cut: usize) -> String {
    let mut text = String::from("_This thread is too long for me to read whole.");
    if skipped > 0 {
        text.push_str(&format!(
            " I read the first message and the newest {read_newest} {}, and skipped the other \
             {skipped} {}.",
            plural(read_newest, "message", "messages"),
            plural(skipped, "message", "messages"),
        ));
    }
    if cut > 0 {
        text.push_str(&format!(
            " I read only the beginning of {cut} very long {}.",
            plural(cut, "message", "messages"),
        ));
    }
    text.push('_');
    text
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

/// A Slack mrkdwn link. The URL is the bot's own (Staw's), the label a
/// fixed text.
fn slack_link(url: &str, label: &str) -> String {
    format!(
        "<{}|{}>",
        url.replace('|', "%7C").replace('>', "%3E"),
        label
    )
}

/// Slack's three control characters in mrkdwn text, as entities
/// (docs.slack.dev, "Formatting message text").
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_escaped() {
        assert_eq!(
            connected("A <b> & c"),
            "Connected as *A &lt;b&gt; &amp; c*. Mention me in the thread again to ask."
        );
    }

    #[test]
    fn the_connect_message_carries_the_link_and_the_code() {
        let text = connect("http://staw.test/#/pair/BCDF-GHJK", "BCDF-GHJK", 15);
        assert!(text.contains("<http://staw.test/#/pair/BCDF-GHJK|this link>"));
        assert!(text.contains("*BCDF-GHJK*"));
        assert!(text.contains("15 minutes"));
    }

    #[test]
    fn counts_agree_in_number() {
        assert!(not_read(1, 1, 0).contains("newest 1 message, and skipped the other 1 message."));
        assert!(
            not_read(40, 56, 0).contains("newest 40 messages, and skipped the other 56 messages.")
        );
        let cut = not_read(3, 0, 2);
        assert!(!cut.contains("skipped"), "{cut}");
        assert!(
            cut.contains("the beginning of 2 very long messages."),
            "{cut}"
        );
    }
}
