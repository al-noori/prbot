//! Claude Messages API over raw HTTP (there is no official Rust SDK).

use anyhow::{bail, Result};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use std::time::Duration;

pub const MODEL: &str = "claude-opus-5-5";
pub const EFFORT: &str = "medium";
const MAX_TOKENS: u32 = 16_000;
const MAX_ATTEMPTS: u32 = 4;

pub struct Completion {
    pub text: String,
    /// The model that actually answered (differs from MODEL if a fallback served the request).
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub truncated: bool,
}

pub struct Claude {
    http: Client,
    api_key: String,
}

impl Claude {
    pub fn new(http: Client, api_key: String) -> Self {
        Self { http, api_key }
    }

    pub async fn complete(&self, system: &str, user: &str) -> Result<Completion> {
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
            let result = self
                .http
                .post("https://api.anthropic.com/v1/messages")
                .header("x-api-key", &self.api_key)
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
        Ok(Completion {
            text,
            model: v["model"].as_str().unwrap_or(MODEL).to_string(),
            input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
            truncated: stop == "max_tokens",
        })
    }
}
