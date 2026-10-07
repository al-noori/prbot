//! One Slack app shared by a team. Everyone runs their own prbot with the same app, but Slack hands
//! each event to just one of the connected copies. A copy that gets someone else's event forwards it
//! into that person's DM with the bot, as a message with hidden metadata. Their prbot polls its DM,
//! picks the event up, deletes the note and handles it. If their prbot isn't running, the note stays
//! and tells them so.

use crate::bot::Bot;
use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const EVENT_TYPE: &str = "prbot_forward";
const POLL_EVERY: Duration = Duration::from_secs(10);

impl Bot {
    /// Hands an event from another team member to their own prbot.
    pub async fn forward(&self, kind: &str, p: &Value, user: &str) -> Result<()> {
        let what = match kind {
            "slash_commands" => format!(
                "`{} {}`",
                p["command"].as_str().unwrap_or("/prreview"),
                p["text"].as_str().unwrap_or_default()
            ),
            "interactive" => "Your click".into(),
            _ => "Your message".into(),
        };
        let text = format!(
            ":hourglass_flowing_sand: {what} is on its way to your prbot. If nothing happens within a minute, your prbot isn't running: \
             start it on your computer with `prbot`. Don't have one yet? Open my *Home* tab and follow the steps (about a minute)."
        );
        let dm = self.slack.open_dm(user).await?;
        let payload = json!({ "kind": kind, "payload": slim(kind, p).to_string() });
        self.slack.post_with_metadata(&dm, &text, EVENT_TYPE, payload).await?;
        if let Some(url) = p["response_url"].as_str().filter(|_| kind == "slash_commands") {
            self.slack.respond(url, ":hourglass_flowing_sand: Passing this to your prbot…").await?;
        }
        Ok(())
    }

    /// Home tab for someone in the workspace who has no prbot yet: one PowerShell line that installs
    /// prbot and joins this app. Guests (and anyone whose account can't be checked) get no team code.
    pub async fn show_setup_home(&self, user: &str) -> Result<()> {
        let member = match self.slack.user_info(user).await {
            Ok(u) => !(u["is_restricted"] == true || u["is_ultra_restricted"] == true || u["is_bot"] == true || u["deleted"] == true),
            Err(e) => {
                crate::log(&format!("could not check {user} before showing the team code: {e:#}"));
                false
            }
        };
        let intro = "*Get your own PR reviewer.* Claude reviews the GitHub PRs that request *your* review, posts the review on the PR \
                     (marked as written by Claude, not by you) and sends you a short summary here. It runs on your computer \
                     with your own GitHub and Claude accounts.";
        let mut blocks = vec![
            json!({ "type": "header", "text": { "type": "plain_text", "text": "PR Reviewer" } }),
            json!({ "type": "section", "text": { "type": "mrkdwn", "text": intro } }),
        ];
        if member {
            let line = crate::setup::setup_line()?;
            blocks.push(json!({ "type": "section", "text": { "type": "mrkdwn", "text":
                "*Set up on Windows (about a minute):* open PowerShell, paste this line and press Enter. It installs prbot \
                 (and the GitHub CLI and Claude Code if you don't have them), logs you in, and starts prbot whenever you log in." } }));
            blocks.push(json!({ "type": "section", "text": { "type": "mrkdwn", "text": format!("```{line}```") } }));
            blocks.push(json!({ "type": "context", "elements": [{ "type": "mrkdwn", "text": format!(
                "The line contains this team's code for the app: don't share it outside the team. macOS or Linux: see {}.",
                env!("CARGO_PKG_REPOSITORY")) }] }));
        } else {
            blocks.push(json!({ "type": "section", "text": { "type": "mrkdwn", "text": format!(
                "Ask a teammate who uses it to run `prbot invite` and send you the setup line, or see {}.",
                env!("CARGO_PKG_REPOSITORY")) } }));
        }
        self.slack.publish_home(user, json!({ "type": "home", "blocks": blocks })).await
    }

    /// Picks up events other copies forwarded into the owner's DM.
    pub async fn run_forward_poller(self: Arc<Self>) {
        let mut oldest = slack_ts(chrono::Utc::now());
        loop {
            tokio::time::sleep(POLL_EVERY).await;
            let messages = match self.slack.history(&self.dm, &oldest).await {
                Ok(m) => m,
                Err(e) => {
                    crate::log(&format!("could not read forwarded events: {e:#}"));
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    continue;
                }
            };
            // Newest first, so walk backwards to handle them in order.
            for m in messages.iter().rev() {
                let Some(ts) = m["ts"].as_str() else { continue };
                if ts.parse::<f64>().unwrap_or(0.0) > oldest.parse::<f64>().unwrap_or(0.0) {
                    oldest = ts.to_string();
                }
                let meta = &m["metadata"];
                if meta["event_type"] != EVENT_TYPE || m["bot_id"].is_null() {
                    continue;
                }
                let kind = meta["event_payload"]["kind"].as_str().unwrap_or_default().to_string();
                let Ok(payload) = serde_json::from_str::<Value>(meta["event_payload"]["payload"].as_str().unwrap_or("null")) else {
                    continue;
                };
                if let Err(e) = self.slack.delete(&self.dm, ts).await {
                    crate::log(&format!("could not delete a forwarded event: {e:#}"));
                }
                tokio::spawn(self.clone().handle_own(kind, payload));
            }
        }
    }
}

/// Slack timestamps are seconds with microseconds, e.g. "1696681234.123456".
fn slack_ts(t: chrono::DateTime<chrono::Utc>) -> String {
    format!("{}.{:06}", t.timestamp(), t.timestamp_subsec_micros())
}

/// Only what the handlers read, so the forwarded event stays well under Slack's metadata size limit.
/// Button clicks drop the clicked message; `post_to_github` fetches it again when needed.
fn slim(kind: &str, p: &Value) -> Value {
    match kind {
        "slash_commands" => json!({
            "user_id": p["user_id"], "command": p["command"], "text": p["text"], "response_url": p["response_url"],
        }),
        "interactive" => json!({
            "type": p["type"], "user": { "id": p["user"]["id"] }, "actions": p["actions"], "container": p["container"],
        }),
        _ => {
            let e = &p["event"];
            json!({ "event": {
                "type": e["type"], "user": e["user"], "text": e["text"], "channel": e["channel"],
                "channel_type": e["channel_type"], "ts": e["ts"], "tab": e["tab"],
            } })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slim_keeps_what_handlers_need() {
        let p = json!({ "type": "block_actions", "user": { "id": "U1", "name": "x" }, "actions": [{ "action_id": "toggle", "value": "on" }],
                        "container": { "channel_id": "D1", "message_ts": "1.2" }, "message": { "blocks": ["big"] } });
        let s = slim("interactive", &p);
        assert_eq!(s["user"]["id"], "U1");
        assert_eq!(s["actions"][0]["action_id"], "toggle");
        assert!(s["message"].is_null());
    }
}
