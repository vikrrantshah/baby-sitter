use crate::domain::*;
use std::{collections::BTreeSet, fmt, time::Duration};

const MAX_CI_FIX_ATTEMPTS: u32 = 3;
const MAX_CONFLICT_ATTEMPTS: u32 = 3;

fn names<I>(mut items: I) -> String
where
    I: ExactSizeIterator,
    I::Item: fmt::Display,
{
    use std::fmt::Write;

    let count = items.len();
    let mut result = String::new();
    for index in 0..count.min(3) {
        if index > 0 {
            result.push_str(if index == count - 1 { " and " } else { ", " });
        }
        write!(result, "{}", items.next().unwrap()).unwrap();
    }
    if count > 3 {
        write!(result, " and {} more", count - 3).unwrap();
    }
    result
}

fn check_names(checks: &[FailedCheck]) -> String {
    names(checks.iter().map(|check| &check.name))
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

pub struct Facts<'a> {
    pub snap: &'a Snapshot,
    pub local: Option<&'a LocalState>,
    pub method: MergeMethod,
    pub stack: StackMode,
}

#[derive(Debug, PartialEq, Eq)]
enum StackGate<'a> {
    Clear,
    Retarget { to: &'a str },
    BelowClosed { number: u64 },
    BelowOpen { number: u64 },
    AboveOpen { numbers: &'a [u64] },
}

impl StackGate<'_> {
    fn holds(&self) -> bool {
        matches!(self, Self::BelowOpen { .. } | Self::AboveOpen { .. })
    }
}

fn stack_gate<'a>(f: &Facts<'a>) -> StackGate<'a> {
    if let Some(parent) = &f.snap.parent {
        match parent.state {
            ParentState::Merged => return StackGate::Retarget { to: &parent.base },
            ParentState::Closed => {
                return StackGate::BelowClosed {
                    number: parent.number,
                };
            }
            ParentState::Open if f.stack == StackMode::BottomUp => {
                return StackGate::BelowOpen {
                    number: parent.number,
                };
            }
            ParentState::Open => {}
        }
    }
    if f.stack == StackMode::TopDown && !f.snap.children.is_empty() {
        StackGate::AboveOpen {
            numbers: &f.snap.children,
        }
    } else {
        StackGate::Clear
    }
}
#[derive(Debug, PartialEq)]
pub struct Decision {
    pub rule: &'static str,
    pub verdict: Verdict,
}
#[derive(Debug, PartialEq)]
pub enum Verdict {
    Done,
    Stop(String),
    Wait(String),
    Act(Action),
}
impl fmt::Display for Verdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Done => f.write_str("merged"),
            Self::Stop(reason) => write!(f, "stop: {reason}"),
            Self::Wait(reason) => write!(f, "wait: {reason}"),
            Self::Act(action) => write!(f, "{action}"),
        }
    }
}
#[derive(Debug, PartialEq)]
pub enum Action {
    FastForward(Sha),
    ResolveThreads(Vec<ThreadId>),
    HandleComments {
        batch: Batch,
        head: Head,
    },
    RerunCi {
        runs: BTreeSet<u64>,
        checks: Vec<String>,
    },
    FixCi {
        checks: Vec<FailedCheck>,
        head: Head,
    },
    ResolveConflicts {
        head: Head,
        base: String,
    },
    RetargetBase {
        to: String,
    },
    UpdateBranch,
    Enqueue {
        head: Sha,
    },
    Merge {
        head: Sha,
        method: MergeMethod,
    },
    /// Push commits baby-sitter made locally but failed to push. `base` is the PR head.
    PushLocal {
        base: Sha,
        head: Head,
    },
}
impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FastForward(sha) => {
                write!(f, "Fast-forwarding checkout to {}", &sha.as_str()[..7])
            }
            Self::ResolveThreads(ids) => write!(
                f,
                "Resolving {} fixed review thread{}",
                ids.len(),
                plural(ids.len())
            ),
            Self::HandleComments { batch, .. } => {
                write!(
                    f,
                    "Answering {} review thread{}: {}",
                    batch.0.len(),
                    plural(batch.0.len()),
                    names(batch.0.iter())
                )
            }
            Self::RerunCi { checks, .. } => {
                write!(f, "Rerunning cancelled CI: {}", names(checks.iter()))
            }
            Self::FixCi { checks, .. } => write!(f, "Fixing CI: {}", check_names(checks)),
            Self::ResolveConflicts { base, .. } => {
                write!(f, "Resolving merge conflicts with {base}")
            }
            Self::RetargetBase { to } => write!(f, "Retargeting the base to {to}"),
            Self::UpdateBranch => f.write_str("Updating the branch with its base"),
            Self::Enqueue { head } => {
                write!(f, "Adding {} to the merge queue", &head.as_str()[..7])
            }
            Self::Merge { head, method } => {
                let method = match method {
                    MergeMethod::Squash => "squash",
                    MergeMethod::Merge => "merge",
                    MergeMethod::Rebase => "rebase",
                };
                write!(f, "Merging {} ({method})", &head.as_str()[..7])
            }
            Self::PushLocal { head, .. } => {
                write!(
                    f,
                    "Pushing checkout commits up to {}",
                    &head.sha.as_str()[..7]
                )
            }
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct Batch(pub Vec<Item>);
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: ThreadId,
    pub answers: u64,
    pub rereview: Option<Login>,
    pub actor: Actor,
    pub path: String,
    pub line: Option<u32>,
    pub context: String,
}
impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.path)?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
        }
        match &self.actor {
            Actor::Human(login) | Actor::Bot(login) => write!(f, " (@{})", login.0),
            Actor::Viewer => f.write_str(" (viewer)"),
            Actor::Ghost => f.write_str(" (ghost)"),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Precondition,
    Phase(Gate),
}
struct Rule {
    name: &'static str,
    stage: Stage,
    eval: fn(&Facts<'_>) -> Option<Verdict>,
}
const RULES: &[Rule] = &[
    Rule {
        name: "merged",
        stage: Stage::Precondition,
        eval: merged,
    },
    Rule {
        name: "closed",
        stage: Stage::Precondition,
        eval: closed,
    },
    Rule {
        name: "local-upstream",
        stage: Stage::Precondition,
        eval: local_upstream,
    },
    Rule {
        name: "local-dirty",
        stage: Stage::Precondition,
        eval: local_dirty,
    },
    Rule {
        name: "local-diverged",
        stage: Stage::Precondition,
        eval: local_diverged,
    },
    Rule {
        name: "local-behind",
        stage: Stage::Precondition,
        eval: local_behind,
    },
    Rule {
        name: "resolve-fixed",
        stage: Stage::Phase(Gate::Reviews),
        eval: resolve_fixed,
    },
    Rule {
        name: "unhandled-items",
        stage: Stage::Phase(Gate::Reviews),
        eval: unhandled_items,
    },
    Rule {
        name: "stack-retarget",
        stage: Stage::Phase(Gate::Branch),
        eval: stack_retarget,
    },
    Rule {
        name: "mergeability-unknown",
        stage: Stage::Phase(Gate::Branch),
        eval: mergeability_unknown,
    },
    Rule {
        name: "conflict",
        stage: Stage::Phase(Gate::Branch),
        eval: conflict,
    },
    Rule {
        name: "behind-base",
        stage: Stage::Phase(Gate::Branch),
        eval: behind_base,
    },
    Rule {
        name: "ci-failed",
        stage: Stage::Phase(Gate::Checks),
        eval: ci_failed,
    },
    Rule {
        name: "rerun-cancelled",
        stage: Stage::Phase(Gate::Checks),
        eval: rerun_cancelled,
    },
    Rule {
        name: "fix-ci",
        stage: Stage::Phase(Gate::Checks),
        eval: fix_ci,
    },
    Rule {
        name: "ci-pending",
        stage: Stage::Phase(Gate::Checks),
        eval: ci_pending,
    },
    Rule {
        name: "ci-starting",
        stage: Stage::Phase(Gate::Checks),
        eval: ci_starting,
    },
    Rule {
        name: "stacked",
        stage: Stage::Phase(Gate::Merge),
        eval: stacked,
    },
    Rule {
        name: "stack-above",
        stage: Stage::Phase(Gate::Merge),
        eval: stack_above,
    },
    Rule {
        name: "in-queue",
        stage: Stage::Phase(Gate::Merge),
        eval: in_queue,
    },
    Rule {
        name: "dequeued",
        stage: Stage::Phase(Gate::Merge),
        eval: dequeued,
    },
    Rule {
        name: "review-decision",
        stage: Stage::Phase(Gate::Merge),
        eval: review_decision,
    },
    Rule {
        name: "review-requests",
        stage: Stage::Phase(Gate::Merge),
        eval: review_requests,
    },
    Rule {
        name: "unresolved-threads",
        stage: Stage::Phase(Gate::Merge),
        eval: unresolved_threads,
    },
    Rule {
        name: "draft",
        stage: Stage::Phase(Gate::Merge),
        eval: draft,
    },
    Rule {
        name: "merge-state",
        stage: Stage::Phase(Gate::Merge),
        eval: merge_state,
    },
];
/// Phase identifiers in wire order, as reported by `--gates`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Reviews,
    Checks,
    Branch,
    Merge,
}
impl Gate {
    const WIRE_ORDER: [Gate; 4] = [Gate::Reviews, Gate::Checks, Gate::Branch, Gate::Merge];
    const ORDER: [Gate; 4] = [Gate::Reviews, Gate::Branch, Gate::Checks, Gate::Merge];
    fn name(self) -> &'static str {
        match self {
            Gate::Reviews => "reviews",
            Gate::Checks => "checks",
            Gate::Branch => "branch",
            Gate::Merge => "merge",
        }
    }
}
/// Phase states. Skipped applies only to the merge phase of draft PRs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GateState {
    /// An earlier phase or precondition has not completed.
    Pending,
    Met,
    /// A rule waits on GitHub or a person, or the binary is acting on it.
    Waiting,
    /// A rule stops the run.
    Blocked,
    /// Draft PRs do not enter the merge gate.
    Skipped,
}
impl GateState {
    fn from_verdict(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Done => Self::Met,
            Verdict::Stop(_) => Self::Blocked,
            Verdict::Wait(_) | Verdict::Act(_) => Self::Waiting,
        }
    }
    fn name(self) -> &'static str {
        match self {
            GateState::Pending => "pending",
            GateState::Met => "met",
            GateState::Waiting => "waiting",
            GateState::Skipped => "skipped",
            GateState::Blocked => "blocked",
        }
    }
}
#[derive(Debug, PartialEq)]
pub struct Gates([GateState; 4]);
impl Gates {
    #[cfg(test)]
    fn get(&self, gate: Gate) -> GateState {
        self.0[gate as usize]
    }
}
impl fmt::Display for Gates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, (gate, state)) in Gate::WIRE_ORDER.iter().zip(self.0).enumerate() {
            let sep = if index == 0 { "" } else { " " };
            write!(f, "{sep}{}={}", gate.name(), state.name())?;
        }
        Ok(())
    }
}
/// Reports phases in order, leaving every phase after the first incomplete one pending.
pub fn gates(f: &Facts<'_>) -> Gates {
    let draft = f.snap.draft || f.snap.merge_state == MergeState::Draft;
    if f.snap.lifecycle == Lifecycle::Merged {
        let mut states = [GateState::Met; 4];
        if draft {
            states[Gate::Merge as usize] = GateState::Skipped;
        }
        return Gates(states);
    }

    let mut states = [GateState::Pending; 4];
    for rule in RULES.iter().filter(|r| r.stage == Stage::Precondition) {
        if let Some(verdict) = (rule.eval)(f) {
            states[Gate::Branch as usize] = GateState::from_verdict(verdict);
            return Gates(states);
        }
    }

    let held = stack_gate(f).holds();
    for phase in Gate::ORDER {
        let state = if phase == Gate::Merge && draft {
            GateState::Skipped
        } else {
            let mut state = GateState::Met;
            for rule in RULES.iter().filter(|r| r.stage == Stage::Phase(phase)) {
                if let Some(verdict) = (rule.eval)(f) {
                    let retarget = matches!(&verdict, Verdict::Act(Action::RetargetBase { .. }));
                    state = state.max(GateState::from_verdict(verdict));
                    if state == GateState::Blocked || retarget || (phase == Gate::Merge && held) {
                        break;
                    }
                }
            }
            if phase == Gate::Merge && state == GateState::Met {
                GateState::Waiting
            } else {
                state
            }
        };
        states[phase as usize] = state;
        if state != GateState::Met {
            break;
        }
    }
    Gates(states)
}
pub fn decide(f: &Facts<'_>) -> Decision {
    RULES
        .iter()
        .find_map(|r| {
            (r.eval)(f).map(|verdict| Decision {
                rule: r.name,
                verdict,
            })
        })
        .unwrap_or_else(|| Decision {
            rule: if matches!(f.snap.route, MergeRoute::Queue { .. }) {
                "enqueue"
            } else {
                "merge"
            },
            verdict: Verdict::Act(match f.snap.route {
                MergeRoute::Queue { .. } => Action::Enqueue {
                    head: f.snap.head.sha.clone(),
                },
                MergeRoute::Direct => Action::Merge {
                    head: f.snap.head.sha.clone(),
                    method: f.method,
                },
            }),
        })
}
pub fn explain(f: &Facts<'_>) -> String {
    let mut out = String::new();
    let mut fired = false;
    for r in RULES {
        let v = (r.eval)(f);
        let first = v.is_some() && !fired;
        fired |= v.is_some();
        out.push_str(&format!(
            "{} {:<20} {}\n",
            if first { "->" } else { "  " },
            r.name,
            v.map_or_else(|| "pass".into(), |v| v.to_string())
        ));
    }
    let action = if matches!(f.snap.route, MergeRoute::Queue { .. }) {
        "enqueue"
    } else {
        "merge"
    };
    out.push_str(&format!(
        "{} {action}{}\n",
        if fired { "  " } else { "->" },
        if fired { " (not reached)" } else { "" }
    ));
    out
}
fn merged(f: &Facts<'_>) -> Option<Verdict> {
    (f.snap.lifecycle == Lifecycle::Merged).then_some(Verdict::Done)
}
fn closed(f: &Facts<'_>) -> Option<Verdict> {
    (f.snap.lifecycle == Lifecycle::Closed)
        .then(|| Verdict::Stop("PR was closed without merging".into()))
}
fn draft(f: &Facts<'_>) -> Option<Verdict> {
    (f.snap.draft || f.snap.merge_state == MergeState::Draft).then(|| {
        Verdict::Wait(
            "Draft is up to date. Watching for new commits, CI results, and review comments."
                .into(),
        )
    })
}
fn stack_retarget(f: &Facts<'_>) -> Option<Verdict> {
    match stack_gate(f) {
        StackGate::Retarget { to } => Some(Verdict::Act(Action::RetargetBase { to: to.into() })),
        _ => None,
    }
}
fn mergeability_unknown(f: &Facts<'_>) -> Option<Verdict> {
    (f.snap.mergeable == Mergeable::Unknown)
        .then(|| Verdict::Wait("Waiting for GitHub to check mergeability".into()))
}
fn conflict(f: &Facts<'_>) -> Option<Verdict> {
    (f.snap.mergeable == Mergeable::Conflicting || f.snap.merge_state == MergeState::Dirty).then(
        || {
            if f.snap.conflict_attempts >= MAX_CONFLICT_ATTEMPTS {
                Verdict::Stop(format!(
                    "PR still conflicts with {} after 3 resolution attempts",
                    f.snap.base
                ))
            } else {
                Verdict::Act(Action::ResolveConflicts {
                    head: f.snap.head.clone(),
                    base: f.snap.base.clone(),
                })
            }
        },
    )
}
fn ci_failed(f: &Facts<'_>) -> Option<Verdict> {
    match &f.snap.ci {
        Ci::Rollup {
            state: RollupState::Failure | RollupState::Error,
            failing,
            pending,
            ..
        } if !pending.is_empty() => Some(Verdict::Wait(format!(
            "Waiting for {} more check{} before fixing {}",
            pending.len(),
            plural(pending.len()),
            if failing.is_empty() {
                "CI".to_owned()
            } else {
                check_names(failing)
            }
        ))),
        Ci::Rollup {
            state: RollupState::Failure | RollupState::Error,
            failing,
            cancelled,
            ..
        } if failing.is_empty() && cancelled.is_empty() => Some(Verdict::Stop(
            "CI failed, but GitHub didn't say which check".into(),
        )),
        Ci::Rollup {
            state: RollupState::Failure | RollupState::Error,
            failing,
            ..
        } if !failing.is_empty() && f.snap.ci_fix_attempts >= MAX_CI_FIX_ATTEMPTS => {
            Some(Verdict::Stop(format!(
                "CI still fails after 3 fix attempts: {}",
                check_names(failing)
            )))
        }
        _ => None,
    }
}
fn stacked(f: &Facts<'_>) -> Option<Verdict> {
    match stack_gate(f) {
        StackGate::BelowOpen { number } => Some(Verdict::Wait(format!(
            "Waiting for #{number} below to merge"
        ))),
        StackGate::BelowClosed { number } => Some(Verdict::Stop(format!(
            "The PR below, #{number}, was closed without merging"
        ))),
        _ => None,
    }
}
fn stack_above(f: &Facts<'_>) -> Option<Verdict> {
    match stack_gate(f) {
        StackGate::AboveOpen { numbers } => Some(Verdict::Wait(format!(
            "Waiting for {} above to merge",
            names(numbers.iter().map(|number| format!("#{number}")))
        ))),
        _ => None,
    }
}
fn in_queue(f: &Facts<'_>) -> Option<Verdict> {
    match &f.snap.queue {
        QueueStatus::Queued { state, position } => Some(Verdict::Wait(format!(
            "In the merge queue at position {position}, {state}"
        ))),
        _ => None,
    }
}
fn dequeued(f: &Facts<'_>) -> Option<Verdict> {
    match &f.snap.queue {
        QueueStatus::Removed {
            reason,
            before_commit,
            removed_ago,
        } if !reason.eq_ignore_ascii_case("merged")
            && removed_current_head(before_commit.as_ref(), *removed_ago, f.snap) =>
        {
            Some(Verdict::Stop(format!(
                "Merge queue removed the PR: {reason}"
            )))
        }
        _ => None,
    }
}
/// Without the removed commit, a removal concerns the current head only if it happened after
/// that head was committed. When either time is unknown, the run does not stop on it.
fn removed_current_head(
    before_commit: Option<&Sha>,
    removed_ago: Option<Duration>,
    snap: &Snapshot,
) -> bool {
    match before_commit {
        Some(sha) => *sha == snap.head.sha,
        None => {
            matches!((removed_ago, snap.head_age), (Some(removed), Some(head)) if removed <= head)
        }
    }
}
fn local_upstream(f: &Facts<'_>) -> Option<Verdict> {
    match &f.local?.upstream_branch {
        None => Some(Verdict::Stop(
            "Checkout's branch doesn't track a remote branch".into(),
        )),
        Some(b) if *b != f.snap.head.branch => Some(Verdict::Stop(format!(
            "Checkout tracks {b}, but the PR branch is {}",
            f.snap.head.branch
        ))),
        _ => None,
    }
}
fn local_dirty(f: &Facts<'_>) -> Option<Verdict> {
    let local = f.local?;
    (!local.dirty.is_empty()).then(|| {
        Verdict::Stop(format!(
            "Checkout has uncommitted changes: {}",
            names(local.dirty.iter())
        ))
    })
}
fn local_diverged(f: &Facts<'_>) -> Option<Verdict> {
    let local = f.local?;
    match local.relation {
        Relation::Ahead => Some(Verdict::Stop(
            "Checkout has commits that aren't on GitHub; push or reset them".into(),
        )),
        Relation::Pushed => Some(Verdict::Wait(format!(
            "Waiting for GitHub to show the push ({})",
            &local.head.as_str()[..7]
        ))),
        Relation::Unpushed => Some(Verdict::Act(Action::PushLocal {
            base: f.snap.head.sha.clone(),
            head: Head {
                branch: f.snap.head.branch.clone(),
                sha: local.head.clone(),
            },
        })),
        Relation::Diverged => Some(Verdict::Stop(format!(
            "Checkout ({}) and the PR ({}) have diverged",
            &local.head.as_str()[..7],
            &f.snap.head.sha.as_str()[..7]
        ))),
        _ => None,
    }
}
fn local_behind(f: &Facts<'_>) -> Option<Verdict> {
    (f.local?.relation == Relation::Behind)
        .then(|| Verdict::Act(Action::FastForward(f.snap.head.sha.clone())))
}
fn resolve_fixed(f: &Facts<'_>) -> Option<Verdict> {
    let ids: Vec<_> = f
        .snap
        .threads
        .iter()
        .filter(|t| {
            !t.resolved
                && !t.unhandled()
                && t.newest_marker()
                    .is_some_and(|m| m.outcome == Outcome::Fixed)
        })
        .map(|t| t.id.clone())
        .collect();
    (!ids.is_empty()).then_some(Verdict::Act(Action::ResolveThreads(ids)))
}
fn unhandled_items(f: &Facts<'_>) -> Option<Verdict> {
    let b = unhandled(f.snap);
    (!b.0.is_empty()).then(|| {
        Verdict::Act(Action::HandleComments {
            batch: b,
            head: f.snap.head.clone(),
        })
    })
}
fn rerun_cancelled(f: &Facts<'_>) -> Option<Verdict> {
    match &f.snap.ci {
        Ci::Rollup {
            state: RollupState::Failure | RollupState::Error,
            failing,
            cancelled,
            pending,
        } if failing.is_empty() && !cancelled.is_empty() && pending.is_empty() => {
            Some(Verdict::Act(Action::RerunCi {
                runs: cancelled.iter().map(|check| check.run_id).collect(),
                checks: cancelled.iter().map(|check| check.name.clone()).collect(),
            }))
        }
        _ => None,
    }
}
fn fix_ci(f: &Facts<'_>) -> Option<Verdict> {
    match &f.snap.ci {
        Ci::Rollup {
            state: RollupState::Failure | RollupState::Error,
            failing,
            pending,
            ..
        } if !failing.is_empty()
            && pending.is_empty()
            && f.snap.ci_fix_attempts < MAX_CI_FIX_ATTEMPTS =>
        {
            Some(Verdict::Act(Action::FixCi {
                checks: failing.clone(),
                head: f.snap.head.clone(),
            }))
        }
        _ => None,
    }
}
fn behind_base(f: &Facts<'_>) -> Option<Verdict> {
    (f.snap.route == MergeRoute::Direct
        && (f.snap.merge_state == MergeState::Behind
            || (f.snap.merge_state == MergeState::Draft
                && f.snap.commits_behind.is_some_and(|count| count > 0))))
    .then_some(Verdict::Act(Action::UpdateBranch))
}
fn ci_pending(f: &Facts<'_>) -> Option<Verdict> {
    match &f.snap.ci {
        Ci::Rollup {
            state: RollupState::Pending | RollupState::Expected,
            pending,
            ..
        } => Some(Verdict::Wait(if pending.is_empty() {
            "Waiting for CI".into()
        } else {
            format!("Waiting for CI: {}", names(pending.iter()))
        })),
        _ => None,
    }
}
fn ci_starting(f: &Facts<'_>) -> Option<Verdict> {
    (matches!(f.snap.ci, Ci::NoChecks)
        && f.snap
            .head_age
            .is_some_and(|age| age < Duration::from_secs(3 * 60)))
    .then(|| Verdict::Wait("Waiting for CI to start on the new commit".into()))
}
fn merge_state(f: &Facts<'_>) -> Option<Verdict> {
    match f.snap.merge_state {
        MergeState::Clean | MergeState::HasHooks | MergeState::Draft => None,
        MergeState::Behind if matches!(f.snap.route, MergeRoute::Queue { .. }) => None,
        state => Some(Verdict::Wait(
            match state {
                MergeState::Behind => "Branch is behind its base",
                MergeState::Blocked => {
                    "Waiting because GitHub blocks the merge without naming a missing review, check, or thread"
                }
                MergeState::Dirty => "PR has merge conflicts with its base branch",
                MergeState::Unknown => "Waiting for GitHub to work out the merge state",
                MergeState::Unstable => "Waiting because some checks are failing or still running",
                MergeState::Clean | MergeState::HasHooks | MergeState::Draft => unreachable!(),
            }
            .into(),
        )),
    }
}
fn unresolved_threads(f: &Facts<'_>) -> Option<Verdict> {
    let n = f.snap.threads.iter().filter(|t| !t.resolved).count();
    (n > 0).then(|| {
        Verdict::Wait(format!(
            "Waiting on {n} unresolved review thread{}",
            plural(n)
        ))
    })
}
/// GitHub's `reviewDecision` already encodes the required approvals; once it is
/// `Approved`, a still-pending request (often our own re-review request) no longer blocks.
fn review_requests(f: &Facts<'_>) -> Option<Verdict> {
    (!f.snap.review_requests.is_empty() && f.snap.review_decision != Some(ReviewDecision::Approved))
        .then(|| {
            Verdict::Wait(format!(
                "Waiting for review from {}",
                names(f.snap.review_requests.iter())
            ))
        })
}
fn review_decision(f: &Facts<'_>) -> Option<Verdict> {
    let mut message = match f.snap.review_decision {
        Some(ReviewDecision::ChangesRequested) => {
            let mut seen = BTreeSet::new();
            let mut requested = Vec::new();
            for review in f.snap.reviews.iter().rev() {
                if let Actor::Human(login) = &review.actor
                    && matches!(
                        review.state,
                        ReviewState::Approved
                            | ReviewState::ChangesRequested
                            | ReviewState::Dismissed
                    )
                    && seen.insert(login)
                    && review.state == ReviewState::ChangesRequested
                {
                    requested.push(login.0.as_str());
                }
            }
            requested.reverse();
            if requested.is_empty() {
                "Waiting because changes were requested".to_owned()
            } else {
                format!(
                    "Waiting because {} requested changes",
                    names(requested.iter())
                )
            }
        }
        Some(ReviewDecision::ReviewRequired) => "Waiting for an approving review".to_owned(),
        _ => return None,
    };
    if !f.snap.review_requests.is_empty() {
        use std::borrow::Cow;
        let reviewers = f
            .snap
            .review_requests
            .iter()
            .map(|reviewer| match reviewer {
                Reviewer::User(login) | Reviewer::Bot(login) => Cow::Borrowed(login.0.as_str()),
                _ => Cow::Owned(reviewer.to_string()),
            });
        message.push_str(&format!(". Review requested from {}", names(reviewers)));
    }
    Some(Verdict::Wait(message))
}
fn unhandled(s: &Snapshot) -> Batch {
    let rereview = |a: &Actor| match a {
        Actor::Human(l)
            if s.author != *a
                && s.reviews
                    .iter()
                    .any(|r| r.actor == *a && r.state != ReviewState::Pending) =>
        {
            Some(l.clone())
        }
        _ => None,
    };
    let mut items = Vec::new();
    for t in s.threads.iter().filter(|t| t.unhandled()) {
        let newest = t
            .comments()
            .iter()
            .filter(|c| c.actor != Actor::Viewer)
            .max_by_key(|c| c.id.0)
            .expect("unhandled thread has non-viewer comment");
        let context = format!(
            "Thread {} at {}:{:?}, outdated={}:\n{}",
            t.id.0,
            t.path,
            t.line,
            t.outdated,
            t.comments()
                .iter()
                .map(|c| format!("{:?} {}: {}", c.actor, c.url, c.body))
                .collect::<Vec<_>>()
                .join("\n")
        );
        items.push(Item {
            id: t.id.clone(),
            answers: newest.id.0,
            rereview: rereview(&newest.actor),
            actor: newest.actor.clone(),
            path: t.path.clone(),
            line: t.line,
            context,
        });
    }
    Batch(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn human_review(name: &str, state: ReviewState) -> Review {
        Review {
            actor: Actor::Human(Login(name.into())),
            state,
        }
    }

    #[test]
    fn latest_decisive_human_reviews_name_the_blocker() {
        let mut w = World::new();
        w.snap.merge_state = MergeState::Blocked;
        w.snap.review_decision = Some(ReviewDecision::ChangesRequested);
        w.snap.reviews = vec![
            human_review("muslemomar", ReviewState::ChangesRequested),
            human_review("windeko", ReviewState::ChangesRequested),
            human_review("muslemomar", ReviewState::Approved),
            human_review("windeko", ReviewState::Commented),
            human_review("windeko", ReviewState::Pending),
        ];
        w.snap.review_requests = vec![
            Reviewer::User(Login("mohsen89z".into())),
            Reviewer::User(Login("windeko".into())),
        ];
        assert_eq!(
            w.decision(),
            Decision {
                rule: "review-decision",
                verdict: Verdict::Wait("Waiting because windeko requested changes. Review requested from mohsen89z and windeko".into()),
            }
        );
        assert_eq!(w.gates().get(Gate::Merge), GateState::Waiting);
    }

    #[test]
    fn changes_requested_names_follow_their_latest_decisive_reviews() {
        let mut w = World::new();
        w.snap.review_decision = Some(ReviewDecision::ChangesRequested);
        w.snap.reviews = vec![
            human_review("alice", ReviewState::ChangesRequested),
            human_review("bob", ReviewState::ChangesRequested),
            human_review("alice", ReviewState::ChangesRequested),
            Review {
                actor: Actor::Bot(Login("bot".into())),
                state: ReviewState::ChangesRequested,
            },
        ];
        assert_eq!(
            w.decision().verdict,
            Verdict::Wait("Waiting because bob and alice requested changes".into())
        );
    }

    #[test]
    fn unidentified_changes_and_approval_requests_are_explained() {
        let mut w = World::new();
        w.snap.review_decision = Some(ReviewDecision::ChangesRequested);
        w.snap.reviews = vec![
            human_review("alice", ReviewState::ChangesRequested),
            human_review("alice", ReviewState::Dismissed),
        ];
        w.snap.review_requests = vec![Reviewer::User(Login("x".into()))];
        assert_eq!(
            w.decision().verdict,
            Verdict::Wait("Waiting because changes were requested. Review requested from x".into())
        );
        w.snap.review_decision = Some(ReviewDecision::ReviewRequired);
        assert_eq!(
            w.decision(),
            Decision {
                rule: "review-decision",
                verdict: Verdict::Wait(
                    "Waiting for an approving review. Review requested from x".into()
                ),
            }
        );
    }

    #[test]
    fn unexplained_github_block_is_the_fallback() {
        let mut w = World::new();
        w.snap.merge_state = MergeState::Blocked;
        assert_eq!(
            w.decision(),
            Decision {
                rule: "merge-state",
                verdict: Verdict::Wait("Waiting because GitHub blocks the merge without naming a missing review, check, or thread".into()),
            }
        );
        assert_eq!(w.gates().get(Gate::Merge), GateState::Waiting);
    }

    #[test]
    fn name_lists_limit_the_displayed_names() {
        let all = ["a", "b", "c", "d", "e"];
        for (count, expected) in [
            "",
            "a",
            "a and b",
            "a, b and c",
            "a, b, c and 1 more",
            "a, b, c and 2 more",
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(names(all[..count].iter()), expected);
        }
    }

    #[test]
    fn review_requests_display_plain_names() {
        let mut w = World::new();
        w.snap.review_decision = None;
        w.snap.review_requests = vec![
            Reviewer::User(Login("alice".into())),
            Reviewer::Bot(Login("bot".into())),
            Reviewer::Team("platform".into()),
            Reviewer::Unknown,
        ];
        let message = w.decision().verdict.to_string();
        assert_eq!(
            message,
            "wait: Waiting for review from @alice, @bot, team platform and 1 more"
        );
        assert!(!message.contains("User("));
        assert!(!message.contains("Login("));
        assert!(!message.contains("Bot("));
        assert_eq!(Reviewer::Unknown.to_string(), "an unknown reviewer");
    }

    struct World {
        snap: Snapshot,
        local: LocalState,
        stack: StackMode,
    }
    impl World {
        fn new() -> Self {
            let sha = Sha::parse(&"a".repeat(40)).unwrap();
            Self {
                snap: Snapshot {
                    pr_id: "PR_test".into(),
                    route: MergeRoute::Direct,
                    queue: QueueStatus::NotQueued,
                    author: Actor::Viewer,
                    lifecycle: Lifecycle::Open,
                    draft: false,
                    head: Head {
                        branch: "feature".into(),
                        sha: sha.clone(),
                    },
                    base: "main".into(),
                    parent: None,
                    children: vec![],
                    head_age: None,
                    mergeable: Mergeable::Yes,
                    merge_state: MergeState::Clean,
                    commits_behind: None,
                    review_decision: Some(ReviewDecision::Approved),
                    review_requests: vec![],
                    ci: Ci::NoChecks,
                    ci_fix_attempts: 0,
                    conflict_attempts: 0,
                    threads: vec![],
                    reviews: vec![],
                },
                local: LocalState {
                    upstream_branch: Some("feature".into()),
                    head: sha,
                    dirty: vec![],
                    relation: Relation::Equal,
                },
                stack: StackMode::BottomUp,
            }
        }
        fn decision(&self) -> Decision {
            decide(&Facts {
                snap: &self.snap,
                local: Some(&self.local),
                method: MergeMethod::Squash,
                stack: self.stack,
            })
        }
        fn gates(&self) -> Gates {
            gates(&Facts {
                snap: &self.snap,
                local: Some(&self.local),
                method: MergeMethod::Squash,
                stack: self.stack,
            })
        }
    }

    #[test]
    fn stack_decisions_and_gates_follow_mode_and_parent_precedence() {
        for mode in [StackMode::BottomUp, StackMode::TopDown] {
            for (parent, children, rule, verdict, branch, merge) in [
                (
                    None,
                    false,
                    "merge",
                    "merge",
                    GateState::Met,
                    GateState::Waiting,
                ),
                (
                    None,
                    true,
                    if mode == StackMode::TopDown {
                        "stack-above"
                    } else {
                        "merge"
                    },
                    if mode == StackMode::TopDown {
                        "wait: Waiting for #12 and #14 above to merge"
                    } else {
                        "merge"
                    },
                    GateState::Met,
                    GateState::Waiting,
                ),
                (
                    Some(ParentState::Open),
                    false,
                    if mode == StackMode::BottomUp {
                        "stacked"
                    } else {
                        "merge"
                    },
                    if mode == StackMode::BottomUp {
                        "wait: Waiting for #10 below to merge"
                    } else {
                        "merge"
                    },
                    GateState::Met,
                    GateState::Waiting,
                ),
                (
                    Some(ParentState::Open),
                    true,
                    if mode == StackMode::BottomUp {
                        "stacked"
                    } else {
                        "stack-above"
                    },
                    if mode == StackMode::BottomUp {
                        "wait: Waiting for #10 below to merge"
                    } else {
                        "wait: Waiting for #12 and #14 above to merge"
                    },
                    GateState::Met,
                    GateState::Waiting,
                ),
                (
                    Some(ParentState::Closed),
                    true,
                    "stacked",
                    "stop: The PR below, #10, was closed without merging",
                    GateState::Met,
                    GateState::Blocked,
                ),
                (
                    Some(ParentState::Merged),
                    true,
                    "stack-retarget",
                    "Retargeting the base to main",
                    GateState::Waiting,
                    GateState::Pending,
                ),
            ] {
                let mut w = World::new();
                w.stack = mode;
                w.snap.parent = parent.map(|state| Parent {
                    number: 10,
                    state,
                    base: "main".into(),
                });
                if children {
                    w.snap.children = vec![12, 14];
                }
                let decision = w.decision();
                assert_eq!(decision.rule, rule, "{mode:?} {parent:?}");
                if verdict == "merge" {
                    assert!(matches!(
                        decision.verdict,
                        Verdict::Act(Action::Merge { .. })
                    ));
                } else {
                    assert_eq!(decision.verdict.to_string(), verdict);
                }
                let gates = w.gates();
                assert_eq!(gates.get(Gate::Branch), branch);
                assert_eq!(gates.get(Gate::Merge), merge);
            }
        }
    }

    #[test]
    fn top_down_children_hold_before_reviews_and_queue_removal() {
        let mut w = World::new();
        w.stack = StackMode::TopDown;
        w.snap.children = vec![12];
        w.snap.review_decision = Some(ReviewDecision::ReviewRequired);
        w.snap.queue = QueueStatus::Removed {
            reason: "checks failed".into(),
            before_commit: Some(w.snap.head.sha.clone()),
            removed_ago: None,
        };
        assert_eq!(
            w.decision(),
            Decision {
                rule: "stack-above",
                verdict: Verdict::Wait("Waiting for #12 above to merge".into()),
            }
        );
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=met branch=met merge=waiting"
        );
    }
    #[test]
    fn open_parent_prevents_both_direct_merge_and_enqueue() {
        for route in [
            MergeRoute::Direct,
            MergeRoute::Queue {
                method: MergeMethod::Squash,
            },
        ] {
            let mut w = World::new();
            w.snap.base = "feature/parent".into();
            w.snap.route = route;
            w.snap.parent = Some(Parent {
                number: 12,
                state: ParentState::Open,
                base: "main".into(),
            });
            assert_eq!(
                w.decision(),
                Decision {
                    rule: "stacked",
                    verdict: Verdict::Wait("Waiting for #12 below to merge".into()),
                }
            );
            assert_eq!(
                w.gates().to_string(),
                "reviews=met checks=met branch=met merge=waiting"
            );
            let facts = Facts {
                snap: &w.snap,
                local: Some(&w.local),
                method: MergeMethod::Squash,
                stack: w.stack,
            };
            let explanation = explain(&facts);
            let stacked = explanation
                .lines()
                .find(|line| line.contains("stacked"))
                .unwrap();
            assert!(stacked.starts_with("->"));
            assert!(stacked.contains("wait: Waiting for #12 below to merge"));
            let fallback = if route == MergeRoute::Direct {
                "merge"
            } else {
                "enqueue"
            };
            assert!(explanation.contains(&format!("{fallback} (not reached)")));
            w.snap.queue = QueueStatus::Removed {
                reason: "checks failed".into(),
                before_commit: Some(w.snap.head.sha.clone()),
                removed_ago: None,
            };
            assert_eq!(w.decision().rule, "stacked");
            assert_eq!(
                w.gates().to_string(),
                "reviews=met checks=met branch=met merge=waiting"
            );
        }
    }

    #[test]
    fn open_parent_wait_takes_precedence_over_pending_reviews() {
        let mut w = World::new();
        w.snap.parent = Some(Parent {
            number: 12,
            state: ParentState::Open,
            base: "main".into(),
        });
        w.snap.review_decision = Some(ReviewDecision::ReviewRequired);
        w.snap.merge_state = MergeState::Blocked;
        assert_eq!(
            w.decision(),
            Decision {
                rule: "stacked",
                verdict: Verdict::Wait("Waiting for #12 below to merge".into()),
            }
        );
    }

    #[test]
    fn merged_parent_retargets_before_mergeability_or_conflicts() {
        for (mergeable, merge_state) in [
            (Mergeable::Unknown, MergeState::Unknown),
            (Mergeable::Conflicting, MergeState::Dirty),
        ] {
            let mut w = World::new();
            w.snap.base = "feature/parent".into();
            w.snap.conflict_attempts = 3;
            w.snap.mergeable = mergeable;
            w.snap.merge_state = merge_state;
            w.snap.parent = Some(Parent {
                number: 12,
                state: ParentState::Merged,
                base: "release/next".into(),
            });
            assert_eq!(
                w.decision(),
                Decision {
                    rule: "stack-retarget",
                    verdict: Verdict::Act(Action::RetargetBase {
                        to: "release/next".into(),
                    }),
                }
            );
            assert_eq!(
                w.gates().to_string(),
                "reviews=met checks=pending branch=waiting merge=pending"
            );
            assert_eq!(
                w.decision().verdict.to_string(),
                "Retargeting the base to release/next"
            );
        }
    }

    #[test]
    fn closed_parent_stops_in_the_merge_phase() {
        let mut w = World::new();
        w.snap.parent = Some(Parent {
            number: 12,
            state: ParentState::Closed,
            base: "main".into(),
        });
        assert_eq!(
            w.decision(),
            Decision {
                rule: "stacked",
                verdict: Verdict::Stop("The PR below, #12, was closed without merging".into()),
            }
        );
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=met branch=met merge=blocked"
        );
    }

    #[test]
    fn non_default_base_without_parent_merges_normally() {
        let mut w = World::new();
        w.snap.base = "release/next".into();
        assert_eq!(
            w.decision(),
            Decision {
                rule: "merge",
                verdict: Verdict::Act(Action::Merge {
                    head: w.snap.head.sha.clone(),
                    method: MergeMethod::Squash,
                }),
            }
        );
    }
    #[test]
    fn fresh_head_without_checks_waits_for_ci_and_reports_the_gate() {
        let mut w = World::new();
        w.snap.head_age = Some(Duration::from_secs(10));
        assert_eq!(
            w.decision(),
            Decision {
                rule: "ci-starting",
                verdict: Verdict::Wait("Waiting for CI to start on the new commit".into()),
            }
        );
        let facts = Facts {
            snap: &w.snap,
            local: Some(&w.local),
            method: MergeMethod::Squash,
            stack: w.stack,
        };
        assert_eq!(
            gates(&facts).to_string(),
            "reviews=met checks=waiting branch=met merge=pending"
        );
        w.snap.ci = Ci::Rollup {
            state: RollupState::Pending,
            failing: vec![],
            cancelled: vec![],
            pending: vec!["github-actions checks".into()],
        };
        assert_eq!(w.decision().rule, "ci-pending");
        w.snap.ci = Ci::Rollup {
            state: RollupState::Success,
            failing: vec![],
            cancelled: vec![],
            pending: vec![],
        };
        assert_eq!(w.decision().rule, "merge");
    }
    #[test]
    fn old_or_unknown_head_without_checks_can_merge() {
        for age in [
            Some(Duration::from_secs(180)),
            Some(Duration::from_secs(600)),
            None,
        ] {
            let mut w = World::new();
            w.snap.head_age = age;
            assert_eq!(
                w.decision(),
                Decision {
                    rule: "merge",
                    verdict: Verdict::Act(Action::Merge {
                        head: w.snap.head.sha.clone(),
                        method: MergeMethod::Squash,
                    }),
                }
            );
        }
    }
    fn comment(id: u64, actor: Actor, body: &str) -> Comment {
        Comment::new(CommentId(id), actor, body.into(), "url".into())
    }
    fn bot(id: u64) -> Comment {
        comment(id, Actor::Bot(Login("bot".into())), "issue")
    }
    fn marked(id: u64, item: ThreadId, outcome: Outcome, answers: u64) -> Comment {
        comment(
            id,
            Actor::Viewer,
            &Marker {
                item,
                outcome,
                answers,
            }
            .render(),
        )
    }
    fn thread(comments: Vec<Comment>) -> Thread {
        Thread::new(
            ThreadId("t".into()),
            false,
            false,
            "x".into(),
            Some(1),
            comments,
        )
        .unwrap()
    }
    fn tid() -> ThreadId {
        ThreadId("t".into())
    }
    #[test]
    fn merged_pr_needs_no_local_checkout() {
        let mut w = World::new();
        w.snap.lifecycle = Lifecycle::Merged;
        let facts = Facts {
            snap: &w.snap,
            local: None,
            method: MergeMethod::Squash,
            stack: w.stack,
        };
        assert_eq!(decide(&facts).verdict, Verdict::Done);
        assert!(local_upstream(&facts).is_none());
        assert!(local_dirty(&facts).is_none());
        assert!(local_diverged(&facts).is_none());
        assert!(local_behind(&facts).is_none());
    }
    #[test]
    fn queue_handles_feedback_before_waiting() {
        let mut w = World::new();
        w.snap.route = MergeRoute::Queue {
            method: MergeMethod::Squash,
        };
        w.snap.queue = QueueStatus::Queued {
            state: QueueEntryState::AwaitingChecks,
            position: 2,
        };
        w.snap.merge_state = MergeState::Behind;
        w.snap.threads.push(thread(vec![bot(3)]));
        let decision = w.decision();
        assert_eq!(decision.rule, "unhandled-items");
        assert!(matches!(
            decision.verdict,
            Verdict::Act(Action::HandleComments { .. })
        ));
        assert_eq!(
            w.gates().to_string(),
            "reviews=waiting checks=pending branch=pending merge=pending"
        );
    }
    #[test]
    fn queue_removal_stops_only_at_the_removed_head() {
        let mut w = World::new();
        w.snap.route = MergeRoute::Queue {
            method: MergeMethod::Squash,
        };
        w.snap.queue = QueueStatus::Removed {
            reason: "checks failed".into(),
            before_commit: Some(w.snap.head.sha.clone()),
            removed_ago: None,
        };
        assert_eq!(
            w.decision().verdict,
            Verdict::Stop("Merge queue removed the PR: checks failed".into())
        );
        w.snap.head.sha = Sha::parse(&"b".repeat(40)).unwrap();
        assert_eq!(
            w.decision().verdict,
            Verdict::Act(Action::Enqueue {
                head: w.snap.head.sha.clone()
            })
        );
    }
    #[test]
    fn queue_removal_naming_no_commit_stops_only_when_it_followed_the_head() {
        let mut w = World::new();
        w.snap.route = MergeRoute::Queue {
            method: MergeMethod::Squash,
        };
        w.snap.head_age = Some(Duration::from_secs(600));
        w.snap.queue = QueueStatus::Removed {
            reason: "merge_conflict".into(),
            before_commit: None,
            removed_ago: Some(Duration::from_secs(3600)),
        };
        assert_eq!(
            w.decision().rule,
            "enqueue",
            "a removal before the head was pushed concerns an older commit"
        );
        w.snap.queue = QueueStatus::Removed {
            reason: "checks failed".into(),
            before_commit: None,
            removed_ago: Some(Duration::from_secs(60)),
        };
        assert_eq!(
            w.decision().verdict,
            Verdict::Stop("Merge queue removed the PR: checks failed".into())
        );
        w.snap.head_age = None;
        assert_eq!(w.decision().rule, "enqueue", "unknown order does not stop");
    }
    #[test]
    fn conflicting_branches_are_resolved_after_checkout_guards() {
        for (route, mergeable, merge_state) in [
            (
                MergeRoute::Direct,
                Mergeable::Conflicting,
                MergeState::Clean,
            ),
            (
                MergeRoute::Queue {
                    method: MergeMethod::Squash,
                },
                Mergeable::Yes,
                MergeState::Dirty,
            ),
        ] {
            let mut w = World::new();
            w.snap.route = route;
            w.snap.mergeable = mergeable;
            w.snap.merge_state = merge_state;
            w.snap.base = "release/next".into();
            assert_eq!(
                w.decision(),
                Decision {
                    rule: "conflict",
                    verdict: Verdict::Act(Action::ResolveConflicts {
                        head: w.snap.head.clone(),
                        base: "release/next".into(),
                    }),
                }
            );
            assert_eq!(
                w.decision().verdict.to_string(),
                "Resolving merge conflicts with release/next"
            );
            let facts = Facts {
                snap: &w.snap,
                local: Some(&w.local),
                method: MergeMethod::Squash,
                stack: w.stack,
            };
            assert_eq!(gates(&facts).get(Gate::Branch), GateState::Waiting);
            w.local.dirty.push(" M local-file".into());
            assert_eq!(w.decision().rule, "local-dirty");
            w.local.dirty.clear();
            w.local.relation = Relation::Behind;
            assert_eq!(w.decision().rule, "local-behind");
            w.local.relation = Relation::Equal;
            w.snap.conflict_attempts = 3;
            assert_eq!(
                w.decision().verdict,
                Verdict::Stop(
                    "PR still conflicts with release/next after 3 resolution attempts".into()
                )
            );
        }
    }

    #[test]
    fn queue_removal_for_a_conflict_does_not_prevent_resolution() {
        let mut w = World::new();
        w.snap.route = MergeRoute::Queue {
            method: MergeMethod::Merge,
        };
        w.snap.mergeable = Mergeable::Conflicting;
        w.snap.queue = QueueStatus::Removed {
            reason: "conflicts".into(),
            before_commit: Some(w.snap.head.sha.clone()),
            removed_ago: None,
        };
        assert_eq!(w.decision().rule, "conflict");
    }
    #[test]
    fn queue_behind_base_enqueues_and_direct_route_merges() {
        let mut w = World::new();
        w.snap.route = MergeRoute::Queue {
            method: MergeMethod::Rebase,
        };
        w.snap.merge_state = MergeState::Behind;
        assert_eq!(
            w.decision().verdict,
            Verdict::Act(Action::Enqueue {
                head: w.snap.head.sha.clone()
            })
        );
        w.snap.route = MergeRoute::Direct;
        assert_eq!(w.decision().verdict, Verdict::Act(Action::UpdateBranch));
        w.snap.merge_state = MergeState::Clean;
        assert_eq!(
            w.decision().verdict,
            Verdict::Act(Action::Merge {
                head: w.snap.head.sha.clone(),
                method: MergeMethod::Squash
            })
        );
    }
    #[test]
    fn verdict_displays_items_without_comment_bodies() {
        let mut w = World::new();
        w.snap.threads.push(
            Thread::new(
                ThreadId("t".into()),
                false,
                false,
                "sandbox/stats.py".into(),
                Some(3),
                vec![comment(
                    1,
                    Actor::Bot(Login("github-actions".into())),
                    "long body",
                )],
            )
            .unwrap(),
        );
        assert_eq!(
            w.decision().verdict.to_string(),
            "Answering 1 review thread: sandbox/stats.py:3 (@github-actions)"
        );
    }
    #[test]
    fn bands_precede_the_next_band() {
        let mut w = World::new();
        w.snap.draft = true;
        w.local.dirty.push("x".into());
        assert_eq!(w.decision().rule, "local-dirty");
        w.snap
            .threads
            .push(thread(vec![bot(1), marked(2, tid(), Outcome::Fixed, 1)]));
        assert_eq!(w.decision().rule, "local-dirty");
        w.local.dirty.clear();
        w.snap.threads.push(thread(vec![bot(3)]));
        assert_eq!(w.decision().rule, "resolve-fixed");
        w.snap.threads.remove(0);
        w.snap.ci = Ci::Rollup {
            state: RollupState::Pending,
            failing: vec![],
            cancelled: vec![],
            pending: vec![],
        };
        assert_eq!(w.decision().rule, "unhandled-items");
        w.snap.threads.clear();
        assert_eq!(w.decision().rule, "ci-pending");
        w.snap.ci = Ci::NoChecks;
        assert_eq!(w.decision().rule, "draft");
        w.snap.draft = false;
        assert_eq!(w.decision().rule, "merge");
    }
    #[test]
    fn draft_waits_until_ready_then_merges() {
        let mut w = World::new();
        w.snap.draft = true;
        w.snap.merge_state = MergeState::Draft;
        w.snap.ci = Ci::Rollup {
            state: RollupState::Success,
            failing: vec![],
            cancelled: vec![],
            pending: vec![],
        };
        let decision = w.decision();
        assert_eq!(decision.rule, "draft");
        assert_eq!(
            decision.verdict,
            Verdict::Wait(
                "Draft is up to date. Watching for new commits, CI results, and review comments."
                    .into()
            )
        );
        let facts = Facts {
            snap: &w.snap,
            local: Some(&w.local),
            method: MergeMethod::Squash,
            stack: w.stack,
        };
        assert_eq!(
            gates(&facts).to_string(),
            "reviews=met checks=met branch=met merge=skipped"
        );
        w.snap.draft = false;
        w.snap.merge_state = MergeState::Clean;
        assert_eq!(
            w.decision().verdict,
            Verdict::Act(Action::Merge {
                head: w.snap.head.sha.clone(),
                method: MergeMethod::Squash,
            })
        );
    }
    #[test]
    fn draft_merge_stays_pending_until_earlier_phases_are_complete() {
        let mut w = World::new();
        for (draft, state) in [(true, MergeState::Clean), (false, MergeState::Draft)] {
            w.snap.draft = draft;
            w.snap.merge_state = state;
            w.snap.threads.push(thread(vec![bot(1)]));
            assert_eq!(
                w.gates().to_string(),
                "reviews=waiting checks=pending branch=pending merge=pending"
            );
            w.snap.threads.clear();
            w.snap.ci = Ci::Rollup {
                state: RollupState::Pending,
                failing: vec![],
                cancelled: vec![],
                pending: vec!["tests".into()],
            };
            assert_eq!(
                w.gates().to_string(),
                "reviews=met checks=waiting branch=met merge=pending"
            );
            w.snap.ci = Ci::NoChecks;
            assert_eq!(
                w.gates().to_string(),
                "reviews=met checks=met branch=met merge=skipped"
            );
        }
    }
    #[test]
    fn draft_work_precedes_waiting_for_ready() {
        let mut w = World::new();
        w.snap.draft = true;
        w.snap.merge_state = MergeState::Draft;
        w.snap.commits_behind = Some(3);
        assert_eq!(w.decision().verdict, Verdict::Act(Action::UpdateBranch));
        w.snap.route = MergeRoute::Queue {
            method: MergeMethod::Squash,
        };
        assert_eq!(w.decision().rule, "draft");
        w.snap.route = MergeRoute::Direct;
        w.snap.commits_behind = Some(0);
        let checks = vec![FailedCheck {
            name: "build".into(),
            source: CheckSource::Actions { job_id: 17 },
        }];
        w.snap.ci = Ci::Rollup {
            state: RollupState::Failure,
            failing: checks.clone(),
            cancelled: vec![],
            pending: vec![],
        };
        assert_eq!(
            w.decision().verdict,
            Verdict::Act(Action::FixCi {
                checks,
                head: w.snap.head.clone()
            })
        );
        w.snap.threads.push(thread(vec![bot(1)]));
        assert_eq!(w.decision().rule, "unhandled-items");
        assert!(matches!(
            w.decision().verdict,
            Verdict::Act(Action::HandleComments { .. })
        ));
    }
    #[test]
    fn failing_ci_waits_for_pending_checks_before_checking_the_attempt_limit() {
        let mut w = World::new();
        w.snap.ci_fix_attempts = 3;
        w.snap.ci = Ci::Rollup {
            state: RollupState::Failure,
            failing: vec![FailedCheck {
                name: "build".into(),
                source: CheckSource::Actions { job_id: 17 },
            }],
            cancelled: vec![],
            pending: vec!["test".into()],
        };
        assert_eq!(w.decision().rule, "ci-failed");
        assert_eq!(
            w.decision().verdict,
            Verdict::Wait("Waiting for 1 more check before fixing build".into())
        );
    }
    #[test]
    fn cancelled_actions_runs_are_deduplicated_and_rerun() {
        for state in [RollupState::Failure, RollupState::Error] {
            let mut w = World::new();
            w.snap.ci_fix_attempts = 3;
            w.snap.ci = Ci::Rollup {
                state,
                failing: vec![],
                cancelled: vec![
                    CancelledCheck {
                        name: "build".into(),
                        run_id: 12,
                    },
                    CancelledCheck {
                        name: "test".into(),
                        run_id: 12,
                    },
                    CancelledCheck {
                        name: "lint".into(),
                        run_id: 14,
                    },
                ],
                pending: vec![],
            };
            let decision = w.decision();
            assert_eq!(decision.rule, "rerun-cancelled");
            assert_eq!(
                decision.verdict.to_string(),
                "Rerunning cancelled CI: build, test and lint"
            );
            assert_eq!(
                decision.verdict,
                Verdict::Act(Action::RerunCi {
                    runs: BTreeSet::from([12, 14]),
                    checks: vec!["build".into(), "test".into(), "lint".into()],
                })
            );
        }
    }
    #[test]
    fn real_failures_are_fixed_without_including_cancelled_checks() {
        let mut w = World::new();
        let checks = vec![FailedCheck {
            name: "build".into(),
            source: CheckSource::Actions { job_id: 17 },
        }];
        w.snap.ci = Ci::Rollup {
            state: RollupState::Failure,
            failing: checks.clone(),
            cancelled: vec![CancelledCheck {
                name: "tests".into(),
                run_id: 12,
            }],
            pending: vec![],
        };
        assert_eq!(
            w.decision().verdict,
            Verdict::Act(Action::FixCi {
                checks,
                head: w.snap.head.clone(),
            })
        );
    }
    #[test]
    fn cancelled_checks_wait_for_pending_checks() {
        let mut w = World::new();
        w.snap.ci = Ci::Rollup {
            state: RollupState::Failure,
            failing: vec![],
            cancelled: vec![CancelledCheck {
                name: "build".into(),
                run_id: 12,
            }],
            pending: vec!["tests".into()],
        };
        assert_eq!(w.decision().rule, "ci-failed");
        assert_eq!(
            w.decision().verdict,
            Verdict::Wait("Waiting for 1 more check before fixing CI".into())
        );
    }
    #[test]
    fn failing_ci_is_fixed_after_local_and_review_work() {
        let mut w = World::new();
        let checks = vec![
            FailedCheck {
                name: "build".into(),
                source: CheckSource::Actions { job_id: 17 },
            },
            FailedCheck {
                name: "external".into(),
                source: CheckSource::External { url: None },
            },
        ];
        w.snap.ci = Ci::Rollup {
            state: RollupState::Error,
            failing: checks.clone(),
            cancelled: vec![],
            pending: vec![],
        };
        w.local.dirty.push(" M file".into());
        assert_eq!(w.decision().rule, "local-dirty");
        w.local.dirty.clear();
        w.snap.threads.push(thread(vec![bot(1)]));
        assert_eq!(w.decision().rule, "unhandled-items");
        w.snap.threads.clear();
        assert_eq!(w.decision().rule, "fix-ci");
        assert_eq!(
            w.decision().verdict,
            Verdict::Act(Action::FixCi {
                checks,
                head: w.snap.head.clone(),
            })
        );
        assert_eq!(
            w.decision().verdict.to_string(),
            "Fixing CI: build and external"
        );
        w.snap.ci_fix_attempts = 3;
        assert_eq!(w.decision().rule, "ci-failed");
        assert_eq!(
            w.decision().verdict,
            Verdict::Stop("CI still fails after 3 fix attempts: build and external".into())
        );
    }
    #[test]
    fn failed_ci_without_identified_checks_stops_instead_of_merging_or_fixing() {
        for state in [RollupState::Failure, RollupState::Error] {
            let mut w = World::new();
            w.snap.ci = Ci::Rollup {
                state,
                failing: vec![],
                cancelled: vec![],
                pending: vec![],
            };
            assert_eq!(w.decision().rule, "ci-failed");
            assert_eq!(
                w.decision().verdict,
                Verdict::Stop("CI failed, but GitHub didn't say which check".into())
            );
        }
    }
    #[test]
    fn successful_ci_does_not_trigger_a_fix() {
        let mut w = World::new();
        w.snap.ci = Ci::Rollup {
            state: RollupState::Success,
            failing: vec![],
            cancelled: vec![],
            pending: vec![],
        };
        w.snap.ci_fix_attempts = 3;
        assert_eq!(w.decision().rule, "merge");
    }
    #[test]
    fn marker_answers_only_the_shown_comment_and_manual_reply_answers_earlier_ones() {
        let mut w = World::new();
        w.snap.threads.push(thread(vec![
            bot(1),
            bot(3),
            marked(4, tid(), Outcome::Fixed, 1),
        ]));
        assert_eq!(w.decision().rule, "unhandled-items");
        w.snap.threads = vec![thread(vec![
            bot(1),
            marked(2, tid(), Outcome::Fixed, 1),
            bot(3),
        ])];
        assert_eq!(w.decision().rule, "unhandled-items");
        w.snap.threads = vec![thread(vec![
            bot(1),
            bot(3),
            comment(4, Actor::Viewer, "manual"),
        ])];
        assert_eq!(w.decision().rule, "unresolved-threads");
        w.snap.threads = vec![thread(vec![bot(1), marked(2, tid(), Outcome::Fixed, 1)])];
        assert_eq!(w.decision().rule, "resolve-fixed");
        w.snap.threads = vec![thread(vec![
            bot(1),
            comment(
                2,
                Actor::Viewer,
                "<!-- baby-sitter:v1 item=thread:t outcome=fixed answers=1 -->",
            ),
        ])];
        assert_eq!(w.decision().rule, "resolve-fixed");
    }
    #[test]
    fn submitted_review_author_is_eligible_for_rereview() {
        let mut w = World::new();
        let alice = Actor::Human(Login("alice".into()));
        w.snap
            .threads
            .push(thread(vec![comment(1, alice.clone(), "feedback")]));
        w.snap.reviews.push(Review {
            actor: alice,
            state: ReviewState::Commented,
        });
        let Verdict::Act(Action::HandleComments { batch, .. }) = w.decision().verdict else {
            panic!("expected review thread");
        };
        assert_eq!(batch.0[0].rereview, Some(Login("alice".into())));
    }
    #[test]
    fn merge_requires_all_gates_and_unknown_reviewer_blocks() {
        let mut w = World::new();
        assert_eq!(w.decision().rule, "merge");
        w.snap.review_requests.push(Reviewer::Unknown);
        assert_eq!(
            w.decision().rule,
            "merge",
            "an approved PR merges past a pending request"
        );
        w.snap.review_decision = None;
        assert_eq!(w.decision().rule, "review-requests");
        w.snap.review_requests.clear();
        w.snap.review_decision = Some(ReviewDecision::ReviewRequired);
        assert_eq!(w.decision().rule, "review-decision");
        w.snap.review_decision = Some(ReviewDecision::Approved);
        w.snap.mergeable = Mergeable::Unknown;
        assert_eq!(w.decision().rule, "mergeability-unknown");
        w.snap.mergeable = Mergeable::Yes;
        w.local.relation = Relation::Ahead;
        assert!(matches!(w.decision().verdict, Verdict::Stop(_)));
        w.local.relation = Relation::Pushed;
        let decision = w.decision();
        assert_eq!(decision.rule, "local-diverged");
        assert!(matches!(decision.verdict, Verdict::Wait(_)));
        w.local.relation = Relation::Unpushed;
        let decision = w.decision();
        assert_eq!(decision.rule, "local-diverged");
        assert!(matches!(
            decision.verdict,
            Verdict::Act(Action::PushLocal { ref base, .. }) if *base == w.snap.head.sha
        ));
    }
    #[test]
    fn unhandled_comments_block_a_behind_branch_and_pending_ci() {
        let mut w = World::new();
        w.snap.threads.push(thread(vec![bot(1)]));
        w.snap.merge_state = MergeState::Behind;
        w.snap.ci = Ci::Rollup {
            state: RollupState::Pending,
            failing: vec![],
            cancelled: vec![],
            pending: vec!["tests".into()],
        };
        assert_eq!(w.decision().rule, "unhandled-items");
        assert_eq!(
            w.gates().to_string(),
            "reviews=waiting checks=pending branch=pending merge=pending"
        );
    }

    #[test]
    fn behind_direct_branch_blocks_pending_ci() {
        let mut w = World::new();
        w.snap.merge_state = MergeState::Behind;
        w.snap.ci = Ci::Rollup {
            state: RollupState::Pending,
            failing: vec![],
            cancelled: vec![],
            pending: vec!["tests".into()],
        };
        assert_eq!(
            w.decision(),
            Decision {
                rule: "behind-base",
                verdict: Verdict::Act(Action::UpdateBranch),
            }
        );
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=pending branch=waiting merge=pending"
        );
    }

    #[test]
    fn pending_ci_blocks_merge() {
        let mut w = World::new();
        w.snap.ci = Ci::Rollup {
            state: RollupState::Pending,
            failing: vec![],
            cancelled: vec![],
            pending: vec!["tests".into()],
        };
        assert_eq!(w.decision().rule, "ci-pending");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=waiting branch=met merge=pending"
        );
    }

    #[test]
    fn queue_waits_in_merge_when_earlier_phases_are_complete() {
        let mut w = World::new();
        w.snap.route = MergeRoute::Queue {
            method: MergeMethod::Squash,
        };
        w.snap.merge_state = MergeState::Behind;
        w.snap.queue = QueueStatus::Queued {
            state: QueueEntryState::Queued,
            position: 2,
        };
        assert_eq!(
            w.decision(),
            Decision {
                rule: "in-queue",
                verdict: Verdict::Wait("In the merge queue at position 2, queued".into()),
            }
        );
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=met branch=met merge=waiting"
        );
    }

    #[test]
    fn dirty_checkout_blocks_before_comments_and_failed_ci() {
        let mut w = World::new();
        w.local.dirty.push(" M file".into());
        w.snap.threads.push(thread(vec![bot(1)]));
        w.snap.ci = Ci::Rollup {
            state: RollupState::Failure,
            failing: vec![],
            cancelled: vec![],
            pending: vec![],
        };
        assert_eq!(w.decision().rule, "local-dirty");
        assert_eq!(
            w.gates().to_string(),
            "reviews=pending checks=pending branch=blocked merge=pending"
        );
    }

    #[test]
    fn unknown_mergeability_waits_before_ci() {
        let mut w = World::new();
        w.snap.mergeable = Mergeable::Unknown;
        w.snap.ci = Ci::Rollup {
            state: RollupState::Pending,
            failing: vec![],
            cancelled: vec![],
            pending: vec!["tests".into()],
        };
        assert_eq!(
            w.decision(),
            Decision {
                rule: "mergeability-unknown",
                verdict: Verdict::Wait("Waiting for GitHub to check mergeability".into()),
            }
        );
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=pending branch=waiting merge=pending"
        );
    }

    #[test]
    fn earlier_phase_regressions_grey_out_later_phases() {
        let mut w = World::new();
        assert_eq!(w.decision().rule, "merge");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=met branch=met merge=waiting"
        );
        w.snap.ci = Ci::Rollup {
            state: RollupState::Pending,
            failing: vec![],
            cancelled: vec![],
            pending: vec![],
        };
        assert_eq!(w.decision().rule, "ci-pending");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=waiting branch=met merge=pending"
        );
        w.snap.merge_state = MergeState::Behind;
        assert_eq!(w.decision().rule, "behind-base");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=pending branch=waiting merge=pending"
        );
        w.snap.threads.push(thread(vec![bot(1)]));
        assert_eq!(w.decision().rule, "unhandled-items");
        assert_eq!(
            w.gates().to_string(),
            "reviews=waiting checks=pending branch=pending merge=pending"
        );
    }

    #[test]
    fn phase_stop_takes_precedence_over_a_wait_in_the_same_phase() {
        let mut w = World::new();
        w.snap.mergeable = Mergeable::Unknown;
        w.snap.merge_state = MergeState::Dirty;
        w.snap.conflict_attempts = 3;
        assert_eq!(w.decision().rule, "mergeability-unknown");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=pending branch=blocked merge=pending"
        );
    }

    #[test]
    fn checkout_waits_and_closed_prs_block_before_every_phase() {
        let mut w = World::new();
        for (relation, rule) in [
            (Relation::Behind, "local-behind"),
            (Relation::Pushed, "local-diverged"),
            (Relation::Unpushed, "local-diverged"),
        ] {
            w.local.relation = relation;
            assert_eq!(w.decision().rule, rule);
            assert_eq!(
                w.gates().to_string(),
                "reviews=pending checks=pending branch=waiting merge=pending"
            );
        }
        w.snap.lifecycle = Lifecycle::Closed;
        w.snap.draft = true;
        assert_eq!(w.decision().rule, "closed");
        assert_eq!(
            w.gates().to_string(),
            "reviews=pending checks=pending branch=blocked merge=pending"
        );
    }

    #[test]
    fn review_approvals_and_answered_threads_wait_in_merge() {
        let mut w = World::new();
        w.snap
            .threads
            .push(thread(vec![bot(1), comment(2, Actor::Viewer, "answered")]));
        w.snap.ci = Ci::Rollup {
            state: RollupState::Pending,
            failing: vec![],
            cancelled: vec![],
            pending: vec![],
        };
        assert_eq!(w.decision().rule, "ci-pending");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=waiting branch=met merge=pending"
        );
        w.snap.ci = Ci::NoChecks;
        assert_eq!(w.decision().rule, "unresolved-threads");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=met branch=met merge=waiting"
        );
        w.snap.threads.clear();
        w.snap.review_decision = Some(ReviewDecision::ReviewRequired);
        assert_eq!(w.decision().rule, "review-decision");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=met branch=met merge=waiting"
        );
    }

    #[test]
    fn queued_pr_runs_check_work_before_waiting_in_merge() {
        let mut w = World::new();
        w.snap.route = MergeRoute::Queue {
            method: MergeMethod::Squash,
        };
        w.snap.queue = QueueStatus::Queued {
            state: QueueEntryState::Queued,
            position: 1,
        };
        w.snap.ci = Ci::Rollup {
            state: RollupState::Pending,
            failing: vec![],
            cancelled: vec![],
            pending: vec!["tests".into()],
        };
        assert_eq!(w.decision().rule, "ci-pending");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=waiting branch=met merge=pending"
        );
        w.snap.ci = Ci::Rollup {
            state: RollupState::Failure,
            failing: vec![FailedCheck {
                name: "tests".into(),
                source: CheckSource::Actions { job_id: 1 },
            }],
            cancelled: vec![],
            pending: vec![],
        };
        assert_eq!(w.decision().rule, "fix-ci");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=waiting branch=met merge=pending"
        );
    }

    #[test]
    fn comments_and_conflicts_precede_even_a_stopping_ci_failure() {
        let mut w = World::new();
        w.snap.ci = Ci::Rollup {
            state: RollupState::Failure,
            failing: vec![],
            cancelled: vec![],
            pending: vec![],
        };
        w.snap.mergeable = Mergeable::Conflicting;
        w.snap.threads.push(thread(vec![bot(1)]));
        assert_eq!(w.decision().rule, "unhandled-items");
        assert_eq!(
            w.gates().to_string(),
            "reviews=waiting checks=pending branch=pending merge=pending"
        );
        w.snap.threads.clear();
        assert_eq!(w.decision().rule, "conflict");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=pending branch=waiting merge=pending"
        );
        w.snap.mergeable = Mergeable::Yes;
        assert_eq!(w.decision().rule, "ci-failed");
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=blocked branch=met merge=pending"
        );
    }

    #[test]
    fn merged_pr_reports_completed_phases_even_with_other_blockers() {
        let mut w = World::new();
        w.snap.lifecycle = Lifecycle::Merged;
        w.local.dirty.push(" M file".into());
        w.snap.threads.push(thread(vec![bot(1)]));
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=met branch=met merge=met"
        );
        w.snap.draft = true;
        assert_eq!(
            w.gates().to_string(),
            "reviews=met checks=met branch=met merge=skipped"
        );
    }
}
