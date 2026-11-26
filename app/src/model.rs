use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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

mod optional_duration_serde {
    use super::*;

    pub fn serialize<S: Serializer>(
        value: &Option<Duration>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        match value {
            Some(duration) => duration_serde::serialize(duration, serializer),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Option<Duration>, D::Error> {
        Option::<String>::deserialize(deserializer)?
            .map(|value| parse_duration(&value).map_err(de::Error::custom))
            .transpose()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentKind {
    #[default]
    Codex,
    Claude,
    Pi,
    Omp,
}

impl AgentKind {
    pub const ALL: [Self; 4] = [Self::Codex, Self::Claude, Self::Pi, Self::Omp];

    pub fn arg(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::Omp => "omp",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::Pi => "pi",
            Self::Omp => "omp",
        }
    }

    pub fn logo(self) -> &'static str {
        match self {
            Self::Codex => "logos/codex.svg",
            Self::Claude => "logos/claude.svg",
            Self::Pi => "logos/pi.svg",
            Self::Omp => "logos/omp.svg",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Defaults {
    pub agent: AgentKind,
    pub model: Option<String>,
    #[serde(with = "duration_serde")]
    pub timeout: Duration,
    #[serde(with = "duration_serde")]
    pub draft_timeout: Duration,
    #[serde(with = "duration_serde")]
    pub interval: Duration,
}

impl Default for Defaults {
    fn default() -> Self {
        Self {
            agent: AgentKind::default(),
            model: None,
            timeout: Duration::from_secs(7200),
            draft_timeout: Duration::from_secs(7200),
            interval: Duration::from_secs(30),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub defaults: Defaults,
    pub auto_dismiss_merged: bool,
    #[serde(default, with = "duration_serde")]
    pub clear_merged_after: Duration,
}

impl AppSettings {
    pub fn hides_merged_filter(&self) -> bool {
        self.auto_dismiss_merged && self.clear_merged_after.is_zero()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepoSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(
        default,
        with = "optional_duration_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub timeout: Option<Duration>,
    #[serde(
        default,
        with = "optional_duration_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub draft_timeout: Option<Duration>,
    #[serde(
        default,
        with = "optional_duration_serde",
        skip_serializing_if = "Option::is_none"
    )]
    pub interval: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_command: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Resolved {
    pub agent: AgentKind,
    pub model: Option<String>,
    pub timeout: Duration,
    pub draft_timeout: Duration,
    pub interval: Duration,
    pub setup_command: Option<String>,
}

impl RepoSettings {
    pub fn resolve(&self, defaults: &Defaults) -> Resolved {
        Resolved {
            agent: self.agent.unwrap_or(defaults.agent),
            model: self.model.clone().or_else(|| {
                self.agent
                    .is_none()
                    .then(|| defaults.model.clone())
                    .flatten()
            }),
            timeout: self.timeout.unwrap_or(defaults.timeout),
            draft_timeout: self.draft_timeout.unwrap_or(defaults.draft_timeout),
            interval: self.interval.unwrap_or(defaults.interval),
            setup_command: self.setup_command.clone(),
        }
    }
}

pub fn normalize_setup_command(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepoScope {
    All,
    Watched,
}

impl RepoScope {
    pub const ALL: [Self; 2] = [Self::All, Self::Watched];
}

pub fn repo_counts(
    repos: &[RepoName],
    selected: &BTreeMap<RepoName, RepoSettings>,
    org: Option<&str>,
) -> [(RepoScope, usize); 2] {
    let mut counts = [(RepoScope::All, 0), (RepoScope::Watched, 0)];
    for repo in repos
        .iter()
        .filter(|repo| org.is_none_or(|org| org == repo.owner))
    {
        counts[0].1 += 1;
        counts[1].1 += usize::from(selected.contains_key(repo));
    }
    counts
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

pub fn merged_due(
    history: &BTreeMap<PrRef, PastSit>,
    settings: &AppSettings,
    now: u64,
) -> Vec<PrRef> {
    if !settings.auto_dismiss_merged {
        return Vec::new();
    }
    history
        .iter()
        .filter(|(_, past)| {
            past.outcome == Outcome::Merged
                && past
                    .finished_at
                    .checked_add(settings.clear_merged_after.as_secs())
                    .is_some_and(|deadline| deadline <= now)
        })
        .map(|(pr, _)| pr.clone())
        .collect()
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum WatchFilter {
    #[default]
    All,
    Watched,
    Unwatched,
}

impl WatchFilter {
    pub const ALL: [Self; 3] = [Self::All, Self::Watched, Self::Unwatched];

    pub fn admits(self, watched: bool) -> bool {
        match self {
            Self::All => true,
            Self::Watched => watched,
            Self::Unwatched => !watched,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub enum StateFilter {
    #[default]
    All,
    Open,
    Drafts,
    Merged,
}

impl StateFilter {
    pub const ALL: [Self; 4] = [Self::All, Self::Open, Self::Drafts, Self::Merged];

    pub fn visible(hides_merged: bool) -> &'static [Self] {
        if hides_merged {
            &Self::ALL[..3]
        } else {
            &Self::ALL
        }
    }

    pub fn admits(self, row: &PrRow, sit: Option<&SitState>) -> bool {
        let merged = row.state == PrState::Merged
            || matches!(sit, Some(SitState::Finished(Outcome::Merged)));
        match self {
            Self::All => true,
            Self::Open => matches!(row.state, PrState::Ready | PrState::Queued),
            Self::Drafts => row.state == PrState::Draft,
            Self::Merged => merged,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomPrompt {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl CustomPrompt {
    pub fn in_effect(&self) -> Option<&str> {
        self.text
            .as_deref()
            .filter(|text| self.enabled && !text.trim().is_empty())
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Prompts {
    pub review: CustomPrompt,
    pub ci: CustomPrompt,
    pub conflicts: CustomPrompt,
}

impl Prompts {
    pub fn render_json(&self) -> serde_json::Result<String> {
        #[derive(Serialize)]
        struct Overrides<'a> {
            #[serde(skip_serializing_if = "Option::is_none")]
            review: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            ci: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            conflicts: Option<&'a str>,
        }
        serde_json::to_string_pretty(&Overrides {
            review: self.review.in_effect(),
            ci: self.ci.in_effect(),
            conflicts: self.conflicts.in_effect(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.review.in_effect().is_none()
            && self.ci.in_effect().is_none()
            && self.conflicts.in_effect().is_none()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StackMode {
    #[default]
    BottomUp,
    TopDown,
}

impl StackMode {
    pub fn arg(self) -> &'static str {
        match self {
            Self::BottomUp => "bottom-up",
            Self::TopDown => "top-down",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveSit {
    pub deadline: u64,
    pub stack: StackMode,
}

impl Serialize for ActiveSit {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Object {
            deadline: u64,
            stack: StackMode,
        }
        match self.stack {
            StackMode::BottomUp => self.deadline.serialize(serializer),
            StackMode::TopDown => Object {
                deadline: self.deadline,
                stack: self.stack,
            }
            .serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ActiveSit {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Deadline(u64),
            Object {
                deadline: u64,
                #[serde(default)]
                stack: StackMode,
            },
        }
        Ok(match Raw::deserialize(deserializer)? {
            Raw::Deadline(deadline) => Self {
                deadline,
                stack: StackMode::BottomUp,
            },
            Raw::Object { deadline, stack } => Self { deadline, stack },
        })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default)]
    pub app: AppSettings,
    #[serde(default)]
    pub prompts: Prompts,
    pub repos: BTreeMap<RepoName, RepoSettings>,
    pub active: BTreeMap<PrRef, ActiveSit>,
    #[serde(default)]
    pub history: BTreeMap<PrRef, PastSit>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrState {
    Draft,
    Ready,
    Queued,
    Merged,
    Closed,
}

impl PrState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Draft => "Draft",
            Self::Ready => "Ready",
            Self::Queued => "In queue",
            Self::Merged => "Merged",
            Self::Closed => "Closed",
        }
    }
}

#[derive(Clone, Debug)]
pub struct PrRow {
    pub pr: PrRef,
    pub title: String,
    pub url: String,
    pub state: PrState,
    pub base: String,
    pub head: String,
    pub updated_at: String,
}

pub fn stack_parents(rows: &[PrRow]) -> BTreeMap<PrRef, PrRef> {
    let mut parents = BTreeMap::new();
    for child in rows {
        if child.base.is_empty() {
            continue;
        }
        if let Some(parent) = rows
            .iter()
            .filter(|parent| {
                parent.pr != child.pr
                    && parent.pr.repo == child.pr.repo
                    && parent.head == child.base
                    && matches!(
                        parent.state,
                        PrState::Ready | PrState::Draft | PrState::Queued
                    )
            })
            .min_by_key(|parent| parent.pr.number)
        {
            parents.insert(child.pr.clone(), parent.pr.clone());
        }
    }
    parents
}

pub fn stack_of(pr: &PrRef, parents: &BTreeMap<PrRef, PrRef>) -> Vec<PrRef> {
    let mut bottom = pr;
    let mut seen = BTreeSet::new();
    while seen.insert(bottom.clone()) {
        match parents.get(bottom) {
            Some(parent) if !seen.contains(parent) => bottom = parent,
            _ => break,
        }
    }
    let mut stack = Vec::new();
    seen.clear();
    while seen.insert(bottom.clone()) {
        stack.push(bottom.clone());
        match parents.iter().find(|(_, parent)| *parent == bottom) {
            Some((child, _)) if !seen.contains(child) => bottom = child,
            _ => break,
        }
    }
    stack
}

pub fn stack_tree(pr: &PrRef, parents: &BTreeMap<PrRef, PrRef>) -> Vec<PrRef> {
    let mut bottom = pr;
    let mut seen = BTreeSet::new();
    while seen.insert(bottom.clone()) {
        match parents.get(bottom) {
            Some(parent) if !seen.contains(parent) => bottom = parent,
            _ => break,
        }
    }
    let mut tree = vec![bottom.clone()];
    seen.clear();
    seen.insert(bottom.clone());
    let mut index = 0;
    while index < tree.len() {
        for (child, parent) in parents {
            if *parent == tree[index] && seen.insert(child.clone()) {
                tree.push(child.clone());
            }
        }
        index += 1;
    }
    tree
}

pub fn watched_prs(
    rows: &[Cow<'_, PrRow>],
    sits: &BTreeMap<PrRef, SitState>,
    parents: &BTreeMap<PrRef, PrRef>,
) -> BTreeSet<PrRef> {
    rows.iter()
        .filter(|row| {
            stack_tree(&row.pr, parents).iter().any(|pr| {
                sits.get(pr).is_some_and(|sit| {
                    sit.is_active() || matches!(sit, SitState::Finished(Outcome::Merged))
                })
            })
        })
        .map(|row| row.pr.clone())
        .collect()
}

pub fn stack_mode_of(
    pr: &PrRef,
    parents: &BTreeMap<PrRef, PrRef>,
    active: &BTreeMap<PrRef, ActiveSit>,
) -> Option<StackMode> {
    stack_tree(pr, parents)
        .iter()
        .find_map(|member| active.get(member).map(|sit| sit.stack))
}

pub fn order_stacks<'a>(
    rows: Vec<Cow<'a, PrRow>>,
    parents: &BTreeMap<PrRef, PrRef>,
) -> Vec<Cow<'a, PrRow>> {
    let indices: BTreeMap<_, _> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| (row.pr.clone(), i))
        .collect();
    let mut children: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (index, row) in rows.iter().enumerate() {
        if let Some(parent) = parents.get(&row.pr).and_then(|parent| indices.get(parent)) {
            children.entry(*parent).or_default().push(index);
        }
    }
    let mut pending: Vec<_> = rows.into_iter().map(Some).collect();
    let mut ordered = Vec::with_capacity(pending.len());
    let mut roots: Vec<_> = (0..pending.len())
        .filter(|i| {
            let row = pending[*i].as_ref().unwrap();
            !parents
                .get(&row.pr)
                .is_some_and(|parent| indices.contains_key(parent))
        })
        .collect();
    roots.extend(0..pending.len());
    for root in roots {
        let mut visit = vec![root];
        while let Some(index) = visit.pop() {
            if let Some(row) = pending[index].take() {
                ordered.push(row);
                if let Some(children) = children.get(&index) {
                    visit.extend(children.iter().rev().copied());
                }
            }
        }
    }
    ordered
}

/// Records which neighbouring rows a row's stack line reaches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StackRail {
    pub up: bool,
    pub down: bool,
}

/// Joins neighbouring rows that belong to the same stack, keyed by the PR at its bottom.
pub fn stack_rails(rows: &[Cow<'_, PrRow>], parents: &BTreeMap<PrRef, PrRef>) -> Vec<StackRail> {
    let stacked =
        |pr: &PrRef| parents.contains_key(pr) || parents.values().any(|parent| parent == pr);
    let keys: Vec<_> = rows
        .iter()
        .map(|row| stacked(&row.pr).then(|| stack_of(&row.pr, parents).swap_remove(0)))
        .collect();
    let joined = |a: usize, b: usize| keys[a].is_some() && keys[a] == keys[b];
    (0..rows.len())
        .map(|i| StackRail {
            up: i > 0 && joined(i, i - 1),
            down: i + 1 < rows.len() && joined(i, i + 1),
        })
        .collect()
}

pub fn dashboard_rows<'a>(
    prs: &'a [PrRow],
    history: &'a BTreeMap<PrRef, PastSit>,
    sits: &'a BTreeMap<PrRef, SitState>,
    watched: &BTreeMap<RepoName, RepoSettings>,
    repo: Option<&RepoName>,
) -> Vec<Cow<'a, PrRow>> {
    let matches_filter = |pr: &PrRef| repo.is_none_or(|repo| &pr.repo == repo);
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
                    state: if history
                        .get(pr)
                        .is_some_and(|past| past.outcome == Outcome::Merged)
                        || matches!(sits.get(pr), Some(SitState::Finished(Outcome::Merged)))
                    {
                        PrState::Merged
                    } else {
                        PrState::Ready
                    },
                    base: String::new(),
                    head: String::new(),
                    updated_at: String::new(),
                })
            }),
    );
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    rows
}

/// Digits, with or without a leading `#`, match PR numbers that start with them.
/// Any query also matches titles, ignoring case.
pub fn matches_search(row: &PrRow, query: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    let digits = query.strip_prefix('#').unwrap_or(query);
    let by_number = !digits.is_empty()
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && row.pr.number.to_string().starts_with(digits);
    by_number || row.title.to_lowercase().contains(&query.to_lowercase())
}

pub fn watch_counts(
    rows: &[Cow<'_, PrRow>],
    watched: &BTreeSet<PrRef>,
) -> [(WatchFilter, usize); 3] {
    WatchFilter::ALL.map(|filter| {
        let count = rows
            .iter()
            .filter(|row| filter.admits(watched.contains(&row.pr)))
            .count();
        (filter, count)
    })
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
    Running {
        last_line: Option<String>,
        since: Instant,
    },
    Cancelling,
    Finished(Outcome),
}
impl SitState {
    pub fn is_active(&self) -> bool {
        !matches!(self, Self::Finished(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerdictKind {
    Done,
    Stop,
    Wait,
    Act,
}
/// Holds one `[rule] verdict` line the CLI prints per pass.
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
    /// The merge gate is pending while other gates are still open.
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
    #[test]
    fn prompt_json_contains_only_active_overrides() {
        let prompts = super::Prompts {
            review: super::CustomPrompt {
                enabled: true,
                text: Some("review".into()),
            },
            ci: super::CustomPrompt {
                enabled: false,
                text: Some("disabled".into()),
            },
            conflicts: super::CustomPrompt {
                enabled: true,
                text: Some("  ".into()),
            },
        };
        let json: serde_json::Value =
            serde_json::from_str(&prompts.render_json().unwrap()).unwrap();
        assert_eq!(json, serde_json::json!({"review": "review"}));
    }

    #[test]
    fn prompt_requires_enabled_and_nonblank_saved_text() {
        for (enabled, text, expected) in [
            (false, None, None),
            (true, None, None),
            (false, Some("saved"), None),
            (true, Some("saved"), Some("saved")),
            (true, Some(" \n"), None),
        ] {
            let prompt = super::CustomPrompt {
                enabled,
                text: text.map(str::to_owned),
            };
            assert_eq!(prompt.in_effect(), expected);
        }
    }

    #[test]
    fn merged_retention_respects_delay_and_outcome() {
        let recent: PrRef = "owner/repo#1".parse().unwrap();
        let old: PrRef = "owner/repo#2".parse().unwrap();
        let other: PrRef = "owner/repo#3".parse().unwrap();
        let history = BTreeMap::from([
            (
                recent.clone(),
                PastSit {
                    title: String::new(),
                    outcome: Outcome::Merged,
                    finished_at: 9_400,
                },
            ),
            (
                old.clone(),
                PastSit {
                    title: String::new(),
                    outcome: Outcome::Merged,
                    finished_at: 2_800,
                },
            ),
            (
                other,
                PastSit {
                    title: String::new(),
                    outcome: Outcome::Cancelled,
                    finished_at: 1,
                },
            ),
        ]);
        let mut settings = AppSettings {
            auto_dismiss_merged: true,
            ..Default::default()
        };
        assert_eq!(
            merged_due(&history, &settings, 9_400),
            vec![recent.clone(), old.clone()]
        );
        settings.clear_merged_after = Duration::from_secs(3600);
        assert_eq!(merged_due(&history, &settings, 10_000), vec![old]);
        settings.auto_dismiss_merged = false;
        assert!(merged_due(&history, &settings, 10_000).is_empty());
    }

    #[test]
    fn merged_filter_hides_only_for_immediate_auto_dismiss() {
        for enabled in [false, true] {
            for seconds in [0, 3600] {
                let settings = AppSettings {
                    auto_dismiss_merged: enabled,
                    clear_merged_after: Duration::from_secs(seconds),
                    ..Default::default()
                };
                assert_eq!(settings.hides_merged_filter(), enabled && seconds == 0);
            }
        }
    }

    #[test]
    fn merged_retention_settings_round_trip_and_default() {
        let settings = AppSettings {
            auto_dismiss_merged: true,
            clear_merged_after: Duration::from_secs(3600),
            ..Default::default()
        };
        let json = serde_json::to_string(&settings).unwrap();
        assert_eq!(
            serde_json::from_str::<AppSettings>(&json).unwrap(),
            settings
        );
        let old: Config = serde_json::from_str(r#"{"app":{"auto_dismiss_merged":true}}"#).unwrap();
        assert_eq!(old.app.clear_merged_after, Duration::ZERO);
    }
    use super::*;
    fn row(pr: &str, updated_at: &str) -> PrRow {
        let pr: PrRef = pr.parse().unwrap();
        PrRow {
            url: pr.url(),
            title: format!("PR {}", pr.number),
            pr,
            state: PrState::Ready,
            base: String::new(),
            head: String::new(),
            updated_at: updated_at.into(),
        }
    }

    fn stack_rows() -> Vec<PrRow> {
        [
            ("arbio/platform#6512", "main", "one"),
            ("arbio/platform#6513", "one", "two"),
            ("arbio/platform#6381", "two", "three"),
            ("arbio/platform#6386", "three", "four"),
            ("arbio/platform#7000", "main", "other"),
            ("elsewhere/platform#8000", "one", "foreign"),
        ]
        .into_iter()
        .map(|(pr, base, head)| {
            let mut row = row(pr, "");
            row.base = base.into();
            row.head = head.into();
            row
        })
        .collect()
    }

    #[test]
    fn watched_filter_includes_the_entire_stack() {
        let prs = stack_rows();
        let parents = stack_parents(&prs);
        let sits = BTreeMap::from([(prs[1].pr.clone(), SitState::Preparing)]);
        let rows: Vec<_> = prs.iter().map(Cow::Borrowed).collect();
        let watched = watched_prs(&rows, &sits, &parents);
        let admitted = |filter: WatchFilter| {
            rows.iter()
                .filter(|row| filter.admits(watched.contains(&row.pr)))
                .map(|row| row.pr.clone())
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            admitted(WatchFilter::Watched),
            prs[..4].iter().map(|row| row.pr.clone()).collect()
        );
        assert_eq!(
            admitted(WatchFilter::Unwatched),
            prs[4..].iter().map(|row| row.pr.clone()).collect()
        );
        let counts = watch_counts(&rows, &watched);
        assert_eq!(
            counts,
            [
                (WatchFilter::All, 6),
                (WatchFilter::Watched, 4),
                (WatchFilter::Unwatched, 2)
            ]
        );
        assert_eq!(counts[0].1, counts[1].1 + counts[2].1);
    }

    #[test]
    fn stack_links_are_repo_scoped_and_traverse_from_every_member() {
        let rows = stack_rows();
        let parents = stack_parents(&rows);
        let chain: Vec<_> = rows[..4].iter().map(|row| row.pr.clone()).collect();
        assert_eq!(parents.len(), 3);
        for pair in chain.windows(2) {
            assert_eq!(parents.get(&pair[1]), Some(&pair[0]));
        }
        assert!(!parents.contains_key(&rows[5].pr));
        for pr in &chain {
            assert_eq!(stack_of(pr, &parents), chain);
        }
        let mut closed = rows.clone();
        closed[1].state = PrState::Closed;
        assert!(!stack_parents(&closed).contains_key(&closed[2].pr));
    }

    #[test]
    fn stack_tree_and_mode_include_side_branches() {
        let mut rows = stack_rows();
        let mut sibling = row("arbio/platform#7001", "");
        sibling.base = "one".into();
        sibling.head = "sibling".into();
        rows.push(sibling);
        let parents = stack_parents(&rows);
        let expected = vec![
            rows[0].pr.clone(),
            rows[1].pr.clone(),
            rows[6].pr.clone(),
            rows[2].pr.clone(),
            rows[3].pr.clone(),
        ];
        for pr in &expected {
            assert_eq!(stack_tree(pr, &parents), expected);
        }
        let mut active = BTreeMap::new();
        assert_eq!(stack_mode_of(&rows[3].pr, &parents, &active), None);
        active.insert(
            rows[6].pr.clone(),
            ActiveSit {
                deadline: 42,
                stack: StackMode::TopDown,
            },
        );
        assert_eq!(
            stack_mode_of(&rows[3].pr, &parents, &active),
            Some(StackMode::TopDown)
        );
        assert_eq!(stack_mode_of(&rows[4].pr, &parents, &active), None);
    }

    #[test]
    fn active_sit_object_defaults_mode_and_rejects_unknown_mode() {
        let sit: ActiveSit = serde_json::from_str(r#"{"deadline":123}"#).unwrap();
        assert_eq!(
            sit,
            ActiveSit {
                deadline: 123,
                stack: StackMode::BottomUp
            }
        );
        assert_eq!(serde_json::to_string(&sit).unwrap(), "123");
        assert!(
            serde_json::from_str::<ActiveSit>(r#"{"deadline":123,"stack":"sideways"}"#).is_err()
        );
    }

    #[test]
    fn stack_order_keeps_anchors_stable_and_children_adjacent() {
        let rows = stack_rows();
        let parents = stack_parents(&rows);
        let input = [3, 4, 1, 0, 2, 5]
            .into_iter()
            .map(|i| Cow::Borrowed(&rows[i]))
            .collect();
        let ordered = order_stacks(input, &parents);
        let positions: BTreeMap<_, _> = ordered
            .iter()
            .enumerate()
            .map(|(i, row)| (&row.pr, i))
            .collect();
        assert_eq!(ordered.len(), rows.len());
        assert_eq!(positions.len(), rows.len());
        for (child, parent) in &parents {
            assert_eq!(positions[child], positions[parent] + 1);
        }
        assert!(positions[&rows[4].pr] < positions[&rows[0].pr]);
        assert!(positions[&rows[0].pr] < positions[&rows[5].pr]);
        let filtered = order_stacks(
            vec![
                Cow::Borrowed(&rows[2]),
                Cow::Borrowed(&rows[4]),
                Cow::Borrowed(&rows[3]),
            ],
            &parents,
        );
        assert_eq!(
            filtered.iter().map(|row| row.pr.number).collect::<Vec<_>>(),
            [6381, 6386, 7000]
        );
    }

    #[test]
    fn search_matches_number_prefix_or_title_ignoring_case() {
        let mut pr = row("arbio/platform#6781", "");
        pr.title = "chore(infra): NX-4585 disarm the production one-off".into();
        for query in [
            "",
            "  ",
            "6781",
            "#678",
            "678",
            "nx-4585",
            "DISARM the",
            " production ",
        ] {
            assert!(matches_search(&pr, query), "{query:?} should match");
        }
        for query in ["781", "#", "#x", "6782", "arm the dev"] {
            assert!(!matches_search(&pr, query), "{query:?} should not match");
        }
    }

    #[test]
    fn stack_rails_join_neighbours_in_the_same_stack_only() {
        let rows = stack_rows();
        let parents = stack_parents(&rows);
        let ordered = order_stacks(rows.iter().map(Cow::Borrowed).collect(), &parents);
        let rails = stack_rails(&ordered, &parents);
        let chain: Vec<_> = rows[..4].iter().map(|row| &row.pr).collect();
        for (row, rail) in ordered.iter().zip(&rails) {
            let expected = match chain.iter().position(|pr| **pr == row.pr) {
                Some(0) => StackRail {
                    up: false,
                    down: true,
                },
                Some(3) => StackRail {
                    up: true,
                    down: false,
                },
                Some(_) => StackRail {
                    up: true,
                    down: true,
                },
                None => StackRail::default(),
            };
            assert_eq!(*rail, expected, "{}", row.pr);
        }
        let filtered = vec![Cow::Borrowed(&rows[1]), Cow::Borrowed(&rows[3])];
        assert_eq!(
            stack_rails(&filtered, &parents),
            [
                StackRail {
                    up: false,
                    down: true
                },
                StackRail {
                    up: true,
                    down: false
                }
            ],
            "a filtered-out middle PR still leaves its neighbours joined"
        );
    }

    fn repos(names: &[&str]) -> Vec<RepoName> {
        names.iter().map(|name| name.parse().unwrap()).collect()
    }
    #[test]
    fn repository_settings_resolve_each_inherited_or_overridden_duration() {
        let defaults = Defaults {
            timeout: Duration::from_secs(3600),
            draft_timeout: Duration::from_secs(7200),
            interval: Duration::from_secs(45),
            ..Defaults::default()
        };
        let settings = RepoSettings {
            timeout: Some(Duration::from_secs(1800)),
            draft_timeout: None,
            interval: Some(Duration::from_secs(10)),
            setup_command: Some("pnpm install".into()),
            ..RepoSettings::default()
        };
        assert_eq!(
            settings.resolve(&defaults),
            Resolved {
                timeout: Duration::from_secs(1800),
                draft_timeout: Duration::from_secs(7200),
                interval: Duration::from_secs(10),
                agent: AgentKind::Codex,
                model: None,
                setup_command: Some("pnpm install".into()),
            }
        );
        assert_eq!(
            RepoSettings {
                draft_timeout: Some(Duration::from_secs(14400)),
                ..RepoSettings::default()
            }
            .resolve(&defaults),
            Resolved {
                timeout: Duration::from_secs(3600),
                draft_timeout: Duration::from_secs(14400),
                interval: Duration::from_secs(45),
                agent: AgentKind::Codex,
                model: None,
                setup_command: None,
            }
        );
        assert_eq!(
            RepoSettings::default().resolve(&defaults),
            Resolved {
                timeout: Duration::from_secs(3600),
                draft_timeout: Duration::from_secs(7200),
                interval: Duration::from_secs(45),
                agent: AgentKind::Codex,
                model: None,
                setup_command: None,
            }
        );
    }

    #[test]
    fn agent_and_model_inherit_only_for_the_same_default_agent() {
        let config: Config = serde_json::from_str(r#"{"app":{"defaults":{"timeout":"1h","interval":"30s"}},"repos":{"owner/repo":{"timeout":"30m"}}}"#).unwrap();
        let old = config.app.defaults;
        assert_eq!(
            config
                .repos
                .get(&"owner/repo".parse().unwrap())
                .unwrap()
                .agent,
            None
        );
        assert_eq!(old.agent, AgentKind::Codex);
        assert_eq!(old.model, None);
        let defaults = Defaults {
            agent: AgentKind::Claude,
            model: Some("default-model".into()),
            ..old
        };
        let mut repo = RepoSettings::default();
        assert_eq!(
            repo.resolve(&defaults).model.as_deref(),
            Some("default-model")
        );
        repo.agent = Some(AgentKind::Pi);
        assert_eq!(repo.resolve(&defaults).agent, AgentKind::Pi);
        assert_eq!(repo.resolve(&defaults).model, None);
        repo.model = Some("repo-model".into());
        assert_eq!(repo.resolve(&defaults).model.as_deref(), Some("repo-model"));
        repo.agent = None;
        assert_eq!(repo.resolve(&defaults).model.as_deref(), Some("repo-model"));
    }

    #[test]
    fn repository_settings_omit_inherited_fields_when_serialized() {
        let settings = RepoSettings {
            timeout: None,
            draft_timeout: Some(Duration::from_secs(5400)),
            interval: None,
            setup_command: None,
            ..RepoSettings::default()
        };
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json, serde_json::json!({ "draft_timeout": "90m" }));
        assert_eq!(
            serde_json::from_value::<RepoSettings>(json).unwrap(),
            settings
        );
        assert_eq!(
            serde_json::to_value(RepoSettings::default()).unwrap(),
            serde_json::json!({})
        );
        assert_eq!(
            serde_json::from_str::<RepoSettings>("{}").unwrap(),
            RepoSettings::default()
        );
    }

    #[test]
    fn setup_command_loads_old_configs_and_omits_blank_input() {
        let mut settings: RepoSettings =
            serde_json::from_str(r#"{"timeout":"1h","interval":"30s"}"#).unwrap();
        assert_eq!(settings.setup_command, None);
        assert_eq!(settings.resolve(&Defaults::default()).setup_command, None);

        settings.setup_command = normalize_setup_command("  pnpm install  ").map(str::to_owned);
        let json = serde_json::to_value(&settings).unwrap();
        assert_eq!(json["setup_command"], "pnpm install");
        assert_eq!(
            serde_json::from_value::<RepoSettings>(json).unwrap(),
            settings
        );

        settings.setup_command = normalize_setup_command(" \t ").map(str::to_owned);
        let json = serde_json::to_value(&settings).unwrap();
        assert!(json.get("setup_command").is_none());
    }

    #[test]
    fn merged_filter_visibility_follows_settings() {
        assert_eq!(StateFilter::visible(false), &StateFilter::ALL);
        assert_eq!(
            StateFilter::visible(true),
            &[StateFilter::All, StateFilter::Open, StateFilter::Drafts]
        );
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
    fn repo_counts_follow_organization_filter() {
        let catalog = repos(&["alpha/one", "alpha/two", "beta/three"]);
        let selected = BTreeMap::from([
            ("alpha/one".parse().unwrap(), RepoSettings::default()),
            ("beta/three".parse().unwrap(), RepoSettings::default()),
        ]);
        assert_eq!(
            repo_counts(&catalog, &selected, Some("alpha")),
            [(RepoScope::All, 2), (RepoScope::Watched, 1)]
        );
        assert_eq!(
            repo_counts(&catalog, &selected, Some("missing")),
            [(RepoScope::All, 0), (RepoScope::Watched, 0)]
        );
    }

    #[test]
    fn watch_counts_ignore_the_state_filter_but_follow_the_repository() {
        let mut draft_watched = row("owner/one#1", "2026-01-01");
        draft_watched.state = PrState::Draft;
        let mut draft_unwatched = row("owner/one#2", "2026-01-02");
        draft_unwatched.state = PrState::Draft;
        let prs = [
            draft_watched,
            draft_unwatched,
            row("owner/one#3", "2026-01-03"),
            row("owner/two#4", "2026-01-04"),
        ];
        let sits = BTreeMap::from([
            ("owner/one#1".parse().unwrap(), SitState::Preparing),
            ("owner/one#3".parse().unwrap(), SitState::Preparing),
        ]);
        let watched = BTreeMap::from([
            ("owner/one".parse().unwrap(), RepoSettings::default()),
            ("owner/two".parse().unwrap(), RepoSettings::default()),
        ]);
        let repo = "owner/one".parse().unwrap();
        let history = BTreeMap::new();
        let rows = dashboard_rows(&prs, &history, &sits, &watched, Some(&repo));
        let watched_prs = watched_prs(&rows, &sits, &stack_parents(&prs));
        assert_eq!(
            watch_counts(&rows, &watched_prs),
            [
                (WatchFilter::All, 3),
                (WatchFilter::Watched, 2),
                (WatchFilter::Unwatched, 1)
            ]
        );
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
                SitState::Running {
                    last_line: None,
                    since: Instant::now(),
                },
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
        let rows = dashboard_rows(&prs, &history, &sits, &watched, None);
        let watched_prs = watched_prs(&rows, &sits, &stack_parents(&prs));
        for (filter, expected) in [
            (WatchFilter::All, vec![1, 3, 7, 8, 9, 10, 11, 12]),
            (WatchFilter::Watched, vec![3, 7, 8, 11, 12]),
            (WatchFilter::Unwatched, vec![1, 9, 10]),
        ] {
            let mut numbers: Vec<_> = rows
                .iter()
                .filter(|row| filter.admits(watched_prs.contains(&row.pr)))
                .map(|row| row.pr.number)
                .collect();
            numbers.sort_unstable();
            assert_eq!(numbers, expected, "{filter:?}");
        }
        let repo = "owner/one".parse().unwrap();
        let rows = dashboard_rows(&prs, &history, &sits, &watched, Some(&repo));
        assert_eq!(
            rows.iter().map(|row| row.pr.number).collect::<Vec<_>>(),
            [3, 1, 7, 11]
        );
        assert_eq!(
            rows.iter()
                .filter(|row| WatchFilter::Watched.admits(watched_prs.contains(&row.pr)))
                .map(|row| row.pr.number)
                .collect::<Vec<_>>(),
            [3, 7, 11]
        );
        assert_eq!(WatchFilter::default(), WatchFilter::All);
    }

    #[test]
    fn filters_classify_sits_and_pr_states() {
        let merged = SitState::Finished(Outcome::Merged);
        let rows: Vec<Cow<'_, PrRow>> = (1..=4)
            .map(|n| Cow::Owned(row(&format!("owner/one#{n}"), "2026-01-01")))
            .collect();
        let sits = BTreeMap::from([
            (rows[0].pr.clone(), merged.clone()),
            (rows[1].pr.clone(), SitState::Preparing),
            (
                rows[2].pr.clone(),
                SitState::Finished(Outcome::Stopped("closed".into())),
            ),
        ]);
        let watched = watched_prs(&rows, &sits, &BTreeMap::new());
        assert_eq!(
            watched,
            BTreeSet::from([rows[0].pr.clone(), rows[1].pr.clone()]),
            "merged and running babysits are watched; stopped and never-started are not"
        );

        let mut pr = row("owner/one#1", "2026-01-01");
        for state in [
            PrState::Ready,
            PrState::Queued,
            PrState::Draft,
            PrState::Merged,
            PrState::Closed,
        ] {
            pr.state = state;
            assert_eq!(
                StateFilter::Open.admits(&pr, None),
                matches!(state, PrState::Ready | PrState::Queued)
            );
            assert_eq!(
                StateFilter::Drafts.admits(&pr, None),
                state == PrState::Draft
            );
            assert_eq!(
                StateFilter::Merged.admits(&pr, None),
                state == PrState::Merged
            );
            assert!(StateFilter::All.admits(&pr, None));
        }
        pr.state = PrState::Ready;
        assert!(StateFilter::Merged.admits(&pr, Some(&merged)));
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
        let mut rows = dashboard_rows(&[], &history, &sits, &watched, None);
        let watched_set = watched_prs(&rows, &sits, &BTreeMap::new());
        rows.retain(|row| WatchFilter::Watched.admits(watched_set.contains(&row.pr)));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pr, pr);
        assert_eq!(rows[0].title, "The original title");
        assert_eq!(rows[0].url, pr.url());

        history.remove(&pr);
        sits.remove(&pr);
        assert!(dashboard_rows(&[], &history, &sits, &watched, None).is_empty());
        let open = [row("owner/one#7", "2026-01-01")];
        let mut rows = dashboard_rows(&open, &history, &sits, &watched, None);
        let watched_set = watched_prs(&rows, &sits, &BTreeMap::new());
        rows.retain(|row| WatchFilter::Unwatched.admits(watched_set.contains(&row.pr)));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "PR 7");

        sits.insert(pr.clone(), SitState::Preparing);
        let mut rows = dashboard_rows(&[], &history, &sits, &watched, None);
        let watched_set = watched_prs(&rows, &sits, &BTreeMap::new());
        rows.retain(|row| WatchFilter::Watched.admits(watched_set.contains(&row.pr)));
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
        let rows = dashboard_rows(&prs, &history, &sits, &BTreeMap::new(), None);
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
        config.active.insert(
            pr.clone(),
            ActiveSit {
                deadline: 1_700_000_000,
                stack: StackMode::BottomUp,
            },
        );
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
        assert_eq!(config.repos[&repo].draft_timeout, None);
        assert_eq!(
            config.repos[&repo]
                .resolve(&config.app.defaults)
                .draft_timeout,
            Duration::from_secs(7200)
        );
        config.repos.get_mut(&repo).unwrap().draft_timeout = Some(Duration::from_secs(28800));
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
