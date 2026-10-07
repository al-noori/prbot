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

    /// Read methods take query parameters rather than a JSON body.
    async fn api_get(&self, method: &str, params: &[(&str, &str)]) -> Result<Value> {
        let v: Value = self
            .http
            .get(format!("https://slack.com/api/{method}"))
            .bearer_auth(&self.bot_token)
            .query(params)
            .send()
            .await?
            .json()
            .await?;
        if v["ok"].as_bool() != Some(true) {
            bail!("Slack {method} failed: {}", v["error"].as_str().unwrap_or("unknown error"));
        }
        Ok(v)
    }

    /// Messages in `channel` newer than `oldest`, newest first, including their metadata.
    pub async fn history(&self, channel: &str, oldest: &str) -> Result<Vec<Value>> {
        let v = self
            .api_get(
                "conversations.history",
                &[("channel", channel), ("oldest", oldest), ("limit", "100"), ("include_all_metadata", "true")],
            )
            .await?;
        Ok(v["messages"].as_array().cloned().unwrap_or_default())
    }

    /// One message by its `ts`.
    pub async fn message(&self, channel: &str, ts: &str) -> Result<Value> {
        let v = self
            .api_get("conversations.history", &[("channel", channel), ("latest", ts), ("inclusive", "true"), ("limit", "1")])
            .await?;
        v["messages"].get(0).cloned().context("message not found")
    }

    /// Posts a message carrying hidden structured metadata. Returns its `ts`.
    pub async fn post_with_metadata(&self, channel: &str, text: &str, event_type: &str, payload: Value) -> Result<String> {
        let body = json!({
            "channel": channel,
            "text": text,
            "unfurl_links": false,
            "metadata": { "event_type": event_type, "event_payload": payload },
        });
        let v = self.api("chat.postMessage", body).await?;
        Ok(v["ts"].as_str().unwrap_or_default().to_string())
    }

    pub async fn delete(&self, channel: &str, ts: &str) -> Result<()> {
        self.api("chat.delete", json!({ "channel": channel, "ts": ts })).await.map(|_| ())
    }

    /// The Slack user with this email: `Ok(None)` if there is none, an error if the app may not look it up.
    pub async fn lookup_by_email(&self, email: &str) -> Result<Option<(String, String)>> {
        match self.api_get("users.lookupByEmail", &[("email", email)]).await {
            Ok(v) => {
                let u = &v["user"];
                let name = u["real_name"].as_str().or(u["name"].as_str()).unwrap_or("?").to_string();
                Ok(u["id"].as_str().map(|id| (id.to_string(), name)))
            }
            Err(e) if e.to_string().contains("users_not_found") => Ok(None),
            Err(e) => Err(e),
        }
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

    /// `auth.test` for the bot token, plus the scopes the token was granted.
    pub async fn auth_test(&self) -> Result<(Value, Vec<String>)> {
        let resp = self
            .http
            .post("https://slack.com/api/auth.test")
            .bearer_auth(&self.bot_token)
            .send()
            .await?;
        let scopes = resp
            .headers()
            .get("x-oauth-scopes")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
            .unwrap_or_default();
        let v: Value = resp.json().await?;
        if v["ok"].as_bool() != Some(true) {
            bail!("Slack auth.test failed: {}", v["error"].as_str().unwrap_or("unknown error"));
        }
        Ok((v, scopes))
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
