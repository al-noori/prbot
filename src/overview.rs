//! The overview posted on every check: the PRs on your plate, each marked waiting, in work or done,
//! with a menu for details and for marking it done (which also stops its follow-ups).

use crate::bot::Bot;
use crate::github::{PrInfo, PrRef};
use crate::slack;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::{Arc, OnceLock};

pub const WAITING: &str = ":hourglass_flowing_sand:";
pub const IN_WORK: &str = ":arrows_counterclockwise:";
pub const DONE: &str = ":white_check_mark:";
const MAX_ROWS: usize = 45;

/// One PR in the overview.
pub struct Row {
    pub pr: PrRef,
    pub info: PrInfo,
    pub symbol: &'static str,
    pub status: String,
}

impl Bot {
    /// Runs a check: starts reviews for PRs that need one, then posts the overview.
    pub async fn check_and_overview(self: &Arc<Self>, manual: bool) -> Result<()> {
        let st = self.store.get();
        let prs = self.gh.review_requests(&st.query).await?;
        let mut rows = Vec::new();
        let mut queued = 0;
        for pr in prs {
            let info = self.gh.pr(&pr).await?;
            let key = pr.key();
            // A PR you marked done comes back once its author pushes new commits and asks again.
            if st.done.get(&key).is_some_and(|sha| *sha != info.head_sha) {
                self.store.update(|s| s.done.remove(&key))?;
            }
            let (symbol, status) = if self.store.get().done.contains_key(&key) {
                (DONE, "done (marked by you)".to_string())
            } else if info.draft && self.cfg.skip_drafts {
                (WAITING, "draft, skipped until it's ready".into())
            } else if st.reviewed.get(&key) == Some(&info.head_sha) {
                (IN_WORK, "Claude reviewed the latest commit".into())
            } else if self.is_reviewing(&key) {
                (IN_WORK, "Claude is reviewing it".into())
            } else if queued >= self.cfg.max_reviews_per_run {
                (WAITING, "waiting for the next check (limit per check reached)".into())
            } else if self.spawn_review(pr.clone()) {
                queued += 1;
                (IN_WORK, "Claude is reviewing it now".into())
            } else {
                (IN_WORK, "Claude is reviewing it".into())
            };
            rows.push(Row { pr, info, symbol, status });
        }

        // PRs that no longer request your review but are still on your plate: reviewed and followed, or done.
        let listed: Vec<String> = rows.iter().map(|r| r.pr.key()).collect();
        let st = self.store.get();
        let mut others: Vec<(String, String)> = st.watched.iter().map(|(k, w)| (k.clone(), w.pr_url.clone())).collect();
        others.extend(st.done.keys().filter(|k| !st.watched.contains_key(*k)).map(|k| (k.clone(), String::new())));
        for (key, url) in others {
            if listed.contains(&key) {
                continue;
            }
            let Some(pr) = PrRef::parse(&url).or_else(|| PrRef::parse_key(&key)) else { continue };
            let Ok(info) = self.gh.pr(&pr).await else { continue };
            if !info.open {
                self.store.update(|s| s.done.remove(&key))?;
                continue;
            }
            let (symbol, status) = if st.done.contains_key(&key) {
                (DONE, "done (marked by you)".to_string())
            } else {
                let fresh = st.watched.get(&key).is_some_and(|w| w.reviewed_sha != info.head_sha);
                let status = if fresh { "new commits since Claude's review" } else { "reviewed by Claude, waiting on the author" };
                (IN_WORK, format!("{status}{}", if st.followup { ", followed" } else { "" }))
            };
            rows.push(Row { pr, info, symbol, status });
        }

        self.store.update(|s| s.last_check = Some(chrono::Local::now()))?;
        let header = if manual { "Your PR reviews" } else { "Scheduled check: your PR reviews" };
        let (text, blocks) = overview_message(header, &rows);
        self.slack.post(&self.dm, &text, Some(blocks), None).await?;
        self.refresh_home().await;
        Ok(())
    }

    /// Marks a PR done (no more follow-ups) or reopens it.
    pub async fn set_done(&self, pr: &PrRef, done: bool) -> Result<String> {
        let key = pr.key();
        if done {
            let head = self.gh.pr(pr).await?.head_sha;
            self.store.update(|s| {
                s.done.insert(key.clone(), head);
                s.watched.remove(&key);
            })?;
            Ok(format!("{DONE} Marked <{}|{key}> as done. Claude won't follow up on it anymore.", pr.url()))
        } else {
            // A posted review within the last 14 days makes follow-ups pick the PR up again.
            self.store.update(|s| s.done.remove(&key))?;
            Ok(format!("{IN_WORK} Reopened <{}|{key}>.", pr.url()))
        }
    }

    /// What's going on with a PR, for the overview's Details menu.
    pub async fn pr_details(&self, pr: &PrRef) -> Result<String> {
        let info = self.gh.pr(pr).await?;
        let st = self.store.get();
        let key = pr.key();
        let mut out = vec![format!("*<{}|{key}>* {}", pr.url(), slack::esc(&info.title))];

        let state = if info.merged {
            "merged".to_string()
        } else if !info.open {
            "closed".into()
        } else if info.draft {
            "draft".into()
        } else {
            format!("open, mergeable state: {}", if info.mergeable_state.is_empty() { "unknown" } else { &info.mergeable_state })
        };
        out.push(format!("• *State:* {state} · by {} · last updated {}", slack::esc(&info.author), ago(info.updated)));
        if let Ok(checks) = self.gh.checks_summary(pr, &info.head_sha).await {
            out.push(format!("• *Checks:* {checks}"));
        }

        let last_review = st
            .reviews
            .values()
            .filter(|r| r.pr_url == pr.url() && r.posted_url.is_some())
            .max_by_key(|r| r.created);
        match last_review {
            Some(r) => {
                out.push(format!(
                    "• *Claude's last review:* {} at `{}`. Verdict: {}",
                    ago(r.created),
                    &r.head_sha[..r.head_sha.len().min(7)],
                    slack::esc(&verdict(&r.body))
                ));
                if r.head_sha != info.head_sha {
                    match self.gh.compare(pr, &r.head_sha, &info.head_sha).await {
                        Ok((n, messages)) => {
                            let list: Vec<String> = messages.iter().take(5).map(|m| format!("    ◦ {}", slack::esc(m))).collect();
                            out.push(format!("• *Since then:* {n} new commit(s)\n{}", list.join("\n")));
                        }
                        Err(_) => out.push("• *Since then:* new commits".into()),
                    }
                } else {
                    out.push("• *Since then:* no new commits".into());
                }
                let since = r.created;
                let review_comments = self.gh.review_comments(pr).await.unwrap_or_default();
                let issue_comments = self.gh.issue_comments(pr).await.unwrap_or_default();
                let new: Vec<_> = review_comments
                    .iter()
                    .chain(issue_comments.iter())
                    .filter(|c| c.created > since)
                    .collect();
                let by_you = new.iter().filter(|c| c.author.eq_ignore_ascii_case(&self.gh_login)).count();
                if let Some(last) = new.iter().max_by_key(|c| c.created) {
                    out.push(format!(
                        "• *Discussion:* {} comment(s) since the review, {by_you} from your account (Claude's replies included); latest {} by {}",
                        new.len(),
                        ago(last.created),
                        slack::esc(&last.author)
                    ));
                } else {
                    out.push("• *Discussion:* no comments since the review".into());
                }
            }
            None => out.push("• *Claude's last review:* none posted yet".into()),
        }

        if let Ok(reviews) = self.gh.reviews(pr).await {
            let mut latest: Vec<(String, String)> = Vec::new();
            for (user, state) in reviews {
                if user.eq_ignore_ascii_case(&self.gh_login) || state == "COMMENTED" {
                    continue;
                }
                latest.retain(|(u, _)| *u != user);
                latest.push((user, state));
            }
            if !latest.is_empty() {
                let list: Vec<String> = latest
                    .iter()
                    .map(|(u, s)| format!("{} {}", slack::esc(u), s.to_lowercase().replace('_', " ")))
                    .collect();
                out.push(format!("• *Other reviewers:* {}", list.join(", ")));
            }
        }

        let follow = if st.done.contains_key(&key) {
            "done (marked by you), not followed"
        } else if st.watched.contains_key(&key) && st.followup {
            "followed: new commits get a re-review, replies get an answer"
        } else if st.watched.contains_key(&key) {
            "would be followed, but follow-ups are off"
        } else {
            "not followed"
        };
        out.push(format!("• *Follow-ups:* {follow}"));
        Ok(out.join("\n"))
    }

    /// Handles a choice from an overview row's menu.
    pub async fn on_pr_menu(&self, p: &Value, choice: &str) -> Result<()> {
        let (cmd, key) = choice.split_once(' ').context("bad menu value")?;
        let pr = PrRef::parse_key(key).context("bad PR in menu value")?;
        let channel = p["container"]["channel_id"].as_str().unwrap_or(&self.dm).to_string();
        let ts = p["container"]["message_ts"].as_str().unwrap_or_default().to_string();
        match cmd {
            "details" => {
                let text = self.pr_details(&pr).await?;
                self.slack.post(&channel, &text, None, Some(&ts)).await?;
            }
            "done" | "reopen" => {
                let done = cmd == "done";
                let reply = self.set_done(&pr, done).await?;
                let message = match p["message"].is_null() {
                    true => self.slack.message(&channel, &ts).await?,
                    false => p["message"].clone(),
                };
                let mut blocks = message["blocks"].as_array().cloned().unwrap_or_default();
                let block_id = format!("pr:{key}");
                if let Some(b) = blocks.iter_mut().find(|b| b["block_id"] == block_id.as_str()) {
                    let text = b["text"]["text"].as_str().unwrap_or_default();
                    let (symbol, status) = if done { (DONE, "done (marked by you)") } else { (IN_WORK, "reopened") };
                    *b = row_block(&pr, symbol, &replace_status(text, symbol, status), done);
                }
                let fallback = message["text"].as_str().unwrap_or("Your PR reviews").to_string();
                self.slack.update(&channel, &ts, &fallback, Some(Value::Array(blocks))).await?;
                self.slack.post(&channel, &reply, None, Some(&ts)).await?;
            }
            _ => {}
        }
        Ok(())
    }
}

fn overview_message(header: &str, rows: &[Row]) -> (String, Value) {
    let count = |s: &str| rows.iter().filter(|r| r.symbol == s).count();
    let summary = format!(
        "{} waiting · {} in work · {} done",
        count(WAITING),
        count(IN_WORK),
        count(DONE)
    );
    let text = format!(":clipboard: {header}: {summary}");
    let mut blocks = vec![json!({ "type": "section", "text": { "type": "mrkdwn", "text": format!(
        ":clipboard: *{header}*\n{WAITING} waiting for review   {IN_WORK} in work   {DONE} done\n_{summary}. Use ⋯ on a row for details or to mark it done._") } })];
    if rows.is_empty() {
        blocks.push(json!({ "type": "section", "text": { "type": "mrkdwn", "text": "Nothing on your plate right now." } }));
    }
    for r in rows.iter().take(MAX_ROWS) {
        let text = format!(
            "{} *<{}|{}>* {}\n_{}_ · by {}",
            r.symbol,
            r.pr.url(),
            r.pr.key(),
            slack::esc(&r.info.title),
            slack::esc(&r.status),
            slack::esc(&r.info.author)
        );
        blocks.push(row_block(&r.pr, r.symbol, &text, r.symbol == DONE));
    }
    if rows.len() > MAX_ROWS {
        blocks.push(json!({ "type": "context", "elements": [{ "type": "mrkdwn", "text": format!("…and {} more", rows.len() - MAX_ROWS) }] }));
    }
    (text, Value::Array(blocks))
}

fn row_block(pr: &PrRef, _symbol: &str, text: &str, done: bool) -> Value {
    let opt = |label: &str, value: String| json!({ "text": { "type": "plain_text", "text": label }, "value": value });
    let key = pr.key();
    let toggle = if done { opt("Reopen", format!("reopen {key}")) } else { opt("Mark done", format!("done {key}")) };
    json!({
        "type": "section",
        "block_id": format!("pr:{key}"),
        "text": { "type": "mrkdwn", "text": text.chars().take(2900).collect::<String>() },
        "accessory": { "type": "overflow", "action_id": "pr_menu", "options": [opt("Details", format!("details {key}")), toggle] }
    })
}

/// Swaps the symbol at the start of a row and its italic status.
fn replace_status(text: &str, symbol: &str, status: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?s)^:[a-z_]+: (.*?)\n_[^_]*_").unwrap());
    re.replace(text, |c: &regex::Captures| format!("{symbol} {}\n_{status}_", &c[1])).into_owned()
}

/// The verdict line from a posted review body.
fn verdict(body: &str) -> String {
    body.lines()
        .find_map(|l| l.strip_prefix("**Verdict:** "))
        .unwrap_or("unknown")
        .to_string()
}

fn ago(t: DateTime<Utc>) -> String {
    let d = Utc::now() - t;
    if d.num_minutes() < 1 {
        "just now".into()
    } else if d.num_hours() < 1 {
        format!("{} min ago", d.num_minutes())
    } else if d.num_days() < 1 {
        format!("{} h ago", d.num_hours())
    } else {
        format!("{} days ago", d.num_days())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_status_swaps_symbol_and_status() {
        let text = ":arrows_counterclockwise: *<u|o/r#1>* Title\n_Claude reviewed it_ · by ann";
        assert_eq!(
            replace_status(text, DONE, "done (marked by you)"),
            ":white_check_mark: *<u|o/r#1>* Title\n_done (marked by you)_ · by ann"
        );
    }

    #[test]
    fn verdict_is_read_from_the_review_body() {
        assert_eq!(verdict("> intro\n\n**Verdict:** Minor comments — x\n"), "Minor comments — x");
    }
}
