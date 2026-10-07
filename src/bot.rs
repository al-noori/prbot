//! Slack-facing behaviour: commands, the App Home tab, buttons, the scheduler, and review posting.

use crate::claude::{self, Claude};
use crate::config::Config;
use crate::github::{GitHub, PrRef};
use crate::review::{self, Review};
use crate::slack::{self, Slack};
use crate::state::{is_weekend, Schedule, Store, StoredReview};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveTime, Utc};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::sync::Semaphore;

const MARKDOWN_CHUNK: usize = 11_000;
const PARALLEL_REVIEWS: usize = 2;

const HELP: &str = "*PR Reviewer*: Claude (Opus 5.5, effort medium) reviews PRs that request your review and posts the result here. It never approves or merges.
`/prreview` or `/prreview status`: show status
`/prreview on` / `/prreview off`: turn scheduled checks on or off (manual reviews always work)
`/prreview now`: check for review requests right now
`/prreview every 30m` / `/prreview every 2h`: check on an interval
`/prreview at 09:00,14:00`: check at fixed times
`/prreview weekdays on|off`: skip weekends or not
`/prreview scope me|team`: only PRs requesting you directly, or also through your teams
`/prreview <PR URL>`: review one PR now (you can also just DM me the link)";

pub struct Bot {
    pub cfg: Config,
    pub owner: String,
    pub slack: Slack,
    pub gh: GitHub,
    pub claude: Claude,
    pub store: Store,
    pub gh_login: String,
    /// DM channel with the owner, where everything is posted.
    pub dm: String,
    next_run: Mutex<Option<DateTime<Local>>>,
    in_flight: Mutex<HashSet<String>>,
    workers: Semaphore,
}

impl Bot {
    #[allow(clippy::too_many_arguments)]
    pub fn new(cfg: Config, owner: String, slack: Slack, gh: GitHub, claude: Claude, store: Store, gh_login: String, dm: String) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            owner,
            slack,
            gh,
            claude,
            store,
            gh_login,
            dm,
            next_run: Mutex::new(None),
            in_flight: Mutex::new(HashSet::new()),
            workers: Semaphore::new(PARALLEL_REVIEWS),
        })
    }

    // ---------- Socket Mode dispatch ----------

    pub async fn handle(self: Arc<Self>, kind: String, payload: Value) {
        let result = match kind.as_str() {
            "slash_commands" => self.on_slash(payload).await,
            "events_api" => self.on_event(payload).await,
            "interactive" => self.on_interactive(payload).await,
            _ => Ok(()),
        };
        if let Err(e) = result {
            eprintln!("error handling {kind}: {e:#}");
            let _ = self.slack.post(&self.dm, &format!(":warning: {e:#}"), None, None).await;
        }
    }

    async fn on_slash(self: &Arc<Self>, p: Value) -> Result<()> {
        let response_url = p["response_url"].as_str().unwrap_or_default().to_string();
        let reply = if p["user_id"] != self.owner.as_str() {
            "Sorry, this PR reviewer is private.".to_string()
        } else {
            self.command(p["text"].as_str().unwrap_or_default()).await?
        };
        self.slack.respond(&response_url, &reply).await
    }

    async fn on_event(self: &Arc<Self>, p: Value) -> Result<()> {
        let ev = &p["event"];
        let from_owner = ev["user"] == self.owner.as_str();
        match ev["type"].as_str().unwrap_or_default() {
            "app_home_opened" if from_owner && ev["tab"] == "home" => self.refresh_home().await,
            "message"
                if from_owner
                    && ev["channel_type"] == "im"
                    && ev.get("bot_id").is_none()
                    && ev.get("subtype").is_none() =>
            {
                let text = ev["text"].as_str().unwrap_or_default();
                match PrRef::parse(text) {
                    // The review posts its own "Reviewing…" message.
                    Some(pr) => {
                        if !self.spawn_review(pr.clone()) {
                            self.slack.post(&self.dm, &format!("{} is already being reviewed.", pr.key()), None, None).await?;
                        }
                    }
                    None => {
                        let reply = self.command(text).await?;
                        self.slack.post(&self.dm, &reply, None, None).await?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_interactive(self: &Arc<Self>, p: Value) -> Result<()> {
        if p["user"]["id"] != self.owner.as_str() || p["type"] != "block_actions" {
            return Ok(());
        }
        for a in p["actions"].as_array().cloned().unwrap_or_default() {
            match a["action_id"].as_str().unwrap_or_default() {
                "post_to_github" => self.post_to_github(&p, a["value"].as_str().unwrap_or_default()).await?,
                "rereview" => {
                    if let Some(pr) = a["value"].as_str().and_then(PrRef::parse) {
                        self.spawn_review(pr);
                    }
                }
                "toggle" => {
                    self.command(if a["value"] == "on" { "on" } else { "off" }).await?;
                }
                "check_now" => {
                    self.command("now").await?;
                }
                "set_schedule" => {
                    if let Some(v) = a["selected_option"]["value"].as_str() {
                        self.command(v).await?;
                    }
                }
                "manual_url" => match a["value"].as_str().and_then(PrRef::parse) {
                    Some(pr) => {
                        self.spawn_review(pr);
                    }
                    None => {
                        self.slack.post(&self.dm, "That doesn't look like a GitHub pull request URL.", None, None).await?;
                    }
                },
                _ => {}
            }
        }
        Ok(())
    }

    // ---------- Commands (slash command, DM text, and Home tab buttons all end up here) ----------

    pub async fn command(self: &Arc<Self>, text: &str) -> Result<String> {
        let text = text.trim();
        if let Some(pr) = PrRef::parse(text) {
            return Ok(if self.spawn_review(pr.clone()) {
                format!("Reviewing {}. I'll post the review in our DM.", pr.key())
            } else {
                format!("{} is already being reviewed.", pr.key())
            });
        }
        let mut parts = text.split_whitespace();
        let cmd = parts.next().unwrap_or("status").to_lowercase();
        let arg = parts.collect::<Vec<_>>().join(" ").to_lowercase();

        let reply = match cmd.as_str() {
            "status" => self.status_text(),
            "help" => HELP.to_string(),
            "on" | "off" => {
                let on = cmd == "on";
                self.store.update(|s| s.enabled = on)?;
                self.reschedule();
                format!("Scheduled checks are now *{cmd}*.\n{}", self.status_text())
            }
            "now" | "check" => {
                tokio::spawn(self.clone().check_requests(true));
                "Checking for PRs that request your review…".into()
            }
            "every" => match parse_interval(&arg) {
                Some(minutes) => {
                    let schedule = Schedule::Interval { minutes };
                    let desc = schedule.describe();
                    self.store.update(|s| s.schedule = schedule)?;
                    self.reschedule();
                    format!("OK, checking {desc}.\n{}", self.status_text())
                }
                None => "Use e.g. `every 30m`, `every 1h` or `every 2h` (5 min to 24 h).".into(),
            },
            "at" => match parse_times(&arg) {
                Some(times) => {
                    let schedule = Schedule::Times { times };
                    let desc = schedule.describe();
                    self.store.update(|s| s.schedule = schedule)?;
                    self.reschedule();
                    format!("OK, checking {desc}.\n{}", self.status_text())
                }
                None => "Use e.g. `at 09:00` or `at 09:00,13:30,16:00` (24-hour, local time).".into(),
            },
            "weekdays" => match arg.as_str() {
                "on" | "off" => {
                    let on = arg == "on";
                    self.store.update(|s| s.weekdays_only = on)?;
                    self.reschedule();
                    format!("Weekends are now {}.", if on { "skipped" } else { "included" })
                }
                _ => "Use `weekdays on` (skip weekends) or `weekdays off`.".into(),
            },
            "scope" => {
                let query = match arg.as_str() {
                    "me" => Some("user-review-requested:@me"),
                    "team" => Some("review-requested:@me"),
                    _ => None,
                };
                match query {
                    Some(q) => {
                        self.store.update(|s| s.query = q.to_string())?;
                        format!("Scope is now: {}.", scope_label(q))
                    }
                    None => "Use `scope me` (requested from you directly) or `scope team` (also through your teams).".into(),
                }
            }
            _ => format!("I don't know `{cmd}`.\n\n{HELP}"),
        };
        self.refresh_home().await;
        Ok(reply)
    }

    pub fn status_text(&self) -> String {
        let st = self.store.get();
        let next = *self.next_run.lock().unwrap();
        let in_flight = self.in_flight.lock().unwrap().len();
        format!(
            "*Scheduled checks:* {}\n*Schedule:* {}{}\n*Next check:* {}\n*Last check:* {}\n*Scope:* {}\n*Model:* `{}`, effort `{}`\n*Reviews running:* {}",
            if st.enabled { ":large_green_circle: on" } else { ":white_circle: off" },
            st.schedule.describe(),
            if st.weekdays_only { " (weekdays only)" } else { "" },
            next.map(fmt_time).unwrap_or_else(|| "none".into()),
            st.last_check.map(fmt_time).unwrap_or_else(|| "never".into()),
            scope_label(&st.query),
            claude::MODEL,
            claude::EFFORT,
            in_flight,
        )
    }

    // ---------- Scheduling ----------

    pub fn reschedule(&self) {
        let st = self.store.get();
        *self.next_run.lock().unwrap() =
            if st.enabled { st.schedule.next_after(Local::now(), st.weekdays_only) } else { None };
    }

    /// Ticks every 20 s. A check that was due while the machine slept runs once on wake-up.
    pub async fn run_scheduler(self: Arc<Self>) {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(20));
        loop {
            tick.tick().await;
            let st = self.store.get();
            let now = Local::now();
            let due = {
                let mut next = self.next_run.lock().unwrap();
                if !st.enabled {
                    *next = None;
                    false
                } else {
                    match *next {
                        None => {
                            *next = st.schedule.next_after(now, st.weekdays_only);
                            false
                        }
                        Some(t) if now >= t => {
                            *next = st.schedule.next_after(now, st.weekdays_only);
                            true
                        }
                        Some(_) => false,
                    }
                }
            };
            if due && !(st.weekdays_only && is_weekend(&now)) {
                tokio::spawn(self.clone().check_requests(false));
            }
        }
    }

    pub async fn check_requests(self: Arc<Self>, manual: bool) {
        if let Err(e) = self.try_check(manual).await {
            let _ = self
                .slack
                .post(&self.dm, &format!(":warning: Checking review requests failed: {e:#}"), None, None)
                .await;
        }
    }

    async fn try_check(self: &Arc<Self>, manual: bool) -> Result<()> {
        let st = self.store.get();
        let prs = self.gh.review_requests(&st.query).await?;
        let total = prs.len();
        let (mut queued, mut up_to_date, mut drafts, mut deferred) = (0, 0, 0, 0);
        for pr in prs {
            if queued >= self.cfg.max_reviews_per_run {
                deferred += 1;
                continue;
            }
            let info = self.gh.pr(&pr).await?;
            if info.draft && self.cfg.skip_drafts {
                drafts += 1;
            } else if st.reviewed.get(&pr.key()) == Some(&info.head_sha) {
                up_to_date += 1;
            } else if self.spawn_review(pr) {
                queued += 1;
            }
        }
        self.store.update(|s| s.last_check = Some(Local::now()))?;
        if manual || queued > 0 || deferred > 0 {
            let mut msg = format!(":mag: {total} open PR(s) request your review. Reviewing {queued} now");
            if up_to_date > 0 {
                msg += &format!(", {up_to_date} already reviewed at their latest commit");
            }
            if drafts > 0 {
                msg += &format!(", {drafts} draft(s) skipped");
            }
            if deferred > 0 {
                msg += &format!(", {deferred} left for the next check (limit {} per run)", self.cfg.max_reviews_per_run);
            }
            msg.push('.');
            self.slack.post(&self.dm, &msg, None, None).await?;
        }
        self.refresh_home().await;
        Ok(())
    }

    // ---------- Reviews ----------

    /// Starts a background review unless this PR is already being reviewed.
    pub fn spawn_review(self: &Arc<Self>, pr: PrRef) -> bool {
        if !self.in_flight.lock().unwrap().insert(pr.key()) {
            return false;
        }
        let bot = self.clone();
        tokio::spawn(async move {
            let _permit = bot.workers.acquire().await;
            if let Err(e) = bot.do_review(&pr).await {
                let _ = bot
                    .slack
                    .post(&bot.dm, &format!(":warning: Review of <{}|{}> failed: {e:#}", pr.url(), pr.key()), None, None)
                    .await;
            }
            bot.in_flight.lock().unwrap().remove(&pr.key());
            bot.refresh_home().await;
        });
        true
    }

    async fn do_review(&self, pr: &PrRef) -> Result<()> {
        let ts = self
            .slack
            .post(&self.dm, &format!(":hourglass_flowing_sand: Reviewing <{}|{}> with Claude…", pr.url(), pr.key()), None, None)
            .await?;
        let review = match review::run(&self.gh, &self.claude, pr).await {
            Ok(r) => r,
            Err(e) => {
                let msg = format!(":warning: Review of <{}|{}> failed: {e:#}", pr.url(), pr.key());
                self.slack.update(&self.dm, &ts, &msg, None).await?;
                return Ok(());
            }
        };

        let id = format!("r{}", Utc::now().timestamp_millis());
        self.store.update(|s| {
            s.reviews.insert(
                id.clone(),
                StoredReview {
                    pr_url: pr.url(),
                    head_sha: review.info.head_sha.clone(),
                    body: review.body.clone(),
                    created: Utc::now(),
                    posted_url: None,
                },
            );
            s.reviewed.insert(pr.key(), review.info.head_sha.clone());
        })?;

        let (text, blocks) = header_blocks(&review, &id);
        self.slack.update(&self.dm, &ts, &text, Some(blocks)).await?;
        let chunks = slack::chunk_markdown(&review.body, MARKDOWN_CHUNK);
        for (i, chunk) in chunks.iter().enumerate() {
            self.slack
                .post(
                    &self.dm,
                    &format!("Review part {}/{}", i + 1, chunks.len()),
                    Some(json!([{ "type": "markdown", "text": chunk }])),
                    Some(&ts),
                )
                .await?;
        }
        Ok(())
    }

    /// Only runs on an explicit, confirmed button click. Posts a COMMENT review, never an approval.
    async fn post_to_github(&self, p: &Value, id: &str) -> Result<()> {
        let channel = p["container"]["channel_id"].as_str().unwrap_or(&self.dm).to_string();
        let ts = p["container"]["message_ts"].as_str().unwrap_or_default().to_string();
        let Some(stored) = self.store.get().reviews.get(id).cloned() else {
            self.slack.post(&self.dm, "That review is too old to post. Use Re-review to make a fresh one.", None, None).await?;
            return Ok(());
        };
        let link = match &stored.posted_url {
            Some(url) => url.clone(),
            None => {
                let pr = PrRef::parse(&stored.pr_url).context("stored review has an invalid PR URL")?;
                let body = format!(
                    "{}\n\n---\n<sub>🤖 Review written by Claude ({}, effort {}) and posted by @{}. This is a comment, not an approval.</sub>",
                    stored.body, claude::MODEL, claude::EFFORT, self.gh_login
                );
                let url = self.gh.post_comment_review(&pr, &stored.head_sha, &body).await?;
                self.store.update(|s| {
                    if let Some(r) = s.reviews.get_mut(id) {
                        r.posted_url = Some(url.clone());
                    }
                })?;
                url
            }
        };
        let mut blocks = p["message"]["blocks"].as_array().cloned().unwrap_or_default();
        blocks.retain(|b| b["type"] != "actions");
        blocks.push(json!({
            "type": "context",
            "elements": [{ "type": "mrkdwn", "text": format!(":white_check_mark: Posted as a PR comment: <{link}|view on GitHub>") }]
        }));
        let text = p["message"]["text"].as_str().unwrap_or("PR review").to_string();
        self.slack.update(&channel, &ts, &text, Some(Value::Array(blocks))).await
    }

    // ---------- App Home tab ----------

    pub async fn refresh_home(&self) {
        let enabled = self.store.get().enabled;
        let toggle = if enabled {
            json!({ "type": "button", "action_id": "toggle", "value": "off", "style": "danger",
                    "text": { "type": "plain_text", "text": "Turn off" } })
        } else {
            json!({ "type": "button", "action_id": "toggle", "value": "on", "style": "primary",
                    "text": { "type": "plain_text", "text": "Turn on" } })
        };
        let opt = |label: &str, value: &str| json!({ "text": { "type": "plain_text", "text": label }, "value": value });
        let view = json!({
            "type": "home",
            "blocks": [
                { "type": "header", "text": { "type": "plain_text", "text": "PR Reviewer" } },
                { "type": "section", "text": { "type": "mrkdwn", "text": self.status_text() } },
                { "type": "actions", "elements": [
                    toggle,
                    { "type": "button", "action_id": "check_now", "text": { "type": "plain_text", "text": "Check now" } },
                    { "type": "static_select", "action_id": "set_schedule",
                      "placeholder": { "type": "plain_text", "text": "Change schedule" },
                      "options": [
                          opt("Every 30 minutes", "every 30m"),
                          opt("Every hour", "every 1h"),
                          opt("Every 2 hours", "every 2h"),
                          opt("Every 4 hours", "every 4h"),
                          opt("Daily at 09:00", "at 09:00"),
                          opt("At 09:00 and 14:00", "at 09:00,14:00"),
                      ] }
                ] },
                { "type": "divider" },
                { "type": "input", "block_id": "manual", "dispatch_action": true,
                  "label": { "type": "plain_text", "text": "Review a PR now" },
                  "element": { "type": "url_text_input", "action_id": "manual_url",
                               "placeholder": { "type": "plain_text", "text": "https://github.com/org/repo/pull/123, then press Enter" },
                               "dispatch_action_config": { "trigger_actions_on": ["on_enter_pressed"] } } },
                { "type": "context", "elements": [{ "type": "mrkdwn", "text":
                    "You can also DM me a PR link or use `/prreview help`. Reviews are posted only here in Slack. Posting one to GitHub takes your click, and it is always a comment, never an approval or a merge." }] }
            ]
        });
        if let Err(e) = self.slack.publish_home(&self.owner, view).await {
            eprintln!("could not update the Home tab: {e:#}");
        }
    }
}

fn header_blocks(r: &Review, id: &str) -> (String, Value) {
    let i = &r.info;
    let text = format!("{}: {}", r.pr.key(), r.verdict);
    let mut blocks = vec![
        json!({ "type": "section", "text": { "type": "mrkdwn", "text": format!(
            "*<{}|{}>* {}\n*Verdict:* {}", r.pr.url(), r.pr.key(), slack::esc(&i.title), slack::esc(&r.verdict)) } }),
        json!({ "type": "context", "elements": [{ "type": "mrkdwn", "text": format!(
            "by {} · +{} −{} in {} files · `{}` effort {} · ~${:.2} · full review in the thread",
            slack::esc(&i.author), i.additions, i.deletions, i.changed_files, r.model, claude::EFFORT, r.approx_cost_usd()) }] }),
    ];
    if !r.notes.is_empty() {
        let notes: String = slack::esc(&r.notes.join(" ")).chars().take(2800).collect();
        blocks.push(json!({ "type": "context", "elements": [{ "type": "mrkdwn", "text": format!(":information_source: {notes}") }] }));
    }
    blocks.push(json!({
        "type": "actions",
        "elements": [
            { "type": "button", "action_id": "post_to_github", "value": id,
              "text": { "type": "plain_text", "text": "Post as PR comment" },
              "confirm": {
                  "title": { "type": "plain_text", "text": "Post to GitHub?" },
                  "text": { "type": "mrkdwn", "text": "This posts the review on the PR as a *comment* from your GitHub account, visible to everyone with access. It does not approve or request changes." },
                  "confirm": { "type": "plain_text", "text": "Post" },
                  "deny": { "type": "plain_text", "text": "Cancel" }
              } },
            { "type": "button", "action_id": "rereview", "value": r.pr.url(),
              "text": { "type": "plain_text", "text": "Re-review" } }
        ]
    }));
    (text, Value::Array(blocks))
}

fn scope_label(query: &str) -> &'static str {
    if query.starts_with("user-review-requested") {
        "PRs requesting your review directly"
    } else {
        "PRs requesting your review directly or through your teams"
    }
}

fn fmt_time(t: DateTime<Local>) -> String {
    t.format("%a %d.%m. %H:%M").to_string()
}

/// "30m", "2h", "1.5h", "45" (minutes). Allowed range: 5 min to 24 h.
fn parse_interval(s: &str) -> Option<u32> {
    let s = s.trim().replace(' ', "");
    let (num, factor) = if let Some(n) = s.strip_suffix('h') {
        (n, 60.0)
    } else if let Some(n) = s.strip_suffix("min") {
        (n, 1.0)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 1.0)
    } else {
        (s.as_str(), 1.0)
    };
    let minutes = (num.parse::<f64>().ok()? * factor).round() as u32;
    (5..=1440).contains(&minutes).then_some(minutes)
}

/// "09:00,14:30" or "9:00 14:30" -> sorted ["09:00", "14:30"].
fn parse_times(s: &str) -> Option<Vec<String>> {
    let mut times = s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
        .map(|t| NaiveTime::parse_from_str(t, "%H:%M").ok().map(|t| t.format("%H:%M").to_string()))
        .collect::<Option<Vec<_>>>()?;
    times.sort();
    times.dedup();
    (!times.is_empty()).then_some(times)
}
