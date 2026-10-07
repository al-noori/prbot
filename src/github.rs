//! Minimal GitHub REST client.
//!
//! By design there is no function here to approve, request changes, or merge:
//! the writes are `post_review` (always event "COMMENT"), `comment`, `reply_to_review_comment` and reactions.

use crate::review::InlineComment;
use chrono::{DateTime, Utc};
use anyhow::{anyhow, bail, Context, Result};
use regex::Regex;
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode, Url};
use serde_json::{json, Value};
use std::sync::OnceLock;

const API: &str = "https://api.github.com";
const MAX_FILE_BYTES: usize = 150_000;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}

impl PrRef {
    /// Finds a GitHub PR URL anywhere in `text` (also inside Slack's `<url|label>` links).
    pub fn parse(text: &str) -> Option<Self> {
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| Regex::new(r"github\.com/([\w.-]+)/([\w.-]+)/pull/(\d+)").unwrap());
        let c = re.captures(text)?;
        Some(Self { owner: c[1].to_string(), repo: c[2].to_string(), number: c[3].parse().ok()? })
    }

    pub fn key(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }

    pub fn url(&self) -> String {
        format!("https://github.com/{}/{}/pull/{}", self.owner, self.repo, self.number)
    }

    fn api(&self, suffix: &str) -> String {
        format!("{API}/repos/{}/{}/pulls/{}{suffix}", self.owner, self.repo, self.number)
    }
}

#[derive(Debug, Clone)]
pub struct PrInfo {
    pub title: String,
    pub body: String,
    pub author: String,
    pub head_sha: String,
    pub head_ref: String,
    pub base_ref: String,
    pub draft: bool,
    pub open: bool,
    pub additions: u64,
    pub deletions: u64,
    pub changed_files: u64,
}

#[derive(Debug, Clone)]
pub struct ChangedFile {
    pub filename: String,
    pub status: String,
    pub patch: Option<String>,
}


/// A PR comment: a review comment on a diff line, or a conversation comment.
#[derive(Debug, Clone)]
pub struct Comment {
    pub id: u64,
    pub author: String,
    pub author_is_bot: bool,
    pub body: String,
    pub created: DateTime<Utc>,
    pub url: String,
    /// Review comments: the first comment of the thread this one replies to.
    pub in_reply_to: Option<u64>,
    pub path: Option<String>,
    pub diff_hunk: Option<String>,
}

impl Comment {
    fn from_json(v: &Value) -> Self {
        Self {
            id: v["id"].as_u64().unwrap_or_default(),
            author: s(&v["user"]["login"]),
            author_is_bot: v["user"]["type"] == "Bot" || s(&v["user"]["login"]).ends_with("[bot]"),
            body: s(&v["body"]),
            created: v["created_at"].as_str().and_then(|t| t.parse().ok()).unwrap_or_default(),
            url: s(&v["html_url"]),
            in_reply_to: v["in_reply_to_id"].as_u64(),
            path: v["path"].as_str().map(String::from),
            diff_hunk: v["diff_hunk"].as_str().map(String::from),
        }
    }
}

pub struct GitHub {
    http: Client,
    token: String,
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_string()
}

async fn ok(resp: Response) -> Result<Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let url = resp.url().to_string();
    let body = resp.text().await.unwrap_or_default();
    bail!("GitHub {status} for {url}: {}", body.chars().take(300).collect::<String>())
}

impl GitHub {
    pub fn new(http: Client, token: String) -> Self {
        Self { http, token }
    }

    fn req(&self, method: Method, url: impl reqwest::IntoUrl, accept: &str) -> RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    async fn get_json(&self, url: &str) -> Result<Value> {
        let resp = self.req(Method::GET, url, "application/vnd.github+json").send().await?;
        Ok(ok(resp).await?.json().await?)
    }

    pub async fn login(&self) -> Result<String> {
        let v = self.get_json(&format!("{API}/user")).await?;
        v["login"].as_str().map(String::from).context("GitHub /user returned no login")
    }

    /// Open PRs matching `query` (e.g. "user-review-requested:@me"), oldest first.
    pub async fn review_requests(&self, query: &str) -> Result<Vec<PrRef>> {
        let q = format!("is:pr is:open archived:false {query}");
        let resp = self
            .req(Method::GET, format!("{API}/search/issues"), "application/vnd.github+json")
            .query(&[("q", q.as_str()), ("per_page", "50"), ("sort", "created"), ("order", "asc")])
            .send()
            .await?;
        let v: Value = ok(resp).await?.json().await?;
        Ok(v["items"]
            .as_array()
            .map(|items| items.iter().filter_map(|i| i["html_url"].as_str().and_then(PrRef::parse)).collect())
            .unwrap_or_default())
    }

    pub async fn pr(&self, pr: &PrRef) -> Result<PrInfo> {
        let v = self.get_json(&pr.api("")).await?;
        Ok(PrInfo {
            title: s(&v["title"]),
            body: s(&v["body"]),
            author: s(&v["user"]["login"]),
            head_sha: s(&v["head"]["sha"]),
            head_ref: s(&v["head"]["ref"]),
            base_ref: s(&v["base"]["ref"]),
            draft: v["draft"].as_bool().unwrap_or(false),
            open: v["state"] == "open",
            additions: v["additions"].as_u64().unwrap_or(0),
            deletions: v["deletions"].as_u64().unwrap_or(0),
            changed_files: v["changed_files"].as_u64().unwrap_or(0),
        })
    }

    /// Unified diff. GitHub refuses this for very large PRs; callers fall back to `files`.
    pub async fn diff(&self, pr: &PrRef) -> Result<String> {
        let resp = self.req(Method::GET, pr.api(""), "application/vnd.github.diff").send().await?;
        Ok(ok(resp).await?.text().await?)
    }

    pub async fn files(&self, pr: &PrRef) -> Result<Vec<ChangedFile>> {
        let mut out = Vec::new();
        for page in 1..=30 {
            let v = self.get_json(&pr.api(&format!("/files?per_page=100&page={page}"))).await?;
            let items = v.as_array().cloned().unwrap_or_default();
            out.extend(items.iter().map(|f| ChangedFile {
                filename: s(&f["filename"]),
                status: s(&f["status"]),
                patch: f["patch"].as_str().map(String::from),
            }));
            if items.len() < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// File contents at `sha`; `None` if missing, binary, or too large.
    pub async fn file_at(&self, pr: &PrRef, path: &str, sha: &str) -> Result<Option<String>> {
        let mut url = Url::parse(API)?;
        url.path_segments_mut()
            .map_err(|_| anyhow!("bad GitHub API base URL"))?
            .pop_if_empty()
            .extend(["repos", pr.owner.as_str(), pr.repo.as_str(), "contents"])
            .extend(path.split('/'));
        url.query_pairs_mut().append_pair("ref", sha);
        let resp = self.req(Method::GET, url, "application/vnd.github.raw+json").send().await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let bytes = ok(resp).await?.bytes().await?;
        if bytes.len() > MAX_FILE_BYTES {
            return Ok(None);
        }
        Ok(String::from_utf8(bytes.to_vec()).ok())
    }

    /// Review comments (on lines of the diff), oldest first, at most 300.
    pub async fn review_comments(&self, pr: &PrRef) -> Result<Vec<Comment>> {
        let mut out = Vec::new();
        for page in 1..=3 {
            let v = self.get_json(&pr.api(&format!("/comments?per_page=100&page={page}"))).await?;
            let items = v.as_array().cloned().unwrap_or_default();
            out.extend(items.iter().map(Comment::from_json));
            if items.len() < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// Conversation comments on the PR, oldest first, at most 300.
    pub async fn issue_comments(&self, pr: &PrRef) -> Result<Vec<Comment>> {
        let mut out = Vec::new();
        for page in 1..=3 {
            let url = format!("{API}/repos/{}/{}/issues/{}/comments?per_page=100&page={page}", pr.owner, pr.repo, pr.number);
            let v = self.get_json(&url).await?;
            let items = v.as_array().cloned().unwrap_or_default();
            out.extend(items.iter().map(Comment::from_json));
            if items.len() < 100 {
                break;
            }
        }
        Ok(out)
    }

    /// Replies in the thread of review comment `comment_id`. Returns the reply's URL.
    pub async fn reply_to_review_comment(&self, pr: &PrRef, comment_id: u64, body: &str) -> Result<String> {
        let resp = self
            .req(Method::POST, pr.api(&format!("/comments/{comment_id}/replies")), "application/vnd.github+json")
            .json(&json!({ "body": body }))
            .send()
            .await?;
        let v: Value = ok(resp).await?.json().await?;
        Ok(s(&v["html_url"]))
    }

    /// Adds a reaction such as "+1" to a review comment.
    pub async fn react_to_review_comment(&self, pr: &PrRef, comment_id: u64, content: &str) -> Result<()> {
        let url = format!("{API}/repos/{}/{}/pulls/comments/{comment_id}/reactions", pr.owner, pr.repo);
        let resp = self.req(Method::POST, url, "application/vnd.github+json").json(&json!({ "content": content })).send().await?;
        ok(resp).await.map(|_| ())
    }

    /// Adds a reaction such as "+1" to a conversation comment.
    pub async fn react_to_issue_comment(&self, pr: &PrRef, comment_id: u64, content: &str) -> Result<()> {
        let url = format!("{API}/repos/{}/{}/issues/comments/{comment_id}/reactions", pr.owner, pr.repo);
        let resp = self.req(Method::POST, url, "application/vnd.github+json").json(&json!({ "content": content })).send().await?;
        ok(resp).await.map(|_| ())
    }

    /// Adds a conversation comment to the PR. Returns its URL.
    pub async fn comment(&self, pr: &PrRef, body: &str) -> Result<String> {
        let url = format!("{API}/repos/{}/{}/issues/{}/comments", pr.owner, pr.repo, pr.number);
        let resp = self.req(Method::POST, url, "application/vnd.github+json").json(&json!({ "body": body })).send().await?;
        let v: Value = ok(resp).await?.json().await?;
        Ok(s(&v["html_url"]))
    }

    /// Posts a PR review with event COMMENT (never APPROVE / REQUEST_CHANGES), with optional inline comments.
    /// If GitHub rejects the inline comments, it retries once with them folded into the body.
    pub async fn post_review(&self, pr: &PrRef, commit_id: &str, body: &str, comments: &[InlineComment]) -> Result<String> {
        let inline: Vec<Value> = comments
            .iter()
            .map(|c| json!({ "path": c.path, "line": c.line, "side": "RIGHT", "body": c.body }))
            .collect();
        let send = |body: String, inline: Vec<Value>| {
            self.req(Method::POST, pr.api("/reviews"), "application/vnd.github+json")
                .json(&json!({ "commit_id": commit_id, "body": body, "event": "COMMENT", "comments": inline }))
                .send()
        };
        let mut resp = send(body.to_string(), inline).await?;
        if resp.status() == StatusCode::UNPROCESSABLE_ENTITY && !comments.is_empty() {
            let mut folded = format!("{body}\n### Line comments\n");
            for c in comments {
                folded.push_str(&format!("`{}:{}`\n{}\n\n", c.path, c.line, c.body));
            }
            resp = send(folded, Vec::new()).await?;
        }
        let v: Value = ok(resp).await?.json().await?;
        Ok(v["html_url"].as_str().map(String::from).unwrap_or_else(|| pr.url()))
    }
}
