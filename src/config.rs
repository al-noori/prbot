use anyhow::{bail, Context, Result};
use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

/// Settings needed for reviewing (CLI and bot).
pub struct Config {
    /// When unset, reviews run through the Claude Code CLI instead of the API.
    pub anthropic_api_key: Option<String>,
    pub claude_bin: String,
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

/// Directory holding `.env`, the state file and the log: `PRBOT_HOME`, else the current directory
/// if it has a `.env`, else a per-user config directory (`%APPDATA%\prbot` or `~/.config/prbot`).
pub fn home_dir() -> PathBuf {
    if let Some(dir) = env::var_os("PRBOT_HOME").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if Path::new(".env").is_file() {
        return PathBuf::from(".");
    }
    user_config_dir().join("prbot")
}

fn user_config_dir() -> PathBuf {
    let from = |name: &str| env::var_os(name).filter(|d| !d.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        if let Some(dir) = from("APPDATA") {
            return dir;
        }
    } else if let Some(dir) = from("XDG_CONFIG_HOME") {
        return dir;
    }
    from("HOME").or_else(|| from("USERPROFILE")).unwrap_or_else(|| PathBuf::from(".")).join(".config")
}

pub fn env_path() -> PathBuf {
    home_dir().join(".env")
}

/// Loads `.env` into the process environment. Variables that are already set win.
pub fn load_env() {
    dotenvy::from_path(env_path()).ok();
}

pub fn var(name: &str) -> Result<String> {
    env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .with_context(|| format!("{name} is not set in {} (run `prbot setup`)", env_path().display()))
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let github_token = match env::var("GITHUB_TOKEN") {
            Ok(t) if !t.trim().is_empty() => t,
            _ => gh_cli_token()?,
        };
        Ok(Self {
            anthropic_api_key: var("ANTHROPIC_API_KEY").ok(),
            claude_bin: claude_bin(),
            github_token,
            state_path: env::var("PRBOT_STATE")
                .map(PathBuf::from)
                .unwrap_or_else(|_| home_dir().join("prbot-state.json")),
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

pub fn claude_bin() -> String {
    var("CLAUDE_BIN").unwrap_or_else(|_| "claude".into())
}

/// Falls back to the token of the logged-in GitHub CLI.
pub fn gh_cli_token() -> Result<String> {
    let out = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .context("GITHUB_TOKEN is not set and the GitHub CLI (`gh`) was not found. Install it from https://cli.github.com")?;
    if !out.status.success() {
        bail!(
            "GITHUB_TOKEN is not set and `gh auth token` failed (run `gh auth login`): {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_string())
}
