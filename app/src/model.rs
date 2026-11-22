use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RepoName {
    pub owner: String,
    pub name: String,
}

impl fmt::Display for RepoName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

impl FromStr for RepoName {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (owner, name) = s
            .split_once('/')
            .ok_or_else(|| format!("invalid repository: {s}"))?;
        if [owner, name]
            .iter()
            .any(|part| part.is_empty() || *part == "." || *part == ".." || part.contains('/'))
        {
            return Err(format!("invalid repository: {s}"));
        }
        Ok(Self {
            owner: owner.into(),
            name: name.into(),
        })
    }
}

impl Serialize for RepoName {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for RepoName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PrRef {
    pub repo: RepoName,
    pub number: u64,
}

impl PrRef {
    pub fn url(&self) -> String {
        format!("https://github.com/{}/pull/{}", self.repo, self.number)
    }
}
impl fmt::Display for PrRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}
impl FromStr for PrRef {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (repo, number) = s
            .rsplit_once('#')
            .ok_or_else(|| format!("invalid PR: {s}"))?;
        let number = number
            .parse::<u64>()
            .map_err(|_| format!("invalid PR: {s}"))?;
        if number == 0 {
            return Err(format!("invalid PR: {s}"));
        }
        Ok(Self {
            repo: repo.parse()?,
            number,
        })
    }
}
impl Serialize for PrRef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for PrRef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

mod duration_serde {
    use super::*;
    pub fn serialize<S: Serializer>(value: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format_duration(*value))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Duration, D::Error> {
        parse_duration(&String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepoSettings {
    #[serde(with = "duration_serde")]
    pub timeout: Duration,
    #[serde(with = "duration_serde")]
    pub interval: Duration,
}
impl Default for RepoSettings {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(7200),
            interval: Duration::from_secs(30),
        }
    }
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub repos: BTreeMap<RepoName, RepoSettings>,
    pub active: BTreeMap<PrRef, u64>,
}

#[derive(Clone, Debug)]
pub struct PrRow {
    pub pr: PrRef,
    pub title: String,
    pub url: String,
    pub is_draft: bool,
    pub updated_at: String,
}

pub fn dashboard_rows<'a>(
    prs: &'a [PrRow],
    sits: impl Iterator<Item = &'a PrRef>,
    selected: &BTreeMap<RepoName, RepoSettings>,
    filter: Option<&RepoName>,
) -> Vec<Cow<'a, PrRow>> {
    let existing: BTreeSet<_> = prs.iter().map(|row| &row.pr).collect();
    let mut rows: Vec<_> = prs.iter().map(Cow::Borrowed).collect();
    rows.extend(sits.filter(|pr| !existing.contains(*pr)).map(|pr| {
        Cow::Owned(PrRow {
            pr: pr.clone(),
            title: format!("Pull request #{}", pr.number),
            url: pr.url(),
            is_draft: false,
            updated_at: String::new(),
        })
    }));
    rows.retain(|row| {
        selected.contains_key(&row.pr.repo) && filter.is_none_or(|repo| &row.pr.repo == repo)
    });
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    rows
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Outcome {
    Merged,
    TimedOut,
    Stopped(String),
    Cancelled,
    Failed(String),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SitState {
    Preparing,
    Running { last_line: Option<String> },
    Cancelling,
    Finished(Outcome),
}
impl SitState {
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Finished(_))
    }
}

/// The CLI's decision checklist in precedence order, as shown by the progress meter.
pub const STEPS: [&str; 8] = [
    "PR state",
    "Merge queue",
    "Local checkout",
    "Fixed threads",
    "Review threads",
    "Base branch",
    "Merge gate",
    "Merge",
];
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerdictKind {
    Done,
    Stop,
    Wait,
    Act,
}
/// One `[rule] verdict` line the CLI prints per pass.
#[derive(Debug, Eq, PartialEq)]
pub struct Pass<'a> {
    /// Index into [`STEPS`], or `None` for a rule this app does not know.
    pub step: Option<usize>,
    pub kind: VerdictKind,
    pub detail: &'a str,
}
/// Rule names mirror `RULES` in the CLI's `src/decide.rs`.
pub fn parse_pass(line: &str) -> Option<Pass<'_>> {
    let (rule, verdict) = line.strip_prefix('[')?.split_once("] ")?;
    let step = match rule {
        "merged" | "closed" | "draft" | "conflict" | "ci-failed" => Some(0),
        "in-queue" | "dequeued" => Some(1),
        "local-upstream" | "local-dirty" | "local-diverged" | "local-behind" => Some(2),
        "resolve-fixed" => Some(3),
        "unhandled-items" => Some(4),
        "behind-base" => Some(5),
        "ci-pending" | "merge-state" | "unresolved-threads" | "review-requests"
        | "review-decision" => Some(6),
        "merge" | "enqueue" => Some(7),
        _ => None,
    };
    let (kind, detail) = if verdict == "merged" {
        (VerdictKind::Done, "")
    } else if let Some(reason) = verdict.strip_prefix("stop: ") {
        (VerdictKind::Stop, reason)
    } else if let Some(reason) = verdict.strip_prefix("wait: ") {
        (VerdictKind::Wait, reason)
    } else {
        (VerdictKind::Act, verdict)
    };
    Some(Pass { step, kind, detail })
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let split = s
        .len()
        .checked_sub(1)
        .ok_or_else(|| "empty duration".to_owned())?;
    if !s.is_char_boundary(split) {
        return Err(format!("invalid duration {s:?}"));
    }
    let (number, unit) = s.split_at(split);
    let number: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration {s:?}"))?;
    let factor = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(format!("invalid duration {s:?}; use s, m, or h")),
    };
    number
        .checked_mul(factor)
        .map(Duration::from_secs)
        .ok_or_else(|| "duration overflow".into())
}
pub fn format_duration(d: Duration) -> String {
    let seconds = d.as_secs();
    if seconds != 0 && seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds != 0 && seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn stop_reason(stderr: &str) -> Option<&str> {
    let mut offset = 0;
    for line in stderr.split_inclusive('\n') {
        if let Some(reason) = line.strip_prefix("baby-sitter: stopped: ") {
            let start = offset + line.len() - reason.len();
            return Some(stderr[start..].trim());
        }
        offset += line.len();
    }
    None
}
pub fn lock_held(code: Option<i32>, stderr: &str) -> bool {
    code == Some(1)
        && stop_reason(stderr)
            .is_some_and(|s| s.starts_with("another baby-sitter holds the PR lock"))
}
pub fn classify(code: Option<i32>, stderr: &str, cancel_requested: bool) -> Outcome {
    let tail = || {
        let mut lines: Vec<_> = stderr
            .lines()
            .rev()
            .filter(|line| !line.trim().is_empty())
            .take(5)
            .collect();
        if lines.is_empty() {
            "process exited without an error message".to_owned()
        } else {
            lines.reverse();
            lines.join("\n").trim().to_owned()
        }
    };
    match code {
        None if cancel_requested => Outcome::Cancelled,
        Some(0) => Outcome::Merged,
        Some(1) => stop_reason(stderr)
            .map(|s| Outcome::Stopped(s.to_owned()))
            .unwrap_or_else(|| Outcome::Failed(tail())),
        Some(2)
            if stderr.lines().rev().find(|line| !line.trim().is_empty())
                == Some("baby-sitter: timed out") =>
        {
            Outcome::TimedOut
        }
        _ => Outcome::Failed(tail()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(pr: &str, updated_at: &str) -> PrRow {
        let pr: PrRef = pr.parse().unwrap();
        PrRow {
            url: pr.url(),
            title: format!("PR {}", pr.number),
            pr,
            is_draft: false,
            updated_at: updated_at.into(),
        }
    }

    #[test]
    fn dashboard_excludes_all_rows_without_selected_repositories() {
        let prs = [row("owner/one#1", "2026-01-01")];
        let sits = BTreeMap::from([(
            "owner/two#2".parse().unwrap(),
            SitState::Finished(Outcome::Merged),
        )]);
        assert!(dashboard_rows(&prs, sits.keys(), &BTreeMap::new(), None).is_empty());
    }

    #[test]
    fn dashboard_only_shows_selected_repository_and_sorts_rows() {
        let prs = [
            row("owner/one#1", "2026-01-01"),
            row("owner/two#2", "2026-01-03"),
            row("owner/one#3", "2026-01-02"),
        ];
        let selected = BTreeMap::from([("owner/one".parse().unwrap(), RepoSettings::default())]);
        let rows = dashboard_rows(&prs, std::iter::empty(), &selected, None);
        assert_eq!(
            rows.iter().map(|row| row.pr.number).collect::<Vec<_>>(),
            [3, 1]
        );
        let other = "owner/two".parse().unwrap();
        assert!(dashboard_rows(&prs, std::iter::empty(), &selected, Some(&other)).is_empty());
    }

    #[test]
    fn dashboard_includes_missing_sit_for_selected_repository() {
        let pr: PrRef = "owner/one#7".parse().unwrap();
        let selected = BTreeMap::from([(pr.repo.clone(), RepoSettings::default())]);
        let rows = dashboard_rows(&[], std::iter::once(&pr), &selected, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pr, pr);
        assert_eq!(rows[0].title, "Pull request #7");
        assert_eq!(rows[0].url, pr.url());
    }

    #[test]
    fn failed_reason_keeps_the_last_five_nonempty_lines() {
        let stderr = format!("{}\n\npanic: broken\n\n", "progress\n".repeat(240));
        let Outcome::Failed(reason) = classify(Some(101), &stderr, false) else {
            panic!("expected failure");
        };
        assert_eq!(reason.lines().count(), 5);
        assert!(reason.ends_with("panic: broken"));
        assert_eq!(reason.lines().next(), Some("progress"));
    }
    #[test]
    fn duration_round_trips_and_rejects_invalid_values() {
        for text in ["0s", "1s", "90s", "2m", "2h"] {
            let parsed = parse_duration(text).unwrap();
            assert_eq!(parse_duration(&format_duration(parsed)).unwrap(), parsed);
        }
        for text in [
            "",
            "s",
            "10",
            "1h30m",
            "-1s",
            "2d",
            "1é",
            "18446744073709551615h",
        ] {
            assert!(parse_duration(text).is_err(), "{text}");
        }
    }
    #[test]
    fn process_exit_classification_preserves_first_multiline_stop() {
        assert_eq!(classify(Some(0), "", false), Outcome::Merged);
        assert_eq!(
            classify(
                Some(1),
                "progress\nbaby-sitter: stopped: first\ndetail\nbaby-sitter: stopped: second\n",
                false
            ),
            Outcome::Stopped("first\ndetail\nbaby-sitter: stopped: second".into())
        );
        assert_eq!(
            classify(Some(2), "progress\nbaby-sitter: timed out\n\n", false),
            Outcome::TimedOut
        );
        assert_eq!(
            classify(Some(2), "error: bad argument\n", false),
            Outcome::Failed("error: bad argument".into())
        );
        assert_eq!(
            classify(Some(101), "panic", false),
            Outcome::Failed("panic".into())
        );
        assert_eq!(classify(None, "", true), Outcome::Cancelled);
        assert_eq!(classify(Some(0), "", true), Outcome::Merged);
        assert_eq!(
            classify(Some(1), "baby-sitter: stopped: closed\n", true),
            Outcome::Stopped("closed".into())
        );
        assert_eq!(
            classify(Some(2), "error: bad argument\n", true),
            Outcome::Failed("error: bad argument".into())
        );
        assert_eq!(
            classify(Some(2), "baby-sitter: timed out\n", true),
            Outcome::TimedOut
        );
        assert!(lock_held(
            Some(1),
            "baby-sitter: stopped: another baby-sitter holds the PR lock: x\n"
        ));
        assert!(!lock_held(
            Some(1),
            "baby-sitter: stopped: something else\n"
        ));
    }
    #[test]
    fn pass_lines_map_rules_to_checklist_steps() {
        assert_eq!(
            parse_pass("[ci-pending] wait: CI pending: build"),
            Some(Pass {
                step: Some(6),
                kind: VerdictKind::Wait,
                detail: "CI pending: build"
            })
        );
        assert_eq!(
            parse_pass("[local-dirty] stop: local tree is dirty: a.rs").map(|p| (p.step, p.kind)),
            Some((Some(2), VerdictKind::Stop))
        );
        assert_eq!(
            parse_pass("[enqueue] enqueue abc1234 (queue merges with squash)")
                .map(|p| (p.step, p.kind)),
            Some((Some(7), VerdictKind::Act))
        );
        assert_eq!(
            parse_pass("[merged] merged").map(|p| (p.step, p.kind)),
            Some((Some(0), VerdictKind::Done))
        );
        assert_eq!(
            parse_pass("[new-rule] fix something").map(|p| (p.step, p.kind)),
            Some((None, VerdictKind::Act))
        );
        assert_eq!(
            parse_pass("baby-sitter: warning: re-review request failed"),
            None
        );
    }
    #[test]
    fn config_round_trip_and_pr_url() {
        let repo: RepoName = "owner/repo".parse().unwrap();
        let pr = PrRef {
            repo: repo.clone(),
            number: 42,
        };
        assert_eq!(pr.url(), "https://github.com/owner/repo/pull/42");
        let mut config = Config::default();
        config.repos.insert(repo, RepoSettings::default());
        config.active.insert(pr, 1_700_000_000);
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), config);
    }
}
