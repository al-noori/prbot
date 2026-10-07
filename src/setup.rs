//! `prbot setup`: interactive first-time configuration that writes `.env`.
//! `prbot doctor`: checks the configuration and every connection.

use crate::config::{self, var};
use crate::github::GitHub;
use crate::slack::Slack;
use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, StatusCode, Url};
use serde_json::{json, Value};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const MANIFEST: &str = include_str!("../slack-app-manifest.json");
const ENV_TEMPLATE: &str = include_str!("../.env.example");
const REQUIRED_SCOPES: [&str; 4] = ["chat:write", "commands", "im:history", "im:write"];
const OWNER_WAIT: Duration = Duration::from_secs(600);

pub async fn setup(http: &Client) -> Result<()> {
    let path = config::env_path();
    let mut env = EnvFile::load(&path)?;
    println!("prbot setup. Settings are saved to {}", path.display());

    heading("1/4 GitHub");
    let login = loop {
        match check_github(http).await {
            Ok(login) => break login,
            Err(e) if has_cli("gh") && var("GITHUB_TOKEN").is_err() => {
                println!("✗ {e:#}");
                if !confirm("Log in to GitHub now with `gh auth login`?", true) {
                    bail!("GitHub is not set up");
                }
                run_interactive("gh", &["auth", "login"])?;
            }
            Err(e) => return Err(e),
        }
    };
    println!("✓ GitHub: @{login}");

    heading("2/4 Claude");
    if let Ok(key) = var("ANTHROPIC_API_KEY") {
        println!("✓ Claude: {}", check_api_key(http, &key).await?);
    } else {
        let bin = config::claude_bin();
        let who = loop {
            match check_claude_cli(&bin) {
                Ok(who) => break who,
                Err(e) if has_cli(&bin) => {
                    println!("✗ {e:#}");
                    if !confirm("Log in to Claude now with `claude auth login`?", true) {
                        bail!("Claude Code is not logged in");
                    }
                    run_interactive(&bin, &["auth", "login"])?;
                }
                Err(e) => return Err(e),
            }
        };
        println!("✓ Claude Code: {who}");
    }

    heading("3/4 Slack app");
    let mut app_token = var("SLACK_APP_TOKEN").ok();
    let mut bot_token = var("SLACK_BOT_TOKEN").ok();
    let mut app_name = "PR Reviewer".to_string();
    let mut fresh = false;
    if let (Some(app), Some(bot)) = (&app_token, &bot_token) {
        if let Ok(team) = check_slack(http, bot, app).await {
            println!("✓ Already connected to the Slack workspace {team}.");
            if confirm("Create a new Slack app anyway?", false) {
                (app_token, bot_token, fresh) = (None, None, true);
            }
        }
    }
    let app_ok = match &app_token {
        Some(t) => Slack::new(http.clone(), String::new(), t.clone()).socket_url().await.is_ok(),
        None => false,
    };
    if !app_ok {
        fresh = true;
        app_name = ask("Name for your Slack app", &app_name);
        let url = manifest_url(&app_name)?;
        println!("\nOpening Slack to create the app from prbot's manifest. If no browser opens, use this link:\n{url}\n");
        open_browser(url.as_str());
        println!("In the browser:");
        println!("  1. Pick your workspace, click Next, then Create. (If Slack makes you sign in first, open the link again afterwards.)");
        println!("  2. On the Basic Information page, scroll to App-Level Tokens and click Generate Token and Scopes.");
        println!("     Enter any name (e.g. socket), add the scope connections:write, click Generate and copy the token.");
        let token = loop {
            let t = secret("Paste the app-level token (xapp-…), or press Enter to stop for now")?;
            if t.is_empty() {
                println!("Stopped. Run `prbot setup` again to continue.");
                return Ok(());
            }
            if !t.starts_with("xapp-") {
                println!("That is not an app-level token; it should start with xapp-.");
                continue;
            }
            match Slack::new(http.clone(), String::new(), t.clone()).socket_url().await {
                Ok(_) => break t,
                Err(e) => println!("✗ Slack rejected it: {e:#}. Check that it has the connections:write scope."),
            }
        };
        env.set("SLACK_APP_TOKEN", &token);
        env.save()?;
        println!("✓ App-level token saved.");
        app_token = Some(token);
    }
    let app_token = app_token.expect("set above");

    let bot_ok = match &bot_token {
        Some(t) => check_slack(http, t, &app_token).await.is_ok(),
        None => false,
    };
    if !bot_ok {
        println!("\nNow install the app: in the left sidebar choose Install App, then Install to <workspace>, then Allow.");
        println!("If your workspace needs an admin's approval, click Request to Install. Your progress is saved;");
        println!("run `prbot setup` again once an admin has approved the app.");
        println!("After installing, copy the Bot User OAuth Token from the same page.");
        let token = loop {
            let t = secret("Paste the bot token (xoxb-…), or press Enter to stop for now")?;
            if t.is_empty() {
                println!("Stopped. Run `prbot setup` again to continue where you left off.");
                return Ok(());
            }
            if !t.starts_with("xoxb-") {
                println!("That is not a bot token; it should start with xoxb-.");
                continue;
            }
            match check_slack(http, &t, &app_token).await {
                Ok(team) => {
                    println!("✓ Connected to the Slack workspace {team}.");
                    break t;
                }
                Err(e) => println!("✗ {e:#}"),
            }
        };
        env.set("SLACK_BOT_TOKEN", &token);
        env.save()?;
        bot_token = Some(token);
    }
    let slack = Slack::new(http.clone(), bot_token.expect("set above"), app_token);

    heading("4/4 You in Slack");
    let owner = match var("SLACK_OWNER_ID") {
        Ok(id) if !fresh && slack.open_dm(&id).await.is_ok() => id,
        _ => {
            println!("prbot only listens to you. To find out who you are in Slack:");
            println!("open Slack, find \"{app_name}\" under Apps in the sidebar (or search for it) and send it any message,");
            println!("or run /prreview in any channel. Waiting…");
            let id = tokio::time::timeout(OWNER_WAIT, wait_for_owner(&slack))
                .await
                .context("no message arrived within 10 minutes; run `prbot setup` again")??;
            env.set("SLACK_OWNER_ID", &id);
            env.save()?;
            id
        }
    };
    println!("✓ Slack member ID: {owner}");

    println!("\nDone. Settings are in {}.", path.display());
    println!("Start the bot with `prbot`, then send /prreview help in Slack. `prbot doctor` re-checks everything.");
    Ok(())
}

pub async fn doctor(http: &Client) -> Result<()> {
    let path = config::env_path();
    let mut ok = true;
    ok &= show(
        "Config",
        if path.is_file() {
            Ok(path.display().to_string())
        } else {
            Err(anyhow::anyhow!("{} not found (run `prbot setup`)", path.display()))
        },
    );
    ok &= show("GitHub", check_github(http).await.map(|l| format!("@{l}")));
    let claude = match var("ANTHROPIC_API_KEY") {
        Ok(key) => check_api_key(http, &key).await,
        Err(_) => check_claude_cli(&config::claude_bin()).map(|who| format!("Claude Code, {who}")),
    };
    ok &= show("Claude", claude);

    let bot_token = var("SLACK_BOT_TOKEN");
    let app_token = var("SLACK_APP_TOKEN");
    let slack = Slack::new(
        http.clone(),
        bot_token.as_deref().unwrap_or_default().to_string(),
        app_token.as_deref().unwrap_or_default().to_string(),
    );
    let bot = match bot_token {
        Ok(_) => check_bot_token(&slack).await,
        Err(e) => Err(e),
    };
    ok &= show("Slack bot token", bot);
    let app = match app_token {
        Ok(_) => slack.socket_url().await.map(|_| "Socket Mode connection allowed".to_string()),
        Err(e) => Err(e),
    };
    ok &= show("Slack app token", app);
    let owner = match var("SLACK_OWNER_ID") {
        Ok(id) => slack.open_dm(&id).await.map(|_| id).context("cannot open a DM with SLACK_OWNER_ID"),
        Err(e) => Err(e),
    };
    ok &= show("Slack owner", owner);

    if !ok {
        bail!("some checks failed");
    }
    println!("\nAll good. Start the bot with `prbot`.");
    Ok(())
}

// ---------- checks ----------

async fn check_github(http: &Client) -> Result<String> {
    let token = match var("GITHUB_TOKEN") {
        Ok(t) => t,
        Err(_) => config::gh_cli_token()?,
    };
    GitHub::new(http.clone(), token).login().await.context("GitHub rejected the token")
}

fn check_claude_cli(bin: &str) -> Result<String> {
    let out = Command::new(bin).args(["auth", "status", "--json"]).output().with_context(|| {
        format!("Claude Code (`{bin}`) was not found. Install it from https://claude.com/claude-code, or set CLAUDE_BIN or ANTHROPIC_API_KEY")
    })?;
    let v: Value = serde_json::from_slice(&out.stdout).context("unexpected output from `claude auth status`")?;
    if v["loggedIn"].as_bool() != Some(true) {
        bail!("Claude Code is not logged in (run `claude auth login`)");
    }
    let mut who = v["email"].as_str().unwrap_or("logged in").to_string();
    if let Some(plan) = v["subscriptionType"].as_str() {
        who.push_str(&format!(" ({plan} plan)"));
    }
    Ok(who)
}

async fn check_api_key(http: &Client, key: &str) -> Result<String> {
    let resp = http
        .get(format!("https://api.anthropic.com/v1/models/{}", crate::claude::MODEL))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await?;
    match resp.status() {
        s if s.is_success() => Ok(format!("Anthropic API key works for {}", crate::claude::MODEL)),
        StatusCode::UNAUTHORIZED => bail!("ANTHROPIC_API_KEY is invalid"),
        StatusCode::NOT_FOUND => bail!("ANTHROPIC_API_KEY has no access to {}", crate::claude::MODEL),
        s => bail!("Anthropic API returned {s}"),
    }
}

async fn check_bot_token(slack: &Slack) -> Result<String> {
    let (v, scopes) = slack.auth_test().await?;
    let missing: Vec<&str> = REQUIRED_SCOPES.into_iter().filter(|s| !scopes.iter().any(|g| g == s)).collect();
    if !missing.is_empty() {
        bail!(
            "the bot token lacks the scopes {}; update the app from slack-app-manifest.json and reinstall it",
            missing.join(", ")
        );
    }
    Ok(format!("workspace {}, bot @{}", v["team"].as_str().unwrap_or("?"), v["user"].as_str().unwrap_or("?")))
}

/// Checks both Slack tokens and returns the workspace name.
async fn check_slack(http: &Client, bot_token: &str, app_token: &str) -> Result<String> {
    let slack = Slack::new(http.clone(), bot_token.to_string(), app_token.to_string());
    check_bot_token(&slack).await?;
    slack.socket_url().await?;
    let (v, _) = slack.auth_test().await?;
    Ok(v["team"].as_str().unwrap_or("?").to_string())
}

/// Connects over Socket Mode and returns the Slack user who first DMs the bot or runs /prreview.
async fn wait_for_owner(slack: &Slack) -> Result<String> {
    let url = slack.socket_url().await?;
    let (ws, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let (mut tx, mut rx) = ws.split();
    while let Some(msg) = rx.next().await {
        let text = match msg? {
            Message::Text(text) => text,
            Message::Ping(data) => {
                tx.send(Message::Pong(data)).await?;
                continue;
            }
            Message::Close(_) => break,
            _ => continue,
        };
        let envelope: Value = serde_json::from_str(&text)?;
        if let Some(id) = envelope["envelope_id"].as_str() {
            tx.send(Message::Text(json!({ "envelope_id": id }).to_string())).await?;
        }
        let p = &envelope["payload"];
        let user = match envelope["type"].as_str() {
            Some("slash_commands") => p["user_id"].as_str(),
            Some("events_api") => {
                let e = &p["event"];
                let human = e["type"] == "message" && e["bot_id"].is_null() && e["subtype"].is_null();
                if human { e["user"].as_str() } else { None }
            }
            _ => None,
        };
        if let Some(user) = user {
            let dm = slack.open_dm(user).await?;
            slack
                .post(&dm, "prbot is set up for you. Start it with `prbot`, then try `/prreview help`.", None, None)
                .await?;
            return Ok(user.to_string());
        }
    }
    bail!("the Slack connection closed; run `prbot setup` again")
}

// ---------- helpers ----------

fn show(name: &str, result: Result<String>) -> bool {
    match result {
        Ok(detail) => {
            println!("✓ {name}: {detail}");
            true
        }
        Err(e) => {
            println!("✗ {name}: {e:#}");
            false
        }
    }
}

fn manifest_url(app_name: &str) -> Result<Url> {
    let mut manifest: Value = serde_json::from_str(MANIFEST)?;
    manifest["display_information"]["name"] = json!(app_name);
    manifest["features"]["bot_user"]["display_name"] = json!(app_name);
    Ok(Url::parse_with_params(
        "https://api.slack.com/apps?new_app=1",
        &[("manifest_json", manifest.to_string())],
    )?)
}

fn open_browser(url: &str) {
    // Piped input means nobody is at the keyboard to use the browser.
    if !io::stdin().is_terminal() {
        return;
    }
    let result = if cfg!(windows) {
        Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).status()
    } else if cfg!(target_os = "macos") {
        Command::new("open").arg(url).status()
    } else {
        Command::new("xdg-open").arg(url).status()
    };
    if !result.is_ok_and(|s| s.success()) {
        println!("(Could not open a browser; open the link above yourself.)");
    }
}

fn has_cli(bin: &str) -> bool {
    Command::new(bin).arg("--version").output().is_ok()
}

fn run_interactive(bin: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(bin).args(args).status().with_context(|| format!("could not run `{bin}`"))?;
    if !status.success() {
        bail!("`{bin} {}` failed", args.join(" "));
    }
    Ok(())
}

fn heading(title: &str) {
    println!("\n== {title} ==");
}

fn ask(prompt: &str, default: &str) -> String {
    print!("{prompt} [{default}]: ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line).ok();
    match line.trim() {
        "" => default.to_string(),
        s => s.to_string(),
    }
}

fn confirm(prompt: &str, default_yes: bool) -> bool {
    print!("{prompt} [{}]: ", if default_yes { "Y/n" } else { "y/N" });
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line).ok();
    match line.trim().to_lowercase().as_str() {
        "" => default_yes,
        a => a.starts_with('y'),
    }
}

/// Reads a token without echoing it (reads a plain line when input is piped).
fn secret(prompt: &str) -> Result<String> {
    let hidden = io::stdin()
        .is_terminal()
        .then(|| rpassword::prompt_password(format!("{prompt}: ")).ok())
        .flatten();
    let value = match hidden {
        Some(v) => v,
        None => {
            print!("{prompt}: ");
            io::stdout().flush()?;
            let mut line = String::new();
            io::stdin().read_line(&mut line)?;
            line
        }
    };
    Ok(value.trim().to_string())
}

/// `.env` edited in place: known keys are replaced, comments and other lines are kept.
struct EnvFile {
    path: PathBuf,
    lines: Vec<String>,
}

impl EnvFile {
    fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => ENV_TEMPLATE.to_string(),
            Err(e) => return Err(e).with_context(|| format!("could not read {}", path.display())),
        };
        Ok(Self { path: path.to_path_buf(), lines: text.lines().map(String::from).collect() })
    }

    fn set(&mut self, key: &str, value: &str) {
        let prefix = format!("{key}=");
        let line = format!("{key}={value}");
        match self.lines.iter_mut().find(|l| l.trim_start().trim_start_matches(['#', ' ']).starts_with(&prefix)) {
            Some(l) => *l = line,
            None => self.lines.push(line),
        }
        std::env::set_var(key, value);
    }

    fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&self.path, self.lines.join("\n") + "\n")
            .with_context(|| format!("could not write {}", self.path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_url_carries_the_app_name() {
        let url = manifest_url("Ada's Reviewer").unwrap();
        let (_, json) = url.query_pairs().find(|(k, _)| k == "manifest_json").unwrap();
        let m: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(m["display_information"]["name"], "Ada's Reviewer");
        assert_eq!(m["features"]["bot_user"]["display_name"], "Ada's Reviewer");
        assert_eq!(m["settings"]["socket_mode_enabled"], true);
    }

    #[test]
    fn env_file_replaces_keys_and_keeps_comments() {
        let mut f = EnvFile {
            path: PathBuf::new(),
            lines: vec!["# comment".into(), "SLACK_BOT_TOKEN=".into(), "# PRBOT_STATE=x.json".into()],
        };
        f.set("SLACK_BOT_TOKEN", "xoxb-1");
        f.set("PRBOT_STATE", "y.json");
        f.set("SLACK_OWNER_ID", "U1");
        assert_eq!(f.lines, ["# comment", "SLACK_BOT_TOKEN=xoxb-1", "PRBOT_STATE=y.json", "SLACK_OWNER_ID=U1"]);
    }
}
