//! Talks to Claude in one of two ways:
//! - the Messages API over raw HTTP (there is no official Rust SDK), when ANTHROPIC_API_KEY is set;
//! - the Claude Code CLI (`claude -p`), which uses your Claude login, when it isn't.

use anyhow::{bail, Context, Result};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
pub const DEFAULT_EFFORT: &str = "medium";
pub const EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
/// (shorthand, model ID, label): the models offered by `/prreview model` and on the Home tab.
pub const MODELS: [(&str, &str, &str); 4] = [
    ("opus", "claude-opus-5-5", "Opus 5.5"),
    ("sonnet", "claude-sonnet-5-5", "Sonnet 5.5"),
    ("haiku", "claude-haiku-4-5", "Haiku 4.5"),
    ("fable", "claude-fable-5-1", "Fable 5.1"),
];
const MAX_TOKENS: u32 = 16_000;
const MAX_ATTEMPTS: u32 = 4;
const CLI_TIMEOUT: Duration = Duration::from_secs(1800);

/// Which model reviews, and how hard it thinks.
#[derive(Debug, Clone)]
pub struct Settings {
    pub model: String,
    pub effort: String,
}

impl Settings {
    /// Haiku 4.5 has no effort setting (and no adaptive thinking).
    pub fn uses_effort(&self) -> bool {
        !self.model.starts_with("claude-haiku")
    }

    pub fn describe(&self) -> String {
        if self.uses_effort() {
            format!("{}, effort {}", model_label(&self.model), self.effort)
        } else {
            model_label(&self.model).to_string()
        }
    }
}

/// "opus", "Sonnet", or a full `claude-…` model ID.
pub fn resolve_model(s: &str) -> Option<String> {
    let s = s.trim().to_lowercase();
    if let Some((_, id, _)) = MODELS.iter().find(|(short, id, _)| *short == s || *id == s) {
        return Some(id.to_string());
    }
    let valid = s.starts_with("claude-") && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.');
    valid.then_some(s)
}

pub fn model_label(id: &str) -> &str {
    MODELS.iter().find(|(_, m, _)| *m == id).map(|(_, _, label)| *label).unwrap_or(id)
}

/// List prices in USD per million (input, output) tokens.
fn price(model: &str) -> Option<(f64, f64)> {
    Some(match model {
        m if m.starts_with("claude-fable") || m.starts_with("claude-mythos") => (10.0, 50.0),
        m if m.starts_with("claude-opus-5-5") => (4.0, 20.0),
        m if m.starts_with("claude-opus") => (5.0, 25.0),
        m if m.starts_with("claude-sonnet-4") => (3.0, 15.0),
        m if m.starts_with("claude-sonnet") => (2.0, 10.0),
        m if m.starts_with("claude-haiku") => (1.0, 5.0),
        _ => return None,
    })
}

pub struct Completion {
    pub text: String,
    /// The model that actually answered (differs from the requested one if a fallback served the request).
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub truncated: bool,
    /// Rough cost at list prices; `None` when the review ran on a Claude subscription.
    pub cost_usd: Option<f64>,
}

enum Backend {
    Api { http: Client, api_key: String },
    Cli { bin: String },
}

pub struct Claude {
    backend: Backend,
}

impl Claude {
    pub fn api(http: Client, api_key: String) -> Self {
        Self { backend: Backend::Api { http, api_key } }
    }

    pub fn cli(bin: String) -> Self {
        Self { backend: Backend::Cli { bin } }
    }

    pub fn describe(&self) -> &'static str {
        match self.backend {
            Backend::Api { .. } => "Anthropic API",
            Backend::Cli { .. } => "Claude Code (your Claude login)",
        }
    }

    pub async fn complete(&self, settings: &Settings, system: &str, user: &str) -> Result<Completion> {
        match &self.backend {
            Backend::Api { http, api_key } => complete_api(http, api_key, settings, system, user).await,
            Backend::Cli { bin } => complete_cli(bin, settings, system, user).await,
        }
    }
}

async fn complete_api(http: &Client, api_key: &str, settings: &Settings, system: &str, user: &str) -> Result<Completion> {
    let mut body = json!({
        "model": settings.model,
        "max_tokens": MAX_TOKENS,
        "system": system,
        "messages": [{ "role": "user", "content": user }],
    });
    if settings.uses_effort() {
        body["thinking"] = json!({ "type": "adaptive" });
        body["output_config"] = json!({ "effort": settings.effort });
        // If a safety classifier declines, the API re-runs the request on a suitable fallback model.
        body["fallbacks"] = json!("default");
    }

    let mut attempt = 0;
    let resp = loop {
        attempt += 1;
        let mut req = http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01");
        if settings.uses_effort() {
            req = req.header("anthropic-beta", "server-side-fallback-2026-07-01");
        }
        let result = req.timeout(Duration::from_secs(900)).json(&body).send().await;
        match result {
            Ok(resp) if resp.status().is_success() => break resp,
            Ok(resp) => {
                let status = resp.status();
                let retryable = status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                let text = resp.text().await.unwrap_or_default();
                if !retryable || attempt >= MAX_ATTEMPTS {
                    bail!("Claude API {status}: {text}");
                }
            }
            Err(e) if e.is_connect() && attempt < MAX_ATTEMPTS => {}
            Err(e) => return Err(e.into()),
        }
        tokio::time::sleep(Duration::from_secs(5 * 2u64.pow(attempt))).await;
    };

    let v: Value = resp.json().await?;
    let stop = v["stop_reason"].as_str().unwrap_or_default();
    if stop == "refusal" {
        bail!(
            "Claude declined the request (category: {})",
            v["stop_details"]["category"].as_str().unwrap_or("none")
        );
    }
    let text = v["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect::<Vec<_>>()
        .join("");
    let input_tokens = v["usage"]["input_tokens"].as_u64().unwrap_or(0);
    let output_tokens = v["usage"]["output_tokens"].as_u64().unwrap_or(0);
    Ok(Completion {
        text,
        model: v["model"].as_str().unwrap_or(&settings.model).to_string(),
        input_tokens,
        output_tokens,
        truncated: stop == "max_tokens",
        cost_usd: price(v["model"].as_str().unwrap_or(&settings.model)).map(|(i, o)| (input_tokens as f64 * i + output_tokens as f64 * o) / 1e6),
    })
}

/// Runs `claude -p` with no tools, no MCP servers and no saved session; the prompt goes in on stdin.
async fn complete_cli(bin: &str, settings: &Settings, system: &str, user: &str) -> Result<Completion> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(["-p", "--model", &settings.model]);
    if settings.uses_effort() {
        cmd.args(["--effort", &settings.effort]);
    }
    let mut child = cmd
        .args([
            "--output-format", "json",
            "--tools", "",
            "--strict-mcp-config",
            "--no-session-persistence",
            "--system-prompt", system,
        ])
        // A neutral directory, so no project CLAUDE.md is picked up.
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("could not start `{bin}` (is Claude Code installed and logged in?)"))?;

    let mut stdin = child.stdin.take().context("no stdin for Claude Code")?;
    let input = user.to_string();
    let writer = tokio::spawn(async move {
        stdin.write_all(input.as_bytes()).await?;
        stdin.shutdown().await
    });
    let out = tokio::time::timeout(CLI_TIMEOUT, child.wait_with_output())
        .await
        .context("Claude Code took longer than 30 minutes")??;
    writer.await??;

    let v: Value = serde_json::from_slice(&out.stdout).with_context(|| {
        format!(
            "unexpected output from Claude Code (exit {}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).chars().take(500).collect::<String>()
        )
    })?;
    if v["is_error"].as_bool() == Some(true) || v["subtype"] != "success" {
        bail!("Claude Code failed: {}", v["result"].as_str().unwrap_or("no details"));
    }
    let stop = v["stop_reason"].as_str().unwrap_or_default();
    if stop == "refusal" {
        bail!("Claude declined the request");
    }
    let usage = &v["usage"];
    let n = |k: &str| usage[k].as_u64().unwrap_or(0);
    Ok(Completion {
        text: v["result"].as_str().unwrap_or_default().to_string(),
        model: v["modelUsage"]
            .as_object()
            .and_then(|m| m.keys().next().cloned())
            .unwrap_or_else(|| settings.model.clone()),
        input_tokens: n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens"),
        output_tokens: n("output_tokens"),
        truncated: stop == "max_tokens",
        cost_usd: None,
    })
}
