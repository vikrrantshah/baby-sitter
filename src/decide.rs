use crate::domain::*;
use std::{collections::BTreeSet, fmt};

pub struct Facts<'a> {
    pub snap: &'a Snapshot,
    pub local: &'a LocalState,
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
    HandleComments { batch: Batch, head: Head },
    UpdateBranch,
    Merge { head: Sha, method: MergeMethod },
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
            Self::UpdateBranch => f.write_str("update branch"),
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
    pub id: ItemId,
    pub answers: u64,
    pub rereview: Option<Login>,
    pub actor: Actor,
    pub url: String,
    pub path: Option<String>,
    pub line: Option<u32>,
    pub context: String,
}
impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.id {
            ItemId::Thread(id) => {
                write!(f, "thread {}", self.path.as_deref().unwrap_or(&id.0))?;
                if let Some(line) = self.line {
                    write!(f, ":{line}")?;
                }
            }
            ItemId::Comment(id) => write!(f, "comment #{}", id.0)?,
            ItemId::Review(id) => write!(f, "review #{}", id.0)?,
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
    eval: fn(&Facts<'_>) -> Option<Verdict>,
}
const RULES: &[Rule] = &[
    Rule {
        name: "merged",
        eval: merged,
    },
    Rule {
        name: "closed",
        eval: closed,
    },
    Rule {
        name: "draft",
        eval: draft,
    },
    Rule {
        name: "conflict",
        eval: conflict,
    },
    Rule {
        name: "ci-failed",
        eval: ci_failed,
    },
    Rule {
        name: "local-upstream",
        eval: local_upstream,
    },
    Rule {
        name: "local-dirty",
        eval: local_dirty,
    },
    Rule {
        name: "local-diverged",
        eval: local_diverged,
    },
    Rule {
        name: "local-behind",
        eval: local_behind,
    },
    Rule {
        name: "resolve-fixed",
        eval: resolve_fixed,
    },
    Rule {
        name: "unhandled-items",
        eval: unhandled_items,
    },
    Rule {
        name: "behind-base",
        eval: behind_base,
    },
    Rule {
        name: "ci-pending",
        eval: ci_pending,
    },
    Rule {
        name: "merge-state",
        eval: merge_state,
    },
    Rule {
        name: "unresolved-threads",
        eval: unresolved_threads,
    },
    Rule {
        name: "review-requests",
        eval: review_requests,
    },
    Rule {
        name: "review-decision",
        eval: review_decision,
    },
];
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
            rule: "merge",
            verdict: Verdict::Act(Action::Merge {
                head: f.snap.head.sha.clone(),
                method: f.method,
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
    out.push_str(if fired {
        "   merge (not reached)\n"
    } else {
        "-> merge\n"
    });
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
            ..
        } => Some(Verdict::Stop(format!("CI failed: {}", failing.join(", ")))),
        _ => None,
    }
}
fn local_upstream(f: &Facts<'_>) -> Option<Verdict> {
    match &f.local.upstream_branch {
        None => Some(Verdict::Stop("local branch has no upstream".into())),
        Some(b) if *b != f.snap.head.branch => Some(Verdict::Stop(format!(
            "wrong upstream branch {b}; expected {}",
            f.snap.head.branch
        ))),
        _ => None,
    }
}
fn local_dirty(f: &Facts<'_>) -> Option<Verdict> {
    (!f.local.dirty.is_empty())
        .then(|| Verdict::Stop(format!("local tree is dirty: {}", f.local.dirty.join(", "))))
}
fn local_diverged(f: &Facts<'_>) -> Option<Verdict> {
    match f.local.relation {
        Relation::Ahead => Some(Verdict::Stop(
            "local branch has unpushed commits; push or reset".into(),
        )),
        Relation::Diverged => Some(Verdict::Stop(format!(
            "local head {} and PR head {} diverged",
            &f.local.head.as_str()[..7],
            &f.snap.head.sha.as_str()[..7]
        ))),
        _ => None,
    }
}
fn local_behind(f: &Facts<'_>) -> Option<Verdict> {
    (f.local.relation == Relation::Behind)
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
fn behind_base(f: &Facts<'_>) -> Option<Verdict> {
    (f.snap.merge_state == MergeState::Behind).then_some(Verdict::Act(Action::UpdateBranch))
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
    let handled: BTreeSet<_> = s
        .comments
        .iter()
        .filter_map(|c| c.marker())
        .filter(|m| match &m.item {
            ItemId::Comment(id) => id.0 == m.answers,
            ItemId::Review(id) => id.0 == m.answers,
            _ => false,
        })
        .map(|m| m.item.clone())
        .collect();
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
            id: ItemId::Thread(t.id.clone()),
            answers: newest.id.0,
            rereview: rereview(&newest.actor),
            actor: newest.actor.clone(),
            url: newest.url.clone(),
            path: Some(t.path.clone()),
            line: t.line,
            context,
        });
    }
    for c in &s.comments {
        let id = ItemId::Comment(c.id);
        if matches!(c.actor, Actor::Human(_)) && !handled.contains(&id) {
            items.push(Item {
                id,
                answers: c.id.0,
                rereview: rereview(&c.actor),
                actor: c.actor.clone(),
                url: c.url.clone(),
                path: None,
                line: None,
                context: format!("PR comment by {:?} {}: {}", c.actor, c.url, c.body),
            });
        }
    }
    for r in &s.reviews {
        let id = ItemId::Review(r.id);
        if matches!(r.actor, Actor::Human(_)) && !r.body.trim().is_empty() && !handled.contains(&id)
        {
            items.push(Item {
                id,
                answers: r.id.0,
                rereview: rereview(&r.actor),
                actor: r.actor.clone(),
                url: r.url.clone(),
                path: None,
                line: None,
                context: format!(
                    "Review by {:?} {:?} {}: {}",
                    r.actor, r.state, r.url, r.body
                ),
            });
        }
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
                    threads: vec![],
                    comments: vec![],
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
                local: &self.local,
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
    fn marked(id: u64, item: ItemId, outcome: Outcome, answers: u64) -> Comment {
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
    fn tid() -> ItemId {
        ItemId::Thread(ThreadId("t".into()))
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
        w.snap.comments.push(comment(
            123,
            Actor::Human(Login("alice".into())),
            "another body",
        ));
        assert_eq!(
            w.decision().verdict.to_string(),
            "handle 2 items: thread sandbox/stats.py:3 (github-actions), comment #123 (alice)"
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
        w.snap
            .comments
            .push(comment(3, Actor::Human(Login("bob".into())), "feedback"));
        assert_eq!(w.decision().rule, "resolve-fixed");
        w.snap.threads.clear();
        w.snap.ci = Ci::Rollup {
            state: RollupState::Pending,
            failing: vec![],
            pending: vec![],
        };
        assert_eq!(w.decision().rule, "unhandled-items");
        w.snap.comments.clear();
        assert_eq!(w.decision().rule, "ci-pending");
        w.snap.ci = Ci::NoChecks;
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
    }
    #[test]
    fn top_level_marker_handled_and_forgery_ignored() {
        let mut w = World::new();
        let id = ItemId::Comment(CommentId(9));
        w.snap
            .comments
            .push(comment(9, Actor::Human(Login("bob".into())), "feedback"));
        w.snap
            .comments
            .push(marked(10, id.clone(), Outcome::Fixed, 9));
        assert_eq!(w.decision().rule, "merge");
        w.snap.comments[1] = comment(
            10,
            Actor::Human(Login("evil".into())),
            &Marker {
                item: id,
                outcome: Outcome::Fixed,
                answers: 9,
            }
            .render(),
        );
        assert_eq!(w.decision().rule, "unhandled-items");
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
}
