//! Slack Web API calls and the Socket Mode connection URL.

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};

pub struct Slack {
    http: Client,
    bot_token: String,
    app_token: String,
}

impl Slack {
    pub fn new(http: Client, bot_token: String, app_token: String) -> Self {
        Self { http, bot_token, app_token }
    }

    async fn api(&self, method: &str, body: Value) -> Result<Value> {
        let v: Value = self
            .http
            .post(format!("https://slack.com/api/{method}"))
            .bearer_auth(&self.bot_token)
            .header("Content-Type", "application/json; charset=utf-8")
            .body(body.to_string())
            .send()
            .await?
            .json()
            .await?;
        if v["ok"].as_bool() != Some(true) {
            bail!("Slack {method} failed: {}", v["error"].as_str().unwrap_or("unknown error"));
        }
        Ok(v)
    }

    /// WebSocket URL for Socket Mode (uses the app-level token).
    pub async fn socket_url(&self) -> Result<String> {
        let v: Value = self
            .http
            .post("https://slack.com/api/apps.connections.open")
            .bearer_auth(&self.app_token)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .send()
            .await?
            .json()
            .await?;
        if v["ok"].as_bool() != Some(true) {
            bail!("apps.connections.open failed: {}", v["error"].as_str().unwrap_or("unknown error"));
        }
        v["url"].as_str().map(String::from).context("no Socket Mode URL returned")
    }

    pub async fn open_dm(&self, user: &str) -> Result<String> {
        let v = self.api("conversations.open", json!({ "users": user })).await?;
        v["channel"]["id"].as_str().map(String::from).context("conversations.open returned no channel")
    }

    /// Posts a message and returns its `ts`.
    pub async fn post(&self, channel: &str, text: &str, blocks: Option<Value>, thread_ts: Option<&str>) -> Result<String> {
        let mut body = json!({ "channel": channel, "text": text, "unfurl_links": false });
        if let Some(b) = blocks {
            body["blocks"] = b;
        }
        if let Some(t) = thread_ts {
            body["thread_ts"] = json!(t);
        }
        let v = self.api("chat.postMessage", body).await?;
        Ok(v["ts"].as_str().unwrap_or_default().to_string())
    }

    pub async fn update(&self, channel: &str, ts: &str, text: &str, blocks: Option<Value>) -> Result<()> {
        let mut body = json!({ "channel": channel, "ts": ts, "text": text });
        if let Some(b) = blocks {
            body["blocks"] = b;
        }
        self.api("chat.update", body).await.map(|_| ())
    }

    pub async fn publish_home(&self, user: &str, view: Value) -> Result<()> {
        self.api("views.publish", json!({ "user_id": user, "view": view })).await.map(|_| ())
    }

    /// Ephemeral reply to a slash command.
    pub async fn respond(&self, response_url: &str, text: &str) -> Result<()> {
        self.http
            .post(response_url)
            .json(&json!({ "response_type": "ephemeral", "text": text }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Escapes text for Slack mrkdwn.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Splits Markdown into chunks of at most ~`max` chars on line boundaries,
/// closing and reopening code fences that span a split.
pub fn chunk_markdown(text: &str, max: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut cur = String::new();
    let mut in_fence = false;
    for line in text.lines() {
        if !cur.is_empty() && cur.len() + line.len() + 8 > max {
            if in_fence {
                cur.push_str("```\n");
            }
            chunks.push(std::mem::take(&mut cur));
            if in_fence {
                cur.push_str("```\n");
            }
        }
        cur.push_str(line);
        cur.push('\n');
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
        }
    }
    if !cur.trim().is_empty() {
        chunks.push(cur);
    }
    chunks
}
