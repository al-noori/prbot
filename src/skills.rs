//! Review know-how vendored from other projects (see `skills/`), embedded into prompts:
//! gstack's review checklists for reviews, superpowers' receiving-code-review for fixes and replies.
//! The files are kept as copied, so they can be updated by re-copying; anything that only makes
//! sense inside those projects (their output formats, auto-fix steps, tool commands) is cut here.

const CHECKLIST: &str = include_str!("../skills/gstack-review/checklist.md");
const SECURITY: &str = include_str!("../skills/gstack-review/specialists/security.md");
const TESTING: &str = include_str!("../skills/gstack-review/specialists/testing.md");
const MAINTAINABILITY: &str = include_str!("../skills/gstack-review/specialists/maintainability.md");
const API_CONTRACT: &str = include_str!("../skills/gstack-review/specialists/api-contract.md");
const DATA_MIGRATION: &str = include_str!("../skills/gstack-review/specialists/data-migration.md");
const PERFORMANCE: &str = include_str!("../skills/gstack-review/specialists/performance.md");
const SIMPLIFICATION: &str = include_str!("../skills/gstack-review/specialists/simplification.md");
const RED_TEAM: &str = include_str!("../skills/gstack-review/specialists/red-team.md");
const RECEIVING_REVIEW: &str = include_str!("../skills/superpowers/receiving-code-review/SKILL.md");

/// gstack runs the simplification lens above this many changed lines, and red team above 200.
const SIMPLIFY_LINES: u64 = 100;
const RED_TEAM_LINES: u64 = 200;

const API_HINTS: &[&str] = &[
    "controller", "route", "endpoint", "handler", "api", "openapi", "swagger", "graphql", ".proto", "dto",
    "webhook", "event", "kafka", "client", "sdk",
];
const DATA_HINTS: &[&str] = &[
    "migration", "migrate", "liquibase", "flyway", "changelog", ".sql", "schema", "entity", "entities",
    "repository", "model", "prisma", "alembic",
];
const PERF_HINTS: &[&str] = &[
    "query", "queries", ".sql", "cache", "repository", "batch", "stream", "worker", "job",
    ".tsx", ".jsx", ".vue", ".svelte",
];

const REVIEW_FRAMING: &str = "The checklists below are from gstack's pre-landing review (MIT). Use them as a list of \
what to look for, not as instructions on how to work or answer: ignore their output formats and JSON schemas, \
\"Fix-First\", AUTO-FIX/ASK and anything about fixing code, asking the user or subagents; to look at code outside the diff, use only what this \
prompt offers. Their examples are \
mostly Ruby, Python and JavaScript; apply the same idea in this PR's languages, and skip categories that don't apply. \
Map their severities onto yours: CRITICAL issues are \"blocking\" or \"major\", INFORMATIONAL ones \"minor\" or \"nit\", \
and simplification findings are optional suggestions (at most \"minor\", say they are optional). Honour the \
suppressions. Finish with the red team lens, if present, against your own findings: look for what the checklists \
missed. Then try to refute each finding by reading the actual code, and drop any you can't back with concrete evidence \
from this PR.";

const RECEIVING_FRAMING: &str = "How to handle review feedback, from superpowers' receiving-code-review skill (MIT). \
\"Your human partner\" is the engineer; everyone else is an external reviewer. You can't ask anyone or run tests here: \
where the skill says to ask, stop or test, answer in a reply instead and leave the code as it is.";

/// The gstack checklists that fit a PR, picked like gstack does it: by changed paths and size.
pub fn review_checklists(paths: &[&str], changed_lines: u64) -> String {
    let lower: Vec<String> = paths.iter().map(|p| p.to_lowercase()).collect();
    let touches = |hints: &[&str]| lower.iter().any(|p| hints.iter().any(|h| p.contains(h)));
    let mut picked = vec![
        ("core", core_checklist()),
        ("security", specialist(SECURITY)),
        ("testing", specialist(TESTING)),
        ("maintainability", specialist(MAINTAINABILITY)),
    ];
    if touches(API_HINTS) {
        picked.push(("api-contract", specialist(API_CONTRACT)));
    }
    if touches(DATA_HINTS) {
        picked.push(("data-migration", specialist(DATA_MIGRATION)));
    }
    if touches(PERF_HINTS) {
        picked.push(("performance", specialist(PERFORMANCE)));
    }
    if changed_lines > SIMPLIFY_LINES {
        picked.push(("simplification", specialist(SIMPLIFICATION)));
    }
    if changed_lines > RED_TEAM_LINES {
        picked.push(("red-team", specialist(RED_TEAM)));
    }
    let mut s = format!("<review_checklists source=\"gstack\">\n{REVIEW_FRAMING}\n\n");
    for (name, body) in picked {
        s.push_str(&format!("<checklist name=\"{name}\">\n{}\n</checklist>\n", body.trim()));
    }
    s.push_str("</review_checklists>\n\n");
    s
}

/// The receiving-code-review skill, for drafting fixes and replies to review comments.
pub fn receiving_review() -> String {
    let text = normalize(RECEIVING_REVIEW);
    // Drop the front matter, and the closing note on posting thread replies, which prbot does itself.
    let body = text.splitn(3, "---\n").nth(2).unwrap_or(&text);
    let body = body.split("## GitHub Thread Replies").next().unwrap_or(body);
    format!("<review_feedback_guide source=\"superpowers\">\n{RECEIVING_FRAMING}\n\n{}\n</review_feedback_guide>\n\n", body.trim())
}

/// The review categories and suppressions of gstack's main checklist, without its workflow.
fn core_checklist() -> String {
    let text = normalize(CHECKLIST);
    let categories = section(&text, "## Review Categories", "## Severity Classification");
    let suppressions = text.find("## Suppressions").map(|i| &text[i..]).unwrap_or("");
    drop_gstack_lines(&format!("{categories}\n{suppressions}"))
}

/// A specialist checklist without its header (scope and output schema), which ends at the first `---`.
fn specialist(raw: &str) -> String {
    let text = normalize(raw);
    let body = text.split_once("\n---\n").map(|(_, b)| b).unwrap_or(&text);
    drop_gstack_lines(body)
}

/// From `start` up to (not including) `end`, or to the end of the text; empty if `start` is missing.
fn section<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    let Some(from) = text.find(start) else { return "" };
    let rest = &text[from..];
    rest.find(end).map(|to| &rest[..to]).unwrap_or(rest)
}

/// Lines about gstack's own tooling (its decision ledger, effort estimates) mean nothing outside it.
fn drop_gstack_lines(text: &str) -> String {
    text.lines().filter(|l| !l.contains("gstack")).collect::<Vec<_>>().join("\n")
}

fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_checklist_keeps_categories_and_drops_workflow() {
        let c = core_checklist();
        assert!(c.contains("### Pass 1 — CRITICAL"));
        assert!(c.contains("LLM Output Trust Boundary"));
        assert!(c.contains("## Suppressions"));
        assert!(!c.contains("Fix-First Heuristic"));
        assert!(!c.contains("Output format"));
        assert!(!c.contains("gstack"));
    }

    #[test]
    fn specialists_lose_their_output_schema() {
        for raw in [SECURITY, TESTING, MAINTAINABILITY, API_CONTRACT, DATA_MIGRATION, PERFORMANCE, SIMPLIFICATION, RED_TEAM] {
            let s = specialist(raw);
            assert!(s.len() > 500, "specialist body went missing");
            assert!(!s.contains("NO FINDINGS"));
            assert!(!s.contains("Scope:"));
        }
    }

    #[test]
    fn picks_specialists_by_paths_and_size() {
        let small = review_checklists(&["src/util/strings.rs"], 20);
        assert!(small.contains("name=\"security\""));
        assert!(small.contains("name=\"testing\""));
        assert!(!small.contains("name=\"api-contract\""));
        assert!(!small.contains("name=\"red-team\""));

        let big = review_checklists(&["src/api/UserController.java", "db/migration/V2__users.sql"], 450);
        for name in ["api-contract", "data-migration", "performance", "simplification", "red-team"] {
            assert!(big.contains(&format!("name=\"{name}\"")), "{name} missing");
        }
    }

    #[test]
    fn receiving_review_drops_front_matter_and_thread_note() {
        let r = receiving_review();
        assert!(r.contains("## The Response Pattern"));
        assert!(!r.contains("name: receiving-code-review"));
        assert!(!r.contains("GitHub Thread Replies"));
    }
}
