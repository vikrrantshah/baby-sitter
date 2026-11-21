use crate::domain::*;
use anyhow::{Context, bail};
use serde_json::Value;

pub const SNAPSHOT_QUERY: &str = r#"query($owner:String!,$repo:String!,$n:Int!,$base:String!){
 viewer{login} repository(owner:$owner,name:$repo){
 mergeQueue(branch:$base){configuration{mergeMethod}}
 pullRequest(number:$n){
 id state isDraft headRefName headRefOid baseRefName mergeable mergeStateStatus reviewDecision author{__typename login}
 mergeQueueEntry{state position}
 timelineItems(itemTypes:[ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT],last:1){nodes{__typename ... on RemovedFromMergeQueueEvent{reason beforeCommit{oid}}}}
 reviewRequests(first:100){pageInfo{hasNextPage} nodes{requestedReviewer{__typename ... on User{login} ... on Bot{login} ... on Team{slug}}}}
 commits(last:1){nodes{commit{oid statusCheckRollup{state contexts(first:100){nodes{__typename ... on CheckRun{name conclusion status} ... on StatusContext{context state}}}}}}}
 reviewThreads(first:100){pageInfo{hasNextPage} nodes{id isResolved isOutdated path line comments(last:100){pageInfo{hasPreviousPage} nodes{databaseId author{__typename login} body url}}}}
 comments(last:100){pageInfo{hasPreviousPage} nodes{databaseId author{__typename login} body url}}
 reviews(last:100){pageInfo{hasPreviousPage} nodes{databaseId author{__typename login} state body url}}
 }}}"#;
fn string<'a>(v: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("missing or invalid {key}"))
}
fn boolean(v: &Value, key: &str) -> anyhow::Result<bool> {
    v[key]
        .as_bool()
        .with_context(|| format!("missing or invalid {key}"))
}
fn number(v: &Value, key: &str) -> anyhow::Result<u64> {
    v[key]
        .as_u64()
        .with_context(|| format!("missing or invalid {key}"))
}
fn nodes<'a>(v: &'a Value, name: &str, direction: &str) -> anyhow::Result<&'a Vec<Value>> {
    let p = &v[name];
    if boolean(&p["pageInfo"], direction)? {
        bail!("{name} is truncated ({direction}); refusing partial snapshot");
    }
    p["nodes"]
        .as_array()
        .with_context(|| format!("missing {name} nodes"))
}
fn actor(v: &Value, viewer: &str) -> anyhow::Result<Actor> {
    if v.is_null() {
        return Ok(Actor::Ghost);
    }
    let login = string(v, "login")?;
    Ok(if login.eq_ignore_ascii_case(viewer) {
        Actor::Viewer
    } else if string(v, "__typename")? == "Bot" {
        Actor::Bot(Login(login.into()))
    } else {
        Actor::Human(Login(login.into()))
    })
}
fn comment(v: &Value, viewer: &str) -> anyhow::Result<Comment> {
    Ok(Comment::new(
        CommentId(number(v, "databaseId")?),
        actor(&v["author"], viewer)?,
        string(v, "body")?.into(),
        string(v, "url")?.into(),
    ))
}
macro_rules! parse_enum { ($s:expr, $($name:literal => $value:expr),+ $(,)?) => { match $s { $($name => $value,)+ other => bail!("unknown value {other:?}") } }; }
pub fn parse_snapshot(json: &[u8]) -> anyhow::Result<Snapshot> {
    let v: Value = serde_json::from_slice(json)?;
    if let Some(errors) = v["errors"].as_array().filter(|x| !x.is_empty()) {
        bail!("GraphQL errors: {errors:?}");
    }
    let data = &v["data"];
    let viewer = string(&data["viewer"], "login")?;
    let p = &data["repository"]["pullRequest"];
    if p.is_null() {
        bail!("PR not found");
    }
    let configuration = &data["repository"]["mergeQueue"];
    let route = if configuration.is_null() {
        MergeRoute::Direct
    } else {
        let method = parse_enum!(string(&configuration["configuration"], "mergeMethod")?,
            "SQUASH" => MergeMethod::Squash, "MERGE" => MergeMethod::Merge, "REBASE" => MergeMethod::Rebase);
        MergeRoute::Queue { method }
    };
    let entry = &p["mergeQueueEntry"];
    let queue = if !entry.is_null() {
        let state = parse_enum!(string(entry, "state")?,
            "QUEUED" => QueueEntryState::Queued, "AWAITING_CHECKS" => QueueEntryState::AwaitingChecks,
            "MERGEABLE" => QueueEntryState::Mergeable, "UNMERGEABLE" => QueueEntryState::Unmergeable,
            "LOCKED" => QueueEntryState::Locked);
        QueueStatus::Queued {
            state,
            position: u32::try_from(number(entry, "position")?)?,
        }
    } else {
        let events = p["timelineItems"]["nodes"]
            .as_array()
            .context("missing queue timeline")?;
        match events.last() {
            Some(event) if string(event, "__typename")? == "RemovedFromMergeQueueEvent" => {
                QueueStatus::Removed {
                    reason: string(event, "reason")?.into(),
                    before_commit: Sha::parse(string(&event["beforeCommit"], "oid")?)?,
                }
            }
            _ => QueueStatus::NotQueued,
        }
    };
    let lifecycle = parse_enum!(string(p,"state")?, "OPEN" => Lifecycle::Open, "CLOSED" => Lifecycle::Closed, "MERGED" => Lifecycle::Merged);
    let mergeable = parse_enum!(string(p,"mergeable")?, "MERGEABLE" => Mergeable::Yes, "CONFLICTING" => Mergeable::Conflicting, "UNKNOWN" => Mergeable::Unknown);
    let merge_state = parse_enum!(string(p,"mergeStateStatus")?, "CLEAN" => MergeState::Clean, "BEHIND" => MergeState::Behind, "BLOCKED" => MergeState::Blocked, "DIRTY" => MergeState::Dirty, "DRAFT" => MergeState::Draft, "HAS_HOOKS" => MergeState::HasHooks, "UNKNOWN" => MergeState::Unknown, "UNSTABLE" => MergeState::Unstable);
    let review_decision = match p["reviewDecision"].as_str() {
        None if p["reviewDecision"].is_null() => None,
        Some(s) => Some(
            parse_enum!(s, "APPROVED" => ReviewDecision::Approved, "CHANGES_REQUESTED" => ReviewDecision::ChangesRequested, "REVIEW_REQUIRED" => ReviewDecision::ReviewRequired),
        ),
        _ => bail!("invalid reviewDecision"),
    };
    let mut requests = Vec::new();
    for req in nodes(p, "reviewRequests", "hasNextPage")? {
        let r = &req["requestedReviewer"];
        requests.push(if r.is_null() {
            Reviewer::Unknown
        } else {
            match string(r, "__typename")? {
                "User" => Reviewer::User(Login(string(r, "login")?.into())),
                "Bot" => Reviewer::Bot(Login(string(r, "login")?.into())),
                "Team" => Reviewer::Team(string(r, "slug")?.into()),
                x => bail!("unknown reviewer type {x}"),
            }
        });
    }
    let head = Head {
        branch: string(p, "headRefName")?.into(),
        sha: Sha::parse(string(p, "headRefOid")?)?,
    };
    let commits = p["commits"]["nodes"]
        .as_array()
        .context("missing commits")?;
    let commit = &commits.last().context("PR has no head commit")?["commit"];
    if Sha::parse(string(commit, "oid")?)? != head.sha {
        bail!("head commit changed during snapshot");
    }
    let rollup = &commit["statusCheckRollup"];
    let ci = if rollup.is_null() {
        Ci::NoChecks
    } else {
        let state = parse_enum!(string(rollup,"state")?, "SUCCESS" => RollupState::Success, "FAILURE" => RollupState::Failure, "ERROR" => RollupState::Error, "PENDING" => RollupState::Pending, "EXPECTED" => RollupState::Expected);
        let mut failing = Vec::new();
        let mut pending = Vec::new();
        for c in rollup["contexts"]["nodes"]
            .as_array()
            .context("missing check contexts")?
        {
            match string(c, "__typename")? {
                "CheckRun" => {
                    let name = string(c, "name")?.to_owned();
                    if string(c, "status")? != "COMPLETED" {
                        pending.push(name);
                    } else if matches!(
                        c["conclusion"].as_str(),
                        Some(
                            "FAILURE"
                                | "TIMED_OUT"
                                | "CANCELLED"
                                | "ACTION_REQUIRED"
                                | "STARTUP_FAILURE"
                        )
                    ) {
                        failing.push(name);
                    }
                }
                "StatusContext" => {
                    let name = string(c, "context")?.to_owned();
                    match string(c, "state")? {
                        "FAILURE" | "ERROR" => failing.push(name),
                        "PENDING" | "EXPECTED" => pending.push(name),
                        _ => {}
                    }
                }
                x => bail!("unknown check context {x}"),
            }
        }
        Ci::Rollup {
            state,
            failing,
            pending,
        }
    };
    let mut threads = Vec::new();
    for t in nodes(p, "reviewThreads", "hasNextPage")? {
        let comments = nodes(t, "comments", "hasPreviousPage")?
            .iter()
            .map(|c| comment(c, viewer))
            .collect::<anyhow::Result<Vec<_>>>()?;
        threads.push(Thread::new(
            ThreadId(string(t, "id")?.into()),
            boolean(t, "isResolved")?,
            boolean(t, "isOutdated")?,
            string(t, "path")?.into(),
            t["line"].as_u64().map(|n| n as u32),
            comments,
        )?);
    }
    let comments = nodes(p, "comments", "hasPreviousPage")?
        .iter()
        .map(|c| comment(c, viewer))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut reviews = Vec::new();
    for r in nodes(p, "reviews", "hasPreviousPage")? {
        reviews.push(Review { id: ReviewId(number(r,"databaseId")?), actor: actor(&r["author"], viewer)?, state: parse_enum!(string(r,"state")?, "APPROVED" => ReviewState::Approved, "CHANGES_REQUESTED" => ReviewState::ChangesRequested, "COMMENTED" => ReviewState::Commented, "DISMISSED" => ReviewState::Dismissed, "PENDING" => ReviewState::Pending), body: string(r,"body")?.into(), url: string(r,"url")?.into() });
    }
    Ok(Snapshot {
        pr_id: string(p, "id")?.into(),
        route,
        queue,
        author: actor(&p["author"], viewer)?,
        lifecycle,
        draft: boolean(p, "isDraft")?,
        head,
        mergeable,
        merge_state,
        review_decision,
        review_requests: requests,
        ci,
        threads,
        comments,
        reviews,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_public_response_and_rejects_truncation() {
        let raw = include_bytes!("../tests/fixtures/cli-15102.json");
        let snapshot = parse_snapshot(raw).unwrap();
        assert!(matches!(
            snapshot.threads[0].comments()[0].actor,
            Actor::Bot(_)
        ));
        assert!(snapshot.review_requests.contains(&Reviewer::Unknown));
        let original: Value = serde_json::from_slice(raw).unwrap();
        for (connection, page) in [
            ("reviewThreads", "hasNextPage"),
            ("reviewRequests", "hasNextPage"),
            ("comments", "hasPreviousPage"),
            ("reviews", "hasPreviousPage"),
        ] {
            let mut modified = original.clone();
            modified["data"]["repository"]["pullRequest"][connection]["pageInfo"][page] =
                Value::Bool(true);
            assert!(
                parse_snapshot(&serde_json::to_vec(&modified).unwrap())
                    .unwrap_err()
                    .to_string()
                    .contains("truncated")
            );
        }
        let mut modified = original.clone();
        modified["data"]["repository"]["pullRequest"]["reviewThreads"]["nodes"][0]["comments"]["pageInfo"]
            ["hasPreviousPage"] = Value::Bool(true);
        assert!(parse_snapshot(&serde_json::to_vec(&modified).unwrap()).is_err());
        let mut modified = original;
        modified["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"] =
            Value::Null;
        assert_eq!(
            parse_snapshot(&serde_json::to_vec(&modified).unwrap())
                .unwrap()
                .ci,
            Ci::NoChecks
        );
    }
    #[test]
    fn parses_queue_route_entry_and_removal() {
        let mut v: Value =
            serde_json::from_slice(include_bytes!("../tests/fixtures/cli-15102.json")).unwrap();
        v["data"]["repository"]["mergeQueue"] =
            serde_json::json!({"configuration":{"mergeMethod":"SQUASH"}});
        v["data"]["repository"]["pullRequest"]["mergeQueueEntry"] =
            serde_json::json!({"state":"AWAITING_CHECKS","position":3});
        let snapshot = parse_snapshot(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(
            snapshot.route,
            MergeRoute::Queue {
                method: MergeMethod::Squash
            }
        );
        assert_eq!(
            snapshot.queue,
            QueueStatus::Queued {
                state: QueueEntryState::AwaitingChecks,
                position: 3
            }
        );
        v["data"]["repository"]["pullRequest"]["mergeQueueEntry"] = Value::Null;
        v["data"]["repository"]["pullRequest"]["timelineItems"]["nodes"] = serde_json::json!([{
            "__typename":"RemovedFromMergeQueueEvent", "reason":"checks failed",
            "beforeCommit":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}
        }]);
        let snapshot = parse_snapshot(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(
            snapshot.queue,
            QueueStatus::Removed {
                reason: "checks failed".into(),
                before_commit: Sha::parse("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap(),
            }
        );
    }
}
