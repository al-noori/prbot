use crate::claude::{self, Settings};
use crate::review::InlineComment;
use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, Local, NaiveTime, Utc, Weekday};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io::ErrorKind, path::PathBuf, sync::Mutex};

const MAX_STORED_REVIEWS: usize = 100;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Schedule {
    Interval { minutes: u32 },
    /// Local wall-clock times, "HH:MM".
    Times { times: Vec<String> },
}

impl Schedule {
    pub fn describe(&self) -> String {
        match self {
            Schedule::Interval { minutes } if minutes % 60 == 0 => format!("every {} h", minutes / 60),
            Schedule::Interval { minutes } => format!("every {minutes} min"),
            Schedule::Times { times } => format!("daily at {}", times.join(", ")),
        }
    }

    /// Next scheduled check after `now`, inside the active window.
    pub fn next_after(&self, now: DateTime<Local>, window: &Window) -> Option<DateTime<Local>> {
        match self {
            Schedule::Interval { minutes } => window.next_active(now + Duration::minutes(*minutes as i64)),
            Schedule::Times { times } => {
                let parsed: Vec<NaiveTime> = times
                    .iter()
                    .filter_map(|t| NaiveTime::parse_from_str(t, "%H:%M").ok())
                    .collect();
                (0..8)
                    .flat_map(|d| {
                        let day = now.date_naive() + Duration::days(d);
                        parsed.iter().map(move |t| day.and_time(*t))
                    })
                    .filter_map(|naive| naive.and_local_timezone(Local).earliest())
                    .filter(|dt| *dt > now && window.contains(dt))
                    .min()
            }
        }
    }
}

/// Local working hours, "HH:MM". `end` before `start` means the window runs past midnight.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkHours {
    pub start: String,
    pub end: String,
}

/// When scheduled checks may run.
pub struct Window {
    pub weekdays_only: bool,
    pub hours: Option<(NaiveTime, NaiveTime)>,
}

impl Window {
    pub fn contains(&self, dt: &DateTime<Local>) -> bool {
        if self.weekdays_only && matches!(dt.weekday(), Weekday::Sat | Weekday::Sun) {
            return false;
        }
        match self.hours {
            None => true,
            Some((start, end)) if start <= end => (start..end).contains(&dt.time()),
            Some((start, end)) => dt.time() >= start || dt.time() < end,
        }
    }

    /// The first moment at or after `dt` (to the minute) inside the window, looking up to 8 days ahead.
    pub fn next_active(&self, dt: DateTime<Local>) -> Option<DateTime<Local>> {
        (0..8 * 24 * 60)
            .map(|m| dt + Duration::minutes(m))
            .find(|t| self.contains(t))
    }

    pub fn describe(&self) -> String {
        let days = if self.weekdays_only { "Mon–Fri" } else { "every day" };
        match self.hours {
            Some((s, e)) => format!("{days}, {}–{}", s.format("%H:%M"), e.format("%H:%M")),
            None => format!("{days}, all day"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredReview {
    pub pr_url: String,
    pub head_sha: String,
    /// Review body as posted to GitHub.
    pub body: String,
    #[serde(default)]
    pub comments: Vec<InlineComment>,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub posted_url: Option<String>,
}

/// A PR whose posted review is followed up: new commits get a re-review, replies get an answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Watch {
    pub pr_url: String,
    /// Head commit of the last review.
    pub reviewed_sha: String,
    /// Body of the last posted review, so a re-review knows what was said before.
    #[serde(default)]
    pub last_review: String,
    /// Comments created before this were already looked at.
    pub checked: DateTime<Utc>,
    /// Comment IDs already answered (or deliberately skipped).
    #[serde(default)]
    pub handled: Vec<u64>,
    /// When replies were posted, for the per-day cap.
    #[serde(default)]
    pub replies: Vec<DateTime<Utc>>,
    /// Last review or reply; PRs without activity for a while stop being followed.
    pub active: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Scheduled checks on/off. Manual reviews always work.
    pub enabled: bool,
    pub schedule: Schedule,
    pub weekdays_only: bool,
    /// Scheduled checks only run inside these hours. `None` means all day.
    pub work_hours: Option<WorkHours>,
    /// Post reviews to GitHub automatically (marked as not reviewed by the owner), instead of on click.
    pub autopost: bool,
    /// GitHub search qualifiers selecting the PRs to review.
    pub query: String,
    pub last_check: Option<DateTime<Local>>,
    /// "owner/repo#123" -> head SHA that was last reviewed.
    pub reviewed: HashMap<String, String>,
    /// Review id -> review, so the "Post as PR comment" button can find it.
    pub reviews: HashMap<String, StoredReview>,
    /// Claude model and effort chosen in Slack; `None` falls back to CLAUDE_MODEL / CLAUDE_EFFORT or the defaults.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// Re-review new commits and answer replies on PRs with a posted review.
    pub followup: bool,
    /// "owner/repo#123" -> follow-up state.
    pub watched: HashMap<String, Watch>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule: Schedule::Interval { minutes: 60 },
            weekdays_only: true,
            work_hours: Some(WorkHours { start: "08:00".into(), end: "18:00".into() }),
            autopost: true,
            query: "user-review-requested:@me".into(),
            last_check: None,
            reviewed: HashMap::new(),
            reviews: HashMap::new(),
            model: None,
            effort: None,
            followup: false,
            watched: HashMap::new(),
        }
    }
}

impl State {
    pub fn settings(&self) -> Settings {
        let env = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        Settings {
            model: self
                .model
                .clone()
                .or_else(|| env("CLAUDE_MODEL").and_then(|m| claude::resolve_model(&m)))
                .unwrap_or_else(|| claude::DEFAULT_MODEL.into()),
            effort: self
                .effort
                .clone()
                .or_else(|| env("CLAUDE_EFFORT").filter(|e| claude::EFFORTS.contains(&e.as_str())))
                .unwrap_or_else(|| claude::DEFAULT_EFFORT.into()),
        }
    }

    pub fn window(&self) -> Window {
        let parse = |t: &str| NaiveTime::parse_from_str(t, "%H:%M").ok();
        Window {
            weekdays_only: self.weekdays_only,
            hours: self.work_hours.as_ref().and_then(|h| Some((parse(&h.start)?, parse(&h.end)?))),
        }
    }
}

/// JSON-file-backed state. Every update is written to disk immediately.
pub struct Store {
    path: PathBuf,
    state: Mutex<State>,
}

impl Store {
    pub fn load(path: PathBuf) -> Result<Self> {
        let state = match std::fs::read_to_string(&path) {
            Ok(s) => serde_json::from_str(&s)?,
            Err(e) if e.kind() == ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, state: Mutex::new(state) })
    }

    pub fn get(&self) -> State {
        self.state.lock().unwrap().clone()
    }

    pub fn update<R>(&self, f: impl FnOnce(&mut State) -> R) -> Result<R> {
        let mut s = self.state.lock().unwrap();
        let r = f(&mut s);
        if s.reviews.len() > MAX_STORED_REVIEWS {
            let mut by_age: Vec<(String, DateTime<Utc>)> =
                s.reviews.iter().map(|(k, v)| (k.clone(), v.created)).collect();
            by_age.sort_by_key(|(_, created)| *created);
            let excess = s.reviews.len() - MAX_STORED_REVIEWS;
            for (k, _) in by_age.into_iter().take(excess) {
                s.reviews.remove(&k);
            }
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&*s)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(r)
    }
}
