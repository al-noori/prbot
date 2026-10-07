use anyhow::{bail, Context, Result};
use std::{env, path::PathBuf, process::Command};

/// Settings needed for reviewing (CLI and bot).
pub struct Config {
    pub anthropic_api_key: String,
    pub github_token: String,
    pub state_path: PathBuf,
    pub max_reviews_per_run: usize,
    pub skip_drafts: bool,
}

/// Settings only the Slack bot needs.
pub struct SlackConfig {
    pub bot_token: String,
    pub app_token: String,
    pub owner_id: String,
}

fn var(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .with_context(|| format!("missing env var {name} (see .env.example)"))
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let github_token = match env::var("GITHUB_TOKEN") {
            Ok(t) if !t.trim().is_empty() => t,
            _ => gh_cli_token()?,
        };
        Ok(Self {
            anthropic_api_key: var("ANTHROPIC_API_KEY")?,
            github_token,
            state_path: env::var("PRBOT_STATE")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("prbot-state.json")),
            max_reviews_per_run: env::var("MAX_REVIEWS_PER_RUN")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5),
            skip_drafts: env::var("SKIP_DRAFTS").map(|v| v != "false").unwrap_or(true),
        })
    }
}

impl SlackConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            bot_token: var("SLACK_BOT_TOKEN")?,
            app_token: var("SLACK_APP_TOKEN")?,
            owner_id: var("SLACK_OWNER_ID")?,
        })
    }
}

/// Falls back to the token of the logged-in GitHub CLI.
fn gh_cli_token() -> Result<String> {
    let out = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .context("GITHUB_TOKEN is not set and the `gh` CLI was not found")?;
    if !out.status.success() {
        bail!(
            "GITHUB_TOKEN is not set and `gh auth token` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}
