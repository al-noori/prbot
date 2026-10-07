mod bot;
mod claude;
mod config;
mod github;
mod review;
mod slack;
mod state;

use anyhow::{Context, Result};
use bot::Bot;
use claude::Claude;
use config::{Config, SlackConfig};
use futures_util::{SinkExt, StreamExt};
use github::{GitHub, PrRef};
use serde_json::{json, Value};
use slack::Slack;
use state::Store;
use std::{sync::Arc, time::Duration};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> Result<()> {
    dotenvy::dotenv().ok();
    let cfg = Config::from_env()?;
    let http = reqwest::Client::builder()
        .user_agent("prbot/0.1")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(120))
        .build()?;
    let gh = GitHub::new(http.clone(), cfg.github_token.clone());
    let claude = Claude::new(http.clone(), cfg.anthropic_api_key.clone());

    // `prbot review <url>`: one review printed to the terminal, no Slack needed.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("review") {
        let pr = args.get(2).and_then(|a| PrRef::parse(a)).context("usage: prbot review <GitHub PR URL>")?;
        eprintln!("Reviewing {} with {} (effort {})…", pr.key(), claude::MODEL, claude::EFFORT);
        let r = review::run(&gh, &claude, &pr).await?;
        println!("Verdict: {}\n\n{}", r.verdict, r.body);
        for note in &r.notes {
            eprintln!("note: {note}");
        }
        eprintln!("{} · {} in / {} out tokens · ~${:.2}", r.model, r.input_tokens, r.output_tokens, r.approx_cost_usd());
        return Ok(());
    }

    let sc = SlackConfig::from_env()?;
    let slack = Slack::new(http.clone(), sc.bot_token, sc.app_token);
    let gh_login = gh.login().await.context("GitHub authentication failed")?;
    let dm = slack.open_dm(&sc.owner_id).await.context("could not open a DM with SLACK_OWNER_ID")?;
    let store = Store::load(cfg.state_path.clone())?;
    let bot = Bot::new(cfg, sc.owner_id, slack, gh, claude, store, gh_login, dm);

    bot.reschedule();
    bot.refresh_home().await;
    tokio::spawn(bot.clone().run_scheduler());
    println!("prbot running for GitHub @{}\n{}", bot.gh_login, bot.status_text());

    loop {
        match socket_session(&bot).await {
            Ok(()) => eprintln!("Socket Mode connection closed; reconnecting"),
            Err(e) => eprintln!("Socket Mode error: {e:#}; reconnecting in 5 s"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
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
                    Some("hello") => println!("connected to Slack"),
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
