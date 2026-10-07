mod bot;
mod claude;
mod config;
mod github;
mod review;
mod setup;
mod slack;
mod state;

use anyhow::{bail, Context, Result};
use bot::Bot;
use claude::Claude;
use config::{Config, SlackConfig};
use futures_util::{SinkExt, StreamExt};
use github::{GitHub, PrRef};
use serde_json::{json, Value};
use slack::Slack;
use state::Store;
use std::{io::Write, sync::Arc, time::Duration};
use tokio_tungstenite::tungstenite::Message;

const USAGE: &str = "prbot: Claude reviews the GitHub PRs that request your review, from Slack.

Usage:
  prbot                 run the Slack bot
  prbot setup           set up GitHub, Claude and the Slack app (writes .env)
  prbot doctor          check the configuration and connections
  prbot review <url>    review one PR and print it, without Slack
  prbot --version";

/// Bot output: to stderr and appended to prbot.log next to `.env` (useful when started at login without a window).
pub fn log(line: &str) {
    eprintln!("{line}");
    let path = config::home_dir().join("prbot.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {line}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    config::load_env();
    let http = reqwest::Client::builder()
        .user_agent(concat!("prbot/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()?;

    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        None => {}
        Some("setup") => return setup::setup(&http).await,
        Some("doctor") => return setup::doctor(&http).await,
        Some("review") => return review_once(&http, args.get(2)).await,
        Some("-V" | "--version" | "version") => {
            println!("prbot {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            return Ok(());
        }
        Some(other) => bail!("unknown command `{other}`\n\n{USAGE}"),
    }

    if !config::env_path().is_file() {
        bail!("no configuration found at {}. Run `prbot setup` first.", config::env_path().display());
    }
    let cfg = Config::from_env()?;
    let sc = SlackConfig::from_env()?;
    let gh = GitHub::new(http.clone(), cfg.github_token.clone());
    let claude = make_claude(&http, &cfg);
    let slack = Slack::new(http.clone(), sc.bot_token, sc.app_token);
    // Retried, because at login the network is often not up yet.
    let (gh_login, dm) = loop {
        let attempt = async {
            let login = gh.login().await.context("GitHub authentication failed")?;
            let dm = slack.open_dm(&sc.owner_id).await.context("could not open a DM with SLACK_OWNER_ID")?;
            anyhow::Ok((login, dm))
        };
        match attempt.await {
            Ok(v) => break v,
            Err(e) => log(&format!("startup failed: {e:#}; retrying in 30 s (`prbot doctor` checks the setup)")),
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    };
    let store = Store::load(cfg.state_path.clone())?;
    let bot = Bot::new(cfg, sc.owner_id, slack, gh, claude, store, gh_login, dm);

    bot.reschedule();
    bot.refresh_home().await;
    tokio::spawn(bot.clone().run_scheduler());
    log(&format!("prbot running for GitHub @{}\n{}", bot.gh_login, bot.status_text()));

    loop {
        match socket_session(&bot).await {
            Ok(()) => log("Socket Mode connection closed; reconnecting"),
            Err(e) => log(&format!("Socket Mode error: {e:#}; reconnecting in 5 s")),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn make_claude(http: &reqwest::Client, cfg: &Config) -> Claude {
    match &cfg.anthropic_api_key {
        Some(key) => Claude::api(http.clone(), key.clone()),
        None => Claude::cli(cfg.claude_bin.clone()),
    }
}

/// `prbot review <url>`: one review printed to the terminal, no Slack needed.
async fn review_once(http: &reqwest::Client, url: Option<&String>) -> Result<()> {
    let pr = url.and_then(|a| PrRef::parse(a)).context("usage: prbot review <GitHub PR URL>")?;
    let cfg = Config::from_env()?;
    let gh = GitHub::new(http.clone(), cfg.github_token.clone());
    let claude = make_claude(http, &cfg);
    eprintln!("Reviewing {} with {} (effort {}) via {}…", pr.key(), claude::MODEL, claude::EFFORT, claude.describe());
    let r = review::run(&gh, &claude, &pr).await?;
    println!("Verdict: {}\n\n{}", r.verdict, r.markdown());
    eprintln!("{} of {} finding(s) would be posted as inline comments.", r.inline_count(), r.findings.len());
    for note in &r.notes {
        eprintln!("note: {note}");
    }
    eprintln!("{} · {} in / {} out tokens · {}", r.model, r.input_tokens, r.output_tokens, r.cost_label());
    Ok(())
}

/// One Socket Mode WebSocket connection. Every envelope is acked right away and handled in the background.
async fn socket_session(bot: &Arc<Bot>) -> Result<()> {
    let url = bot.slack.socket_url().await?;
    let (ws, _) = tokio_tungstenite::connect_async(url.as_str()).await?;
    let (mut tx, mut rx) = ws.split();
    while let Some(msg) = rx.next().await {
        match msg? {
            Message::Text(text) => {
                let envelope: Value = serde_json::from_str(&text)?;
                match envelope["type"].as_str() {
                    Some("hello") => log("connected to Slack"),
                    Some("disconnect") => return Ok(()),
                    Some(kind @ ("slash_commands" | "events_api" | "interactive")) => {
                        if let Some(id) = envelope["envelope_id"].as_str() {
                            tx.send(Message::Text(json!({ "envelope_id": id }).to_string())).await?;
                        }
                        tokio::spawn(bot.clone().handle(kind.to_string(), envelope["payload"].clone()));
                    }
                    _ => {}
                }
            }
            Message::Ping(data) => tx.send(Message::Pong(data)).await?,
            Message::Close(_) => return Ok(()),
            _ => {}
        }
    }
    Ok(())
}
