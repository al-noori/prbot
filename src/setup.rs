//! `prbot setup`: interactive first-time configuration that writes `.env`.
//! `prbot doctor`: checks the configuration and every connection.

use crate::claude::Settings;
use crate::config::{self, var};
use crate::github::GitHub;
use crate::lifecycle;
use crate::slack::Slack;
use crate::state::{State, Store};
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

    heading("1/5 GitHub");
    ensure_installed("gh")?;
    let login = loop {
        match check_github(http).await {
            Ok(login) => break login,
            Err(e) if has_cli("gh") && var("GITHUB_TOKEN").is_err() => {
                println!("✗ {e:#}");
                if !confirm("Log in to GitHub now with `gh auth login`?", true) {
                    bail!("GitHub is not set up");
                }
                run_interactive("gh", &["auth", "login", "--web"])?;
            }
            Err(e) => return Err(e),
        }
    };
    println!("✓ GitHub: @{login}");

    heading("2/5 Claude");
    let mut claude_email = None;
    if let Ok(key) = var("ANTHROPIC_API_KEY") {
        println!("✓ Claude: {}", check_api_key(http, &key).await?);
    } else {
        let bin = config::claude_bin();
        if bin == "claude" {
            ensure_installed("claude")?;
        }
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
        claude_email = cli_email(&bin);
    }

    heading("3/5 Slack");
    let mut app_token = var("SLACK_APP_TOKEN").ok();
    let mut bot_token = var("SLACK_BOT_TOKEN").ok();
    // Named after the GitHub login so apps are easy to tell apart (Slack allows 35 characters).
    let mut app_name = Some(format!("PR Reviewer ({login})"))
        .filter(|n| n.chars().count() <= 35)
        .unwrap_or_else(|| "PR Reviewer".into());
    let mut fresh = false;
    let mut joined = false;
    if let (Some(app), Some(bot)) = (&app_token, &bot_token) {
        if let Ok(team) = check_slack(http, bot, app).await {
            println!("✓ Already connected to the Slack workspace {team}.");
            if confirm("Connect to a different Slack app?", false) {
                (app_token, bot_token, fresh) = (None, None, true);
            }
        }
    }
    let mut app_ok = match &app_token {
        Some(t) => Slack::new(http.clone(), String::new(), t.clone()).socket_url().await.is_ok(),
        None => false,
    };
    if !app_ok && bot_token.is_none() {
        println!("If your team already uses prbot, paste the team code a teammate sent you (they get it with `prbot invite`).");
        let code = secret("Team code, or press Enter to create your own Slack app instead")?;
        if !code.is_empty() {
            let (bot, app) = decode_team_code(&code)?;
            let team = check_slack(http, &bot, &app).await.context("the team code doesn't work (ask for a fresh one)")?;
            env.set("SLACK_BOT_TOKEN", &bot);
            env.set("SLACK_APP_TOKEN", &app);
            env.save()?;
            println!("✓ Joined your team's prbot app in the Slack workspace {team}.");
            (bot_token, app_token, fresh, app_ok, joined) = (Some(bot), Some(app), true, true, true);
        }
    }
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

    heading("4/5 You in Slack");
    let owner = match var("SLACK_OWNER_ID") {
        Ok(id) if !fresh && slack.open_dm(&id).await.is_ok() => id,
        _ => {
            let id = find_owner(&slack, claude_email.as_deref(), &app_name, joined).await?;
            env.set("SLACK_OWNER_ID", &id);
            env.save()?;
            id
        }
    };
    println!("✓ Slack member ID: {owner}");

    heading("5/5 Start");
    if cfg!(windows) {
        if confirm("Start prbot now and every time you log in?", true) {
            lifecycle::autostart(true)?;
            if lifecycle::is_running() {
                println!("Restarting the running prbot with the new settings…");
                lifecycle::stop().await?;
            }
            lifecycle::start_in_background()?;
            tokio::time::sleep(Duration::from_secs(5)).await;
            if lifecycle::is_running() {
                println!("✓ prbot is running in the background and starts when you log in (`prbot autostart off` undoes that).");
            } else {
                println!("✗ prbot didn't stay running; see {}", config::home_dir().join("prbot.log").display());
            }
        } else {
            println!("Start it yourself with `prbot`.");
        }
    } else {
        println!("Start it with `prbot`. To start it at login, see \"Run at login\" in {}.", env!("CARGO_PKG_REPOSITORY"));
    }

    println!("\nDone. In Slack, send /prreview help to the app. `prbot doctor` re-checks everything.");
    Ok(())
}

/// Finds the owner's Slack account: by email first (from Claude Code or git), else by asking.
async fn find_owner(slack: &Slack, claude_email: Option<&str>, app_name: &str, joined: bool) -> Result<String> {
    let git_email = Command::new("git")
        .args(["config", "--global", "user.email"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| s.contains('@') && !s.ends_with("users.noreply.github.com"));
    let mut emails: Vec<String> = claude_email.into_iter().map(String::from).chain(git_email).collect();
    emails.dedup();
    let mut can_lookup = true;
    for email in &emails {
        match slack.lookup_by_email(email).await {
            Ok(Some((id, name))) => {
                if confirm(&format!("Are you {name} ({email}) in Slack?"), true) {
                    return greet(slack, id).await;
                }
            }
            Ok(None) => {}
            Err(e) => {
                println!("(Can't find you by email: {e:#}. The Slack app needs the users:read.email scope for that.)");
                can_lookup = false;
                break;
            }
        }
    }
    if can_lookup {
        loop {
            let email = ask("Your Slack email address (Enter to skip)", "");
            if email.is_empty() {
                break;
            }
            match slack.lookup_by_email(&email).await? {
                Some((id, name)) => {
                    println!("Found {name}.");
                    return greet(slack, id).await;
                }
                None => println!("No Slack user has that email."),
            }
        }
    } else if !joined && !lifecycle::is_running() {
        // A new app of your own that can't look up emails: no other prbot is connected to it, so the
        // first DM to the bot comes to us and identifies you.
        println!("Open Slack, find \"{app_name}\" under Apps (or search for it) and send it any message.");
        println!("Waiting…");
        if let Ok(id) = tokio::time::timeout(OWNER_WAIT, wait_for_owner(slack)).await {
            return id;
        }
    }
    println!("In Slack, click your profile picture, then Profile, then the ⋮ button, then Copy member ID.");
    loop {
        let id = ask("Paste your member ID (U…)", "");
        if (id.starts_with('U') || id.starts_with('W')) && slack.open_dm(&id).await.is_ok() {
            return greet(slack, id).await;
        }
        println!("That isn't a member ID Slack knows.");
    }
}

async fn greet(slack: &Slack, id: String) -> Result<String> {
    let dm = slack.open_dm(&id).await?;
    slack.post(&dm, "prbot is set up for you. Try `/prreview help`.", None, None).await?;
    Ok(id)
}

/// On Windows, offers to install a missing `gh` or `claude`; elsewhere explains how.
fn ensure_installed(bin: &str) -> Result<()> {
    if has_cli(bin) || find_known_install(bin) {
        return Ok(());
    }
    let (name, url) = match bin {
        "gh" => ("GitHub CLI", "https://cli.github.com"),
        _ => ("Claude Code", "https://claude.com/claude-code"),
    };
    if !cfg!(windows) {
        bail!("{name} (`{bin}`) is not installed. Install it from {url}, then run `prbot setup` again.");
    }
    if !confirm(&format!("{name} is not installed. Install it now?"), true) {
        bail!("{name} is needed. Install it from {url}, then run `prbot setup` again.");
    }
    let status = match bin {
        "gh" => Command::new("winget")
            .args(["install", "--id", "GitHub.cli", "-e", "--source", "winget", "--accept-package-agreements", "--accept-source-agreements"])
            .status(),
        _ => Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", "irm https://claude.ai/install.ps1 | iex"])
            .status(),
    }
    .with_context(|| format!("could not run the {name} installer"))?;
    if !status.success() || !(has_cli(bin) || find_known_install(bin)) {
        bail!("installing {name} didn't work. Install it from {url}, then run `prbot setup` again.");
    }
    println!("✓ {name} installed.");
    Ok(())
}

/// Installers update PATH only for new terminals, so look where they put the program and add that
/// folder to this process's PATH (a prbot started from setup inherits it).
fn find_known_install(bin: &str) -> bool {
    let env_dir = |name: &str| std::env::var_os(name).map(PathBuf::from);
    let candidates: Vec<PathBuf> = match bin {
        "gh" => [env_dir("ProgramFiles"), env_dir("LOCALAPPDATA").map(|d| d.join("Programs"))]
            .into_iter()
            .flatten()
            .map(|d| d.join("GitHub CLI"))
            .collect(),
        _ => env_dir("USERPROFILE").map(|d| d.join(".local").join("bin")).into_iter().collect(),
    };
    for dir in candidates {
        if dir.join(format!("{bin}.exe")).is_file() {
            let path = std::env::var_os("PATH").unwrap_or_default();
            let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
            dirs.push(dir);
            if let Ok(joined) = std::env::join_paths(dirs) {
                std::env::set_var("PATH", joined);
            }
            return has_cli(bin);
        }
    }
    false
}

/// `prbot invite`: a message with the team code, for a teammate.
pub fn invite() -> Result<()> {
    let bot = var("SLACK_BOT_TOKEN")?;
    let app = var("SLACK_APP_TOKEN")?;
    let message = format!(
        "Set up prbot (Claude reviews the PRs that request your review) in about a minute:\r\n\
         1. In PowerShell, run:  irm https://raw.githubusercontent.com/al-noori/prbot/main/install.ps1 | iex\r\n   \
         (macOS or Linux: see {})\r\n\
         2. When it asks for a team code, paste:  {}\r\n",
        env!("CARGO_PKG_REPOSITORY"),
        encode_team_code(&bot, &app)
    );
    println!("Send this to your teammate privately. The team code contains the Slack app's tokens: anyone who has it can read the bot's DMs and post as the bot.\n");
    println!("{message}");
    if cfg!(windows) {
        let copied = Command::new("clip")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                child.stdin.take().expect("piped").write_all(message.as_bytes())?;
                child.wait()
            })
            .is_ok_and(|s| s.success());
        if copied {
            println!("(Copied to the clipboard.)");
        }
    }
    Ok(())
}

const TEAM_CODE_PREFIX: &str = "prbot1-";

fn encode_team_code(bot: &str, app: &str) -> String {
    let json = json!({ "b": bot, "a": app }).to_string();
    let hex: String = json.bytes().map(|b| format!("{b:02x}")).collect();
    format!("{TEAM_CODE_PREFIX}{hex}")
}

fn decode_team_code(code: &str) -> Result<(String, String)> {
    let damaged = "the team code is damaged (copy it again)";
    let hex: String = code
        .trim()
        .strip_prefix(TEAM_CODE_PREFIX)
        .context("that isn't a prbot team code")?
        .split_whitespace()
        .collect();
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|i| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
        .collect::<Option<Vec<u8>>>()
        .context(damaged)?;
    let v: Value = serde_json::from_slice(&bytes).context(damaged)?;
    match (v["b"].as_str(), v["a"].as_str()) {
        (Some(b), Some(a)) => Ok((b.to_string(), a.to_string())),
        _ => bail!(damaged),
    }
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
        Err(_) => check_claude_cli(&config::claude_bin()).map(|who| format!("Claude Code, {who}; reviews use {}", configured_settings().describe())),
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

/// The email of the Claude Code login, used to find the owner in Slack.
fn cli_email(bin: &str) -> Option<String> {
    let out = Command::new(bin).args(["auth", "status", "--json"]).output().ok()?;
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    v["email"].as_str().map(String::from)
}

/// The model and effort reviews use (Slack choice in the state file, else CLAUDE_MODEL / CLAUDE_EFFORT, else defaults).
fn configured_settings() -> Settings {
    Store::load(config::state_path()).map(|s| s.get().settings()).unwrap_or_else(|_| State::default().settings())
}

async fn check_api_key(http: &Client, key: &str) -> Result<String> {
    let settings = configured_settings();
    let resp = http
        .get(format!("https://api.anthropic.com/v1/models/{}", settings.model))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await?;
    match resp.status() {
        s if s.is_success() => Ok(format!("Anthropic API key works for {}", settings.describe())),
        StatusCode::UNAUTHORIZED => bail!("ANTHROPIC_API_KEY is invalid"),
        StatusCode::NOT_FOUND => bail!("ANTHROPIC_API_KEY has no access to {}", settings.model),
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
    let mut ok = format!("workspace {}, bot @{}", v["team"].as_str().unwrap_or("?"), v["user"].as_str().unwrap_or("?"));
    if !scopes.iter().any(|s| s == "users:read.email") {
        ok.push_str(". Tip: add the scopes users:read and users:read.email to the app and reinstall it, so teammates' setup finds them by email");
    }
    Ok(ok)
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
    fn team_code_round_trips() {
        let code = encode_team_code("xoxb-1-abc", "xapp-1-A1-def");
        assert_eq!(decode_team_code(&format!("  {code}
")).unwrap(), ("xoxb-1-abc".into(), "xapp-1-A1-def".into()));
        assert!(decode_team_code("prbot1-zz").is_err());
        assert!(decode_team_code("hello").is_err());
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
