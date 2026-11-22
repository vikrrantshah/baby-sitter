use anyhow::{Context, bail};
use std::{fmt, str::FromStr};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Login(pub String);
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Sha(String);
impl Sha {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let s = s.trim();
        if s.len() != 40 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid full commit SHA: {s:?}");
        }
        Ok(Self(s.to_ascii_lowercase()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(pub String);
impl fmt::Display for ThreadId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "thread:{}", self.0)
    }
}
impl FromStr for ThreadId {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        let id = s.strip_prefix("thread:").context("invalid thread id")?;
        if id.is_empty() {
            bail!("invalid thread id {s:?}");
        }
        Ok(Self(id.into()))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommentId(pub u64);
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrRef {
    pub owner: String,
    pub repo: String,
    pub number: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Actor {
    Viewer,
    Human(Login),
    Bot(Login),
    Ghost,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reviewer {
    User(Login),
    Bot(Login),
    Team(String),
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Open,
    Closed,
    Merged,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mergeable {
    Yes,
    Conflicting,
    Unknown,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeState {
    Clean,
    Behind,
    Blocked,
    Dirty,
    Draft,
    HasHooks,
    Unknown,
    Unstable,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeRoute {
    Direct,
    Queue { method: MergeMethod },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueEntryState {
    Queued,
    AwaitingChecks,
    Mergeable,
    Unmergeable,
    Locked,
}
impl fmt::Display for QueueEntryState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Queued => "queued",
            Self::AwaitingChecks => "awaiting checks",
            Self::Mergeable => "mergeable",
            Self::Unmergeable => "unmergeable",
            Self::Locked => "locked",
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueueStatus {
    NotQueued,
    Queued {
        state: QueueEntryState,
        position: u32,
    },
    Removed {
        reason: String,
        before_commit: Sha,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RollupState {
    Success,
    Failure,
    Error,
    Pending,
    Expected,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ci {
    NoChecks,
    Rollup {
        state: RollupState,
        failing: Vec<String>,
        pending: Vec<String>,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    Approved,
    ChangesRequested,
    Commented,
    Dismissed,
    Pending,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    pub branch: String,
    pub sha: Sha,
}
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub pr_id: String,
    pub route: MergeRoute,
    pub queue: QueueStatus,
    pub author: Actor,
    pub lifecycle: Lifecycle,
    pub draft: bool,
    pub head: Head,
    pub mergeable: Mergeable,
    pub merge_state: MergeState,
    pub review_decision: Option<ReviewDecision>,
    pub review_requests: Vec<Reviewer>,
    pub ci: Ci,
    pub threads: Vec<Thread>,
    pub reviews: Vec<Review>,
}
#[derive(Clone, Debug)]
pub struct Thread {
    pub id: ThreadId,
    pub resolved: bool,
    pub outdated: bool,
    pub path: String,
    pub line: Option<u32>,
    comments: Vec<Comment>,
}
impl Thread {
    pub fn new(
        id: ThreadId,
        resolved: bool,
        outdated: bool,
        path: String,
        line: Option<u32>,
        comments: Vec<Comment>,
    ) -> anyhow::Result<Self> {
        if comments.is_empty() {
            bail!("review thread {} has no comments", id.0);
        }
        Ok(Self {
            id,
            resolved,
            outdated,
            path,
            line,
            comments,
        })
    }
    pub fn comments(&self) -> &[Comment] {
        &self.comments
    }
    pub fn answered_upto(&self) -> u64 {
        self.comments
            .iter()
            .filter(|c| c.actor == Actor::Viewer)
            .map(|c| match &c.marker {
                Some(m) if m.item == self.id => m.answers,
                Some(_) => 0,
                None => c.id.0,
            })
            .max()
            .unwrap_or(0)
    }
    pub fn unhandled(&self) -> bool {
        !self.resolved
            && self
                .comments
                .iter()
                .any(|c| c.actor != Actor::Viewer && c.id.0 > self.answered_upto())
    }
    pub fn newest_marker(&self) -> Option<&Marker> {
        self.comments
            .iter()
            .rev()
            .filter_map(|c| c.marker.as_ref())
            .find(|m| m.item == self.id)
    }
}
#[derive(Clone, Debug)]
pub struct Comment {
    pub id: CommentId,
    pub actor: Actor,
    pub body: String,
    pub url: String,
    marker: Option<Marker>,
}
impl Comment {
    pub fn new(id: CommentId, actor: Actor, body: String, url: String) -> Self {
        let marker = if actor == Actor::Viewer {
            Marker::parse(&body)
        } else {
            None
        };
        Self {
            id,
            actor,
            body,
            url,
            marker,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Review {
    pub actor: Actor,
    pub state: ReviewState,
}
#[derive(Clone, Debug)]
pub struct LocalState {
    pub upstream_branch: Option<String>,
    pub head: Sha,
    pub dirty: Vec<String>,
    pub relation: Relation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    Equal,
    Behind,
    Ahead,
    Diverged,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Fixed,
    Declined,
    NeedsHuman,
}
impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Fixed => "fixed",
            Self::Declined => "declined",
            Self::NeedsHuman => "needs_human",
        })
    }
}
impl FromStr for Outcome {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        Ok(match s {
            "fixed" => Self::Fixed,
            "declined" => Self::Declined,
            "needs_human" => Self::NeedsHuman,
            _ => bail!("invalid outcome {s:?}"),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub item: ThreadId,
    pub outcome: Outcome,
    pub answers: u64,
}
impl Marker {
    pub fn render(&self) -> String {
        format!(
            "<!-- baby-sitter:v1 item={} outcome={} answers={} -->",
            self.item, self.outcome, self.answers
        )
    }
    pub fn parse(body: &str) -> Option<Self> {
        let rest = body.rsplit_once("<!-- baby-sitter:v1 ")?.1;
        let fields = rest.split_once("-->")?.0;
        let (mut item, mut outcome, mut answers) = (None, None, None);
        for field in fields.split_whitespace() {
            match field.split_once('=') {
                Some(("item", v)) => item = v.parse().ok(),
                Some(("outcome", v)) => outcome = v.parse().ok(),
                Some(("answers", v)) => answers = v.parse().ok(),
                _ => {}
            }
        }
        Some(Self {
            item: item?,
            outcome: outcome?,
            answers: answers?,
        })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum MergeMethod {
    Squash,
    Merge,
    Rebase,
}
impl fmt::Display for MergeMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Squash => "squash",
            Self::Merge => "merge",
            Self::Rebase => "rebase",
        })
    }
}
