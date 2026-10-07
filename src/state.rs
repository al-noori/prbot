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

    pub fn next_after(&self, now: DateTime<Local>, weekdays_only: bool) -> Option<DateTime<Local>> {
        match self {
            Schedule::Interval { minutes } => Some(now + Duration::minutes(*minutes as i64)),
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
                    .filter(|dt| *dt > now && !(weekdays_only && is_weekend(dt)))
                    .min()
            }
        }
    }
}

pub fn is_weekend(dt: &DateTime<Local>) -> bool {
    matches!(dt.weekday(), Weekday::Sat | Weekday::Sun)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredReview {
    pub pr_url: String,
    pub head_sha: String,
    pub body: String,
    pub created: DateTime<Utc>,
    #[serde(default)]
    pub posted_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct State {
    /// Scheduled checks on/off. Manual reviews always work.
    pub enabled: bool,
    pub schedule: Schedule,
    pub weekdays_only: bool,
    /// GitHub search qualifiers selecting the PRs to review.
    pub query: String,
    pub last_check: Option<DateTime<Local>>,
    /// "owner/repo#123" -> head SHA that was last reviewed.
    pub reviewed: HashMap<String, String>,
    /// Review id -> review, so the "Post as PR comment" button can find it.
    pub reviews: HashMap<String, StoredReview>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule: Schedule::Interval { minutes: 60 },
            weekdays_only: true,
            query: "user-review-requested:@me".into(),
            last_check: None,
            reviewed: HashMap::new(),
            reviews: HashMap::new(),
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
