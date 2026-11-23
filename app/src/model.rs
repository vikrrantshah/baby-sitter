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

fn default_draft_timeout() -> Duration {
    Duration::from_secs(7200)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepoSettings {
    #[serde(with = "duration_serde")]
    pub timeout: Duration,
    #[serde(default = "default_draft_timeout", with = "duration_serde")]
    pub draft_timeout: Duration,
    #[serde(with = "duration_serde")]
    pub interval: Duration,
}
impl Default for RepoSettings {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(7200),
            draft_timeout: default_draft_timeout(),
            interval: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoScope {
    All,
    Watched,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepoListItem {
    Org {
        name: String,
        watched: usize,
        total: usize,
    },
    Repo(RepoName),
}

pub fn repo_list(
    repos: &[RepoName],
    selected: &BTreeMap<RepoName, RepoSettings>,
    org: Option<&str>,
    scope: RepoScope,
) -> Vec<RepoListItem> {
    let mut groups: BTreeMap<&str, Vec<(&RepoName, bool)>> = BTreeMap::new();
    for repo in repos {
        if org.is_some_and(|org| org != repo.owner) {
            continue;
        }
        let watched = selected.contains_key(repo);
        if scope == RepoScope::Watched && !watched {
            continue;
        }
        groups.entry(&repo.owner).or_default().push((repo, watched));
    }

    let mut groups: Vec<_> = groups.into_iter().collect();
    groups.sort_by_cached_key(|(owner, _)| owner.to_lowercase());
    let mut items = Vec::with_capacity(repos.len() + groups.len());
    for (owner, mut repos) in groups {
        repos.sort_by_cached_key(|(repo, _)| repo.name.to_lowercase());
        items.push(RepoListItem::Org {
            name: owner.to_owned(),
            watched: repos.iter().filter(|(_, watched)| *watched).count(),
            total: repos.len(),
        });
        items.extend(
            repos
                .into_iter()
                .map(|(repo, _)| RepoListItem::Repo(repo.clone())),
        );
    }
    items
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PastSit {
    pub title: String,
    pub outcome: Outcome,
    pub finished_at: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PrFilter {
    #[default]
    All,
    Watched,
    Unwatched,
    Merged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrStatus {
    Watched,
    Unwatched,
    Merged,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub repos: BTreeMap<RepoName, RepoSettings>,
    pub active: BTreeMap<PrRef, u64>,
    #[serde(default)]
    pub history: BTreeMap<PrRef, PastSit>,
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
    history: &'a BTreeMap<PrRef, PastSit>,
    sits: &'a BTreeMap<PrRef, SitState>,
    watched: &BTreeMap<RepoName, RepoSettings>,
    filter: PrFilter,
    repo: Option<&RepoName>,
) -> Vec<Cow<'a, PrRow>> {
    let matches_filter = |pr: &PrRef| {
        let matches_status = match filter {
            PrFilter::All => true,
            PrFilter::Watched => pr_status(sits.get(pr)) == PrStatus::Watched,
            PrFilter::Unwatched => pr_status(sits.get(pr)) == PrStatus::Unwatched,
            PrFilter::Merged => pr_status(sits.get(pr)) == PrStatus::Merged,
        };
        matches_status && repo.is_none_or(|repo| &pr.repo == repo)
    };
    let mut existing = BTreeSet::new();
    let mut rows: Vec<_> = prs
        .iter()
        .filter(|row| {
            (watched.contains_key(&row.pr.repo)
                || history.contains_key(&row.pr)
                || sits.contains_key(&row.pr))
                && matches_filter(&row.pr)
                && existing.insert(&row.pr)
        })
        .map(Cow::Borrowed)
        .collect();
    rows.extend(
        history
            .keys()
            .chain(sits.keys())
            .filter(|pr| matches_filter(pr) && existing.insert(*pr))
            .map(|pr| {
                Cow::Owned(PrRow {
                    pr: pr.clone(),
                    title: history.get(pr).map_or_else(
                        || format!("Pull request #{}", pr.number),
                        |past| past.title.clone(),
                    ),
                    url: pr.url(),
                    is_draft: false,
                    updated_at: String::new(),
                })
            }),
    );
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    rows
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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

pub fn pr_status(state: Option<&SitState>) -> PrStatus {
    match state {
        Some(state) if state.is_active() => PrStatus::Watched,
        Some(SitState::Finished(Outcome::Merged)) => PrStatus::Merged,
        _ => PrStatus::Unwatched,
    }
}

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
    pub kind: VerdictKind,
    pub detail: &'a str,
}
pub fn parse_pass(line: &str) -> Option<Pass<'_>> {
    let (_rule, verdict) = line.strip_prefix('[')?.split_once("] ")?;
    let (kind, detail) = if verdict == "merged" {
        (VerdictKind::Done, "")
    } else if let Some(reason) = verdict.strip_prefix("stop: ") {
        (VerdictKind::Stop, reason)
    } else if let Some(reason) = verdict.strip_prefix("wait: ") {
        (VerdictKind::Wait, reason)
    } else {
        (VerdictKind::Act, verdict)
    };
    Some(Pass { kind, detail })
}

/// A merge requirement's state, as the CLI reports it with `--gates`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateState {
    /// Merge only: other gates are still open.
    Pending,
    Met,
    Waiting,
    Blocked,
    Skipped,
}
/// Keys of the CLI's `--gates` line, in the order it reports them.
const GATE_KEYS: [&str; 4] = ["reviews", "checks", "branch", "merge"];
const GATES_PREFIX: &str = "[gates] ";
/// Parses `[gates] reviews=met checks=waiting branch=met merge=pending`. The keys
/// mirror `Gate` in the CLI's `src/decide.rs`.
pub fn parse_gates(line: &str) -> Option<[GateState; 4]> {
    let mut states = [None; 4];
    for pair in line.strip_prefix(GATES_PREFIX)?.split(' ') {
        let (key, value) = pair.split_once('=')?;
        let index = GATE_KEYS.iter().position(|name| *name == key)?;
        states[index] = Some(match value {
            "pending" => GateState::Pending,
            "met" => GateState::Met,
            "waiting" => GateState::Waiting,
            "blocked" => GateState::Blocked,
            "skipped" => GateState::Skipped,
            _ => return None,
        });
    }
    let [reviews, checks, branch, merge] = states;
    Some([reviews?, checks?, branch?, merge?])
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
            .filter(|line| !line.trim().is_empty() && !line.starts_with(GATES_PREFIX))
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

    fn repos(names: &[&str]) -> Vec<RepoName> {
        names.iter().map(|name| name.parse().unwrap()).collect()
    }

    #[test]
    fn repo_list_groups_and_orders_repositories_case_insensitively() {
        let catalog = repos(&["Zulu/zebra", "alpha/Zulu", "Zulu/Alpha", "alpha/aaa"]);
        let selected = BTreeMap::from([
            ("alpha/Zulu".parse().unwrap(), RepoSettings::default()),
            ("Zulu/Alpha".parse().unwrap(), RepoSettings::default()),
        ]);
        assert_eq!(
            repo_list(&catalog, &selected, None, RepoScope::All),
            vec![
                RepoListItem::Org {
                    name: "alpha".into(),
                    watched: 1,
                    total: 2,
                },
                RepoListItem::Repo("alpha/aaa".parse().unwrap()),
                RepoListItem::Repo("alpha/Zulu".parse().unwrap()),
                RepoListItem::Org {
                    name: "Zulu".into(),
                    watched: 1,
                    total: 2,
                },
                RepoListItem::Repo("Zulu/Alpha".parse().unwrap()),
                RepoListItem::Repo("Zulu/zebra".parse().unwrap()),
            ]
        );
    }

    #[test]
    fn repo_list_combines_organization_and_watched_filters() {
        let catalog = repos(&["alpha/one", "alpha/two", "beta/three"]);
        let selected = BTreeMap::from([
            ("alpha/two".parse().unwrap(), RepoSettings::default()),
            ("beta/three".parse().unwrap(), RepoSettings::default()),
        ]);
        assert_eq!(
            repo_list(&catalog, &selected, Some("alpha"), RepoScope::Watched),
            vec![
                RepoListItem::Org {
                    name: "alpha".into(),
                    watched: 1,
                    total: 1,
                },
                RepoListItem::Repo("alpha/two".parse().unwrap()),
            ]
        );
    }

    #[test]
    fn repo_list_watched_scope_is_empty_when_nothing_is_watched() {
        let catalog = repos(&["alpha/one", "beta/two"]);
        assert!(repo_list(&catalog, &BTreeMap::new(), None, RepoScope::Watched).is_empty());
    }

    #[test]
    fn dashboard_filters_candidates_by_status_and_repository() {
        let prs = [
            row("owner/one#1", "2026-01-01"),
            row("owner/two#2", "2026-01-03"),
            row("owner/one#3", "2026-01-02"),
        ];
        let history = BTreeMap::from([
            (
                "owner/one#7".parse().unwrap(),
                PastSit {
                    title: "Merged one".into(),
                    outcome: Outcome::Merged,
                    finished_at: 100,
                },
            ),
            (
                "owner/two#8".parse().unwrap(),
                PastSit {
                    title: "Merged two".into(),
                    outcome: Outcome::Merged,
                    finished_at: 101,
                },
            ),
            (
                "owner/two#9".parse().unwrap(),
                PastSit {
                    title: "Stopped".into(),
                    outcome: Outcome::Stopped("closed".into()),
                    finished_at: 102,
                },
            ),
            (
                "owner/three#10".parse().unwrap(),
                PastSit {
                    title: "Timed out".into(),
                    outcome: Outcome::TimedOut,
                    finished_at: 103,
                },
            ),
        ]);
        let sits = BTreeMap::from([
            (
                "owner/one#3".parse().unwrap(),
                SitState::Running { last_line: None },
            ),
            (
                "owner/one#7".parse().unwrap(),
                SitState::Finished(Outcome::Merged),
            ),
            (
                "owner/two#8".parse().unwrap(),
                SitState::Finished(Outcome::Merged),
            ),
            (
                "owner/two#9".parse().unwrap(),
                SitState::Finished(Outcome::Stopped("closed".into())),
            ),
            ("owner/one#11".parse().unwrap(), SitState::Preparing),
            ("owner/two#12".parse().unwrap(), SitState::Cancelling),
        ]);
        let watched = BTreeMap::from([("owner/one".parse().unwrap(), RepoSettings::default())]);
        for (filter, expected) in [
            (PrFilter::All, vec![1, 3, 7, 8, 9, 10, 11, 12]),
            (PrFilter::Watched, vec![3, 11, 12]),
            (PrFilter::Unwatched, vec![1, 9, 10]),
            (PrFilter::Merged, vec![7, 8]),
        ] {
            let mut numbers: Vec<_> = dashboard_rows(&prs, &history, &sits, &watched, filter, None)
                .iter()
                .map(|row| row.pr.number)
                .collect();
            numbers.sort_unstable();
            assert_eq!(numbers, expected, "{filter:?}");
        }
        let repo = "owner/one".parse().unwrap();
        let rows = dashboard_rows(&prs, &history, &sits, &watched, PrFilter::All, Some(&repo));
        assert_eq!(
            rows.iter().map(|row| row.pr.number).collect::<Vec<_>>(),
            [3, 1, 7, 11]
        );
        let rows = dashboard_rows(
            &prs,
            &history,
            &sits,
            &watched,
            PrFilter::Watched,
            Some(&repo),
        );
        assert_eq!(
            rows.iter().map(|row| row.pr.number).collect::<Vec<_>>(),
            [3, 11]
        );
        assert_eq!(PrFilter::default(), PrFilter::All);
    }

    #[test]
    fn pr_status_classifies_every_sit_state() {
        assert_eq!(pr_status(None), PrStatus::Unwatched);
        for state in [
            SitState::Preparing,
            SitState::Running { last_line: None },
            SitState::Cancelling,
        ] {
            assert_eq!(pr_status(Some(&state)), PrStatus::Watched);
        }
        assert_eq!(
            pr_status(Some(&SitState::Finished(Outcome::Merged))),
            PrStatus::Merged
        );
        for outcome in [
            Outcome::TimedOut,
            Outcome::Stopped("closed".into()),
            Outcome::Cancelled,
            Outcome::Failed("error".into()),
        ] {
            assert_eq!(
                pr_status(Some(&SitState::Finished(outcome))),
                PrStatus::Unwatched
            );
        }
    }

    #[test]
    fn dashboard_preserves_history_title_and_removes_dismissed_rows() {
        let pr: PrRef = "owner/one#7".parse().unwrap();
        let watched = BTreeMap::from([(pr.repo.clone(), RepoSettings::default())]);
        let mut history = BTreeMap::from([(
            pr.clone(),
            PastSit {
                title: "The original title".into(),
                outcome: Outcome::Merged,
                finished_at: 100,
            },
        )]);
        let mut sits = BTreeMap::from([(pr.clone(), SitState::Finished(Outcome::Merged))]);
        let rows = dashboard_rows(&[], &history, &sits, &watched, PrFilter::Merged, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pr, pr);
        assert_eq!(rows[0].title, "The original title");
        assert_eq!(rows[0].url, pr.url());

        history.remove(&pr);
        sits.remove(&pr);
        assert!(dashboard_rows(&[], &history, &sits, &watched, PrFilter::All, None).is_empty());
        let open = [row("owner/one#7", "2026-01-01")];
        let rows = dashboard_rows(&open, &history, &sits, &watched, PrFilter::Unwatched, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "PR 7");

        sits.insert(pr.clone(), SitState::Preparing);
        let rows = dashboard_rows(&[], &history, &sits, &watched, PrFilter::Watched, None);
        assert_eq!(rows[0].title, "Pull request #7");
    }

    #[test]
    fn dashboard_prefers_current_title_over_history() {
        let prs = [row("owner/one#1", "2026-01-01")];
        let pr = prs[0].pr.clone();
        let history = BTreeMap::from([(
            pr.clone(),
            PastSit {
                title: "Old title".into(),
                outcome: Outcome::Cancelled,
                finished_at: 100,
            },
        )]);
        let sits = BTreeMap::from([(pr, SitState::Finished(Outcome::Cancelled))]);
        let rows = dashboard_rows(&prs, &history, &sits, &BTreeMap::new(), PrFilter::All, None);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "PR 1");
    }

    #[test]
    fn failed_reason_keeps_the_last_five_nonempty_lines() {
        let stderr = format!(
            "{}\n\n[gates] reviews=met checks=met branch=met merge=waiting\npanic: broken\n\n",
            "progress\n".repeat(240)
        );
        let Outcome::Failed(reason) = classify(Some(101), &stderr, false) else {
            panic!("expected failure");
        };
        assert_eq!(reason.lines().count(), 5);
        assert!(reason.ends_with("progress\npanic: broken"));
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
    fn pass_lines_classify_verdicts() {
        assert_eq!(
            parse_pass("[ci-pending] wait: CI pending: build"),
            Some(Pass {
                kind: VerdictKind::Wait,
                detail: "CI pending: build"
            })
        );
        assert_eq!(
            parse_pass("[local-dirty] stop: local tree is dirty: a.rs"),
            Some(Pass {
                kind: VerdictKind::Stop,
                detail: "local tree is dirty: a.rs"
            })
        );
        assert_eq!(
            parse_pass("[enqueue] enqueue abc1234 (queue merges with squash)").map(|p| p.kind),
            Some(VerdictKind::Act)
        );
        assert_eq!(
            parse_pass("[merged] merged").map(|p| p.kind),
            Some(VerdictKind::Done)
        );
        assert_eq!(
            parse_pass("baby-sitter: warning: re-review request failed"),
            None
        );
    }
    #[test]
    fn gate_lines_need_every_gate_with_a_known_state() {
        assert_eq!(
            parse_gates("[gates] reviews=met checks=waiting branch=blocked merge=pending"),
            Some([
                GateState::Met,
                GateState::Waiting,
                GateState::Blocked,
                GateState::Pending
            ])
        );
        assert_eq!(
            parse_gates("[gates] reviews=met checks=met branch=met merge=skipped"),
            Some([
                GateState::Met,
                GateState::Met,
                GateState::Met,
                GateState::Skipped
            ])
        );
        assert_eq!(
            parse_gates("[gates] reviews=met checks=waiting branch=met"),
            None
        );
        assert_eq!(
            parse_gates("[gates] reviews=met checks=soon branch=met merge=met"),
            None
        );
        assert_eq!(parse_gates("[ci-pending] wait: CI pending: build"), None);
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
        config.active.insert(pr.clone(), 1_700_000_000);
        config.history.insert(
            pr,
            PastSit {
                title: "A pull request".into(),
                outcome: Outcome::Stopped("Needs review".into()),
                finished_at: 1_700_000_001,
            },
        );
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), config);
    }

    #[test]
    fn draft_timeout_defaults_and_round_trips_in_config() {
        let json = r#"{"repos":{"owner/repo":{"timeout":"1h","interval":"30s"}}}"#;
        let mut config: Config = serde_json::from_str(json).unwrap();
        let repo: RepoName = "owner/repo".parse().unwrap();
        assert_eq!(config.repos[&repo].draft_timeout, Duration::from_secs(7200));
        config.repos.get_mut(&repo).unwrap().draft_timeout = Duration::from_secs(28800);
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains(r#""draft_timeout":"8h""#));
        assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), config);
    }

    #[test]
    fn config_without_history_still_loads() {
        let config: Config = serde_json::from_str(
            r#"{"repos":{"owner/repo":{"timeout":"2h","interval":"30s"}},"active":{"owner/repo#42":1700000000}}"#,
        )
        .unwrap();
        assert!(config.history.is_empty());
        assert_eq!(config.repos.len(), 1);
        assert_eq!(config.active.len(), 1);
    }
}
