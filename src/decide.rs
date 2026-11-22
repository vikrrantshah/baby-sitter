use crate::domain::*;
use std::fmt;

const MAX_CI_FIX_ATTEMPTS: u32 = 3;

pub struct Facts<'a> {
    pub snap: &'a Snapshot,
    pub local: Option<&'a LocalState>,
    pub method: MergeMethod,
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
    FixCi {
        checks: Vec<FailedCheck>,
        head: Head,
    },
    UpdateBranch,
    Enqueue {
        head: Sha,
    },
    Merge {
        head: Sha,
        method: MergeMethod,
    },
}
impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FastForward(sha) => write!(f, "fast-forward to {}", &sha.as_str()[..7]),
            Self::ResolveThreads(ids) => write!(f, "resolve {} fixed threads", ids.len()),
            Self::HandleComments { batch, .. } => {
                write!(f, "handle {} items", batch.0.len())?;
                for (index, item) in batch.0.iter().enumerate() {
                    write!(f, "{}{item}", if index == 0 { ": " } else { ", " })?;
                }
                Ok(())
            }
            Self::FixCi { checks, .. } => write!(f, "fix CI: {}", check_names(checks)),
            Self::UpdateBranch => f.write_str("update branch"),
            Self::Enqueue { head } => write!(f, "enqueue {}", &head.as_str()[..7]),
            Self::Merge { head, method } => {
                let method = match method {
                    MergeMethod::Squash => "squash",
                    MergeMethod::Merge => "merge",
                    MergeMethod::Rebase => "rebase",
                };
                write!(f, "merge {} ({method})", &head.as_str()[..7])
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
        write!(f, "thread {}", self.path)?;
        if let Some(line) = self.line {
            write!(f, ":{line}")?;
        }
        let author = match &self.actor {
            Actor::Human(login) | Actor::Bot(login) => login.0.as_str(),
            Actor::Viewer => "viewer",
            Actor::Ghost => "ghost",
        };
        write!(f, " ({author})")
    }
}
struct Rule {
    name: &'static str,
    gate: Gate,
    eval: fn(&Facts<'_>) -> Option<Verdict>,
}
const RULES: &[Rule] = &[
    Rule {
        name: "merged",
        gate: Gate::Merge,
        eval: merged,
    },
    Rule {
        name: "closed",
        gate: Gate::Merge,
        eval: closed,
    },
    Rule {
        name: "draft",
        gate: Gate::Merge,
        eval: draft,
    },
    Rule {
        name: "conflict",
        gate: Gate::Branch,
        eval: conflict,
    },
    Rule {
        name: "ci-failed",
        gate: Gate::Checks,
        eval: ci_failed,
    },
    Rule {
        name: "in-queue",
        gate: Gate::Merge,
        eval: in_queue,
    },
    Rule {
        name: "dequeued",
        gate: Gate::Merge,
        eval: dequeued,
    },
    Rule {
        name: "local-upstream",
        gate: Gate::Branch,
        eval: local_upstream,
    },
    Rule {
        name: "local-dirty",
        gate: Gate::Branch,
        eval: local_dirty,
    },
    Rule {
        name: "local-diverged",
        gate: Gate::Branch,
        eval: local_diverged,
    },
    Rule {
        name: "local-behind",
        gate: Gate::Branch,
        eval: local_behind,
    },
    Rule {
        name: "resolve-fixed",
        gate: Gate::Reviews,
        eval: resolve_fixed,
    },
    Rule {
        name: "unhandled-items",
        gate: Gate::Reviews,
        eval: unhandled_items,
    },
    Rule {
        name: "fix-ci",
        gate: Gate::Checks,
        eval: fix_ci,
    },
    Rule {
        name: "behind-base",
        gate: Gate::Branch,
        eval: behind_base,
    },
    Rule {
        name: "ci-pending",
        gate: Gate::Checks,
        eval: ci_pending,
    },
    Rule {
        name: "merge-state",
        gate: Gate::Merge,
        eval: merge_state,
    },
    Rule {
        name: "unresolved-threads",
        gate: Gate::Reviews,
        eval: unresolved_threads,
    },
    Rule {
        name: "review-requests",
        gate: Gate::Reviews,
        eval: review_requests,
    },
    Rule {
        name: "review-decision",
        gate: Gate::Reviews,
        eval: review_decision,
    },
];
/// The four requirements a PR meets on its way to merged, as reported by `--gates`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Reviews,
    Checks,
    Branch,
    Merge,
}
impl Gate {
    const ALL: [Gate; 4] = [Gate::Reviews, Gate::Checks, Gate::Branch, Gate::Merge];
    fn name(self) -> &'static str {
        match self {
            Gate::Reviews => "reviews",
            Gate::Checks => "checks",
            Gate::Branch => "branch",
            Gate::Merge => "merge",
        }
    }
}
/// Ordered from least to most severe, so the worst rule in a gate decides it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GateState {
    /// Merge only: other gates are still open, so merging has not started.
    Pending,
    Met,
    /// A rule waits on GitHub or a person, or the binary is acting on it.
    Waiting,
    /// A rule stops the run.
    Blocked,
}
impl GateState {
    fn name(self) -> &'static str {
        match self {
            GateState::Pending => "pending",
            GateState::Met => "met",
            GateState::Waiting => "waiting",
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
        for (index, (gate, state)) in Gate::ALL.iter().zip(self.0).enumerate() {
            let sep = if index == 0 { "" } else { " " };
            write!(f, "{sep}{}={}", gate.name(), state.name())?;
        }
        Ok(())
    }
}
/// Evaluates every rule, not just the first match, and files each verdict under its gate.
pub fn gates(f: &Facts<'_>) -> Gates {
    if f.snap.lifecycle == Lifecycle::Merged {
        return Gates([GateState::Met; 4]);
    }
    let mut states = [GateState::Met; 4];
    // Only rules that stop the PR or hold it in a queue; merging itself is decided below.
    let mut merge = None;
    for rule in RULES {
        // GitHub's merge state echoes the other gates (BLOCKED while reviews are
        // missing) and only ever waits, which the merge default below already shows.
        if rule.name == "merge-state" {
            continue;
        }
        let Some(verdict) = (rule.eval)(f) else {
            continue;
        };
        let state = match verdict {
            Verdict::Stop(_) => GateState::Blocked,
            Verdict::Done => GateState::Met,
            Verdict::Wait(_) | Verdict::Act(_) => GateState::Waiting,
        };
        if rule.gate == Gate::Merge {
            merge = merge.max(Some(state));
        } else {
            let slot = &mut states[rule.gate as usize];
            *slot = (*slot).max(state);
        }
    }
    let others_met = states[..3].iter().all(|s| *s == GateState::Met);
    states[Gate::Merge as usize] = match merge {
        Some(state) => state,
        None if others_met => GateState::Waiting,
        None => GateState::Pending,
    };
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
        .then(|| Verdict::Stop("PR is closed without merging".into()))
}
fn draft(f: &Facts<'_>) -> Option<Verdict> {
    (f.snap.draft || f.snap.merge_state == MergeState::Draft)
        .then(|| Verdict::Stop("PR is a draft".into()))
}
fn conflict(f: &Facts<'_>) -> Option<Verdict> {
    (f.snap.mergeable == Mergeable::Conflicting || f.snap.merge_state == MergeState::Dirty)
        .then(|| Verdict::Stop("PR has merge conflicts".into()))
}
fn ci_failed(f: &Facts<'_>) -> Option<Verdict> {
    match &f.snap.ci {
        Ci::Rollup {
            state: RollupState::Failure | RollupState::Error,
            failing,
            pending,
        } if !pending.is_empty() => Some(Verdict::Wait(format!(
            "CI failing: {}; waiting for {}",
            check_names(failing),
            pending.join(", ")
        ))),
        Ci::Rollup {
            state: RollupState::Failure | RollupState::Error,
            failing,
            ..
        } if failing.is_empty() => Some(Verdict::Stop(
            "CI failed, but GitHub reported no failing check".into(),
        )),
        Ci::Rollup {
            state: RollupState::Failure | RollupState::Error,
            failing,
            ..
        } if f.snap.ci_fix_attempts >= MAX_CI_FIX_ATTEMPTS => Some(Verdict::Stop(format!(
            "CI still failing after 3 fix attempts: {}",
            check_names(failing)
        ))),
        _ => None,
    }
}
fn in_queue(f: &Facts<'_>) -> Option<Verdict> {
    match &f.snap.queue {
        QueueStatus::Queued { state, position } => Some(Verdict::Wait(format!(
            "in merge queue: position {position}, {state}"
        ))),
        _ => None,
    }
}
fn dequeued(f: &Facts<'_>) -> Option<Verdict> {
    match &f.snap.queue {
        QueueStatus::Removed {
            reason,
            before_commit,
        } if *before_commit == f.snap.head.sha && !reason.eq_ignore_ascii_case("merged") => {
            Some(Verdict::Stop(format!("removed from merge queue: {reason}")))
        }
        _ => None,
    }
}
fn local_upstream(f: &Facts<'_>) -> Option<Verdict> {
    match &f.local?.upstream_branch {
        None => Some(Verdict::Stop("local branch has no upstream".into())),
        Some(b) if *b != f.snap.head.branch => Some(Verdict::Stop(format!(
            "wrong upstream branch {b}; expected {}",
            f.snap.head.branch
        ))),
        _ => None,
    }
}
fn local_dirty(f: &Facts<'_>) -> Option<Verdict> {
    let local = f.local?;
    (!local.dirty.is_empty())
        .then(|| Verdict::Stop(format!("local tree is dirty: {}", local.dirty.join(", "))))
}
fn local_diverged(f: &Facts<'_>) -> Option<Verdict> {
    let local = f.local?;
    match local.relation {
        Relation::Ahead => Some(Verdict::Stop(
            "local branch has unpushed commits; push or reset".into(),
        )),
        Relation::Diverged => Some(Verdict::Stop(format!(
            "local head {} and PR head {} diverged",
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
fn fix_ci(f: &Facts<'_>) -> Option<Verdict> {
    match &f.snap.ci {
        Ci::Rollup {
            state: RollupState::Failure | RollupState::Error,
            failing,
            pending,
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
    (f.snap.merge_state == MergeState::Behind && f.snap.route == MergeRoute::Direct)
        .then_some(Verdict::Act(Action::UpdateBranch))
}
fn ci_pending(f: &Facts<'_>) -> Option<Verdict> {
    match &f.snap.ci {
        Ci::Rollup {
            state: RollupState::Pending | RollupState::Expected,
            pending,
            ..
        } => Some(Verdict::Wait(format!("CI pending: {}", pending.join(", ")))),
        _ => None,
    }
}
fn merge_state(f: &Facts<'_>) -> Option<Verdict> {
    if f.snap.mergeable == Mergeable::Unknown {
        return Some(Verdict::Wait("mergeability unknown".into()));
    }
    match f.snap.merge_state {
        MergeState::Clean | MergeState::HasHooks => None,
        MergeState::Behind if matches!(f.snap.route, MergeRoute::Queue { .. }) => None,
        x => Some(Verdict::Wait(format!("merge state: {x:?}"))),
    }
}
fn unresolved_threads(f: &Facts<'_>) -> Option<Verdict> {
    let n = f.snap.threads.iter().filter(|t| !t.resolved).count();
    (n > 0).then(|| Verdict::Wait(format!("{n} unresolved review threads")))
}
fn review_requests(f: &Facts<'_>) -> Option<Verdict> {
    (!f.snap.review_requests.is_empty()).then(|| {
        Verdict::Wait(format!(
            "pending review requests: {:?}",
            f.snap.review_requests
        ))
    })
}
fn review_decision(f: &Facts<'_>) -> Option<Verdict> {
    match f.snap.review_decision {
        Some(d @ (ReviewDecision::ChangesRequested | ReviewDecision::ReviewRequired)) => {
            Some(Verdict::Wait(format!("review decision: {d:?}")))
        }
        _ => None,
    }
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
    struct World {
        snap: Snapshot,
        local: LocalState,
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
                    mergeable: Mergeable::Yes,
                    merge_state: MergeState::Clean,
                    review_decision: Some(ReviewDecision::Approved),
                    review_requests: vec![],
                    ci: Ci::NoChecks,
                    ci_fix_attempts: 0,
                    threads: vec![],
                    reviews: vec![],
                },
                local: LocalState {
                    upstream_branch: Some("feature".into()),
                    head: sha,
                    dirty: vec![],
                    relation: Relation::Equal,
                },
            }
        }
        fn decision(&self) -> Decision {
            decide(&Facts {
                snap: &self.snap,
                local: Some(&self.local),
                method: MergeMethod::Squash,
            })
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
        };
        assert_eq!(decide(&facts).verdict, Verdict::Done);
        assert!(local_upstream(&facts).is_none());
        assert!(local_dirty(&facts).is_none());
        assert!(local_diverged(&facts).is_none());
        assert!(local_behind(&facts).is_none());
    }
    #[test]
    fn queue_waits_before_feedback_and_behind_base() {
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
        assert_eq!(decision.rule, "in-queue");
        assert_eq!(
            decision.verdict,
            Verdict::Wait("in merge queue: position 2, awaiting checks".into())
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
            before_commit: w.snap.head.sha.clone(),
        };
        assert_eq!(
            w.decision().verdict,
            Verdict::Stop("removed from merge queue: checks failed".into())
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
            "handle 1 items: thread sandbox/stats.py:3 (github-actions)"
        );
    }
    #[test]
    fn bands_precede_the_next_band() {
        let mut w = World::new();
        w.snap.draft = true;
        w.local.dirty.push("x".into());
        assert_eq!(w.decision().rule, "draft");
        w.snap.draft = false;
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
            pending: vec![],
        };
        assert_eq!(w.decision().rule, "unhandled-items");
        w.snap.threads.clear();
        assert_eq!(w.decision().rule, "ci-pending");
        w.snap.ci = Ci::NoChecks;
        assert_eq!(w.decision().rule, "merge");
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
            pending: vec!["test".into()],
        };
        assert_eq!(w.decision().rule, "ci-failed");
        assert_eq!(
            w.decision().verdict,
            Verdict::Wait("CI failing: build; waiting for test".into())
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
        assert_eq!(w.decision().verdict.to_string(), "fix CI: build, external");
        w.snap.ci_fix_attempts = 3;
        assert_eq!(w.decision().rule, "ci-failed");
        assert_eq!(
            w.decision().verdict,
            Verdict::Stop("CI still failing after 3 fix attempts: build, external".into())
        );
    }
    #[test]
    fn failed_ci_without_identified_checks_stops_instead_of_merging_or_fixing() {
        for state in [RollupState::Failure, RollupState::Error] {
            let mut w = World::new();
            w.snap.ci = Ci::Rollup {
                state,
                failing: vec![],
                pending: vec![],
            };
            assert_eq!(w.decision().rule, "ci-failed");
            assert_eq!(
                w.decision().verdict,
                Verdict::Stop("CI failed, but GitHub reported no failing check".into())
            );
        }
    }
    #[test]
    fn successful_ci_does_not_trigger_a_fix() {
        let mut w = World::new();
        w.snap.ci = Ci::Rollup {
            state: RollupState::Success,
            failing: vec![],
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
        assert_eq!(w.decision().rule, "review-requests");
        w.snap.review_requests.clear();
        w.snap.review_decision = Some(ReviewDecision::ReviewRequired);
        assert_eq!(w.decision().rule, "review-decision");
        w.snap.review_decision = Some(ReviewDecision::Approved);
        w.snap.mergeable = Mergeable::Unknown;
        assert_eq!(w.decision().rule, "merge-state");
        w.snap.mergeable = Mergeable::Yes;
        w.local.relation = Relation::Ahead;
        assert!(matches!(w.decision().verdict, Verdict::Stop(_)));
    }
    #[test]
    fn gates_report_every_requirement_not_just_the_first_blocker() {
        let report = |w: &World| {
            gates(&Facts {
                snap: &w.snap,
                local: Some(&w.local),
                method: MergeMethod::Squash,
            })
        };
        let mut w = World::new();
        assert_eq!(
            report(&w).to_string(),
            "reviews=met checks=met branch=met merge=waiting"
        );
        w.snap.threads.push(thread(vec![bot(1)]));
        w.snap.ci = Ci::Rollup {
            state: RollupState::Pending,
            failing: vec![],
            pending: vec![],
        };
        w.snap.mergeable = Mergeable::Unknown;
        let g = report(&w);
        assert_eq!(
            Gate::ALL.map(|gate| g.get(gate)),
            [
                GateState::Waiting,
                GateState::Waiting,
                GateState::Met,
                GateState::Pending
            ]
        );
        w.snap.mergeable = Mergeable::Conflicting;
        assert_eq!(report(&w).get(Gate::Branch), GateState::Blocked);
        w.snap.lifecycle = Lifecycle::Closed;
        assert_eq!(report(&w).get(Gate::Merge), GateState::Blocked);
        w.snap.lifecycle = Lifecycle::Merged;
        assert_eq!(report(&w), Gates([GateState::Met; 4]));
    }
}
