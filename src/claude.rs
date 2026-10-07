//! Talks to Claude in one of two ways:
//! - the Messages API over raw HTTP (there is no official Rust SDK), when ANTHROPIC_API_KEY is set;
//! - the Claude Code CLI (`claude -p`), which uses your Claude login, when it isn't.

use anyhow::{bail, Context, Result};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub const MODEL: &str = "claude-opus-5-5";
pub const EFFORT: &str = "medium";
const MAX_TOKENS: u32 = 16_000;
const MAX_ATTEMPTS: u32 = 4;
const CLI_TIMEOUT: Duration = Duration::from_secs(1800);

pub struct Completion {
    pub text: String,
    /// The model that actually answered (differs from MODEL if a fallback served the request).
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub truncated: bool,
    /// Rough cost at Opus 5.5 list prices; `None` when the review ran on a Claude subscription.
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

    pub async fn complete(&self, system: &str, user: &str) -> Result<Completion> {
        match &self.backend {
            Backend::Api { http, api_key } => complete_api(http, api_key, system, user).await,
            Backend::Cli { bin } => complete_cli(bin, system, user).await,
        }
    }
}

async fn complete_api(http: &Client, api_key: &str, system: &str, user: &str) -> Result<Completion> {
    let body = json!({
        "model": MODEL,
        "max_tokens": MAX_TOKENS,
        "thinking": { "type": "adaptive" },
        "output_config": { "effort": EFFORT },
        // If a safety classifier declines, the API re-runs the request on a suitable fallback model.
        "fallbacks": "default",
        "system": system,
        "messages": [{ "role": "user", "content": user }],
    });

    let mut attempt = 0;
    let resp = loop {
        attempt += 1;
        let result = http
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", "server-side-fallback-2026-07-01")
            .timeout(Duration::from_secs(900))
            .json(&body)
            .send()
            .await;
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
            "Claude declined to review this PR (category: {})",
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
        model: v["model"].as_str().unwrap_or(MODEL).to_string(),
        input_tokens,
        output_tokens,
        truncated: stop == "max_tokens",
        cost_usd: Some(input_tokens as f64 * 4.0 / 1e6 + output_tokens as f64 * 20.0 / 1e6),
    })
}

/// Runs `claude -p` with no tools, no MCP servers and no saved session; the prompt goes in on stdin.
async fn complete_cli(bin: &str, system: &str, user: &str) -> Result<Completion> {
    let mut child = tokio::process::Command::new(bin)
        .args([
            "-p",
            "--model", MODEL,
            "--effort", EFFORT,
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
        bail!("Claude declined to review this PR");
    }
    let usage = &v["usage"];
    let n = |k: &str| usage[k].as_u64().unwrap_or(0);
    Ok(Completion {
        text: v["result"].as_str().unwrap_or_default().to_string(),
        model: v["modelUsage"]
            .as_object()
            .and_then(|m| m.keys().next().cloned())
            .unwrap_or_else(|| MODEL.to_string()),
        input_tokens: n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens"),
        output_tokens: n("output_tokens"),
        truncated: stop == "max_tokens",
        cost_usd: None,
    })
}
