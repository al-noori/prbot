//! Minimal GitHub REST client.
//!
//! By design there is no function here to approve, request changes, or merge:
//! the only write is `post_comment_review`, which always uses event "COMMENT".

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

    /// Posts `body` as a PR review with event COMMENT (never APPROVE / REQUEST_CHANGES).
    pub async fn post_comment_review(&self, pr: &PrRef, commit_id: &str, body: &str) -> Result<String> {
        let resp = self
            .req(Method::POST, pr.api("/reviews"), "application/vnd.github+json")
            .json(&json!({ "commit_id": commit_id, "body": body, "event": "COMMENT" }))
            .send()
            .await?;
        let v: Value = ok(resp).await?.json().await?;
        Ok(v["html_url"].as_str().map(String::from).unwrap_or_else(|| pr.url()))
    }
}
