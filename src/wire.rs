use crate::domain::*;
use anyhow::{Context, bail};
use serde_json::Value;

pub const SNAPSHOT_QUERY: &str = r#"query($owner:String!,$repo:String!,$n:Int!,$base:String!){
 viewer{login} repository(owner:$owner,name:$repo){
 mergeQueue(branch:$base){configuration{mergeMethod}}
 pullRequest(number:$n){
 id state isDraft headRefName headRefOid baseRefName mergeable mergeStateStatus reviewDecision author{__typename login}
 headRef{compare(headRef:$base){aheadBy}}
 mergeQueueEntry{state position}
 timelineItems(itemTypes:[ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT],last:1){nodes{__typename ... on RemovedFromMergeQueueEvent{reason beforeCommit{oid}}}}
 reviewRequests(first:100){pageInfo{hasNextPage} nodes{requestedReviewer{__typename ... on User{login} ... on Bot{login} ... on Team{slug}}}}
 commits(last:1){nodes{commit{oid statusCheckRollup{state contexts(first:100){pageInfo{hasNextPage} nodes{__typename ... on CheckRun{name conclusion status databaseId detailsUrl checkSuite{app{slug}}} ... on StatusContext{context state targetUrl}}}}}}}
 recent: commits(last:20){nodes{commit{oid message}}}
 reviewThreads(first:100){pageInfo{hasNextPage} nodes{id isResolved isOutdated path line comments(last:100){pageInfo{hasPreviousPage} nodes{databaseId author{__typename login} body url}}}}
 reviews(last:100){pageInfo{hasPreviousPage} nodes{author{__typename login} state}}
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
    let recent = p["recent"]["nodes"]
        .as_array()
        .context("missing recent commits")?;
    let recent_head = &recent.last().context("PR has no recent head commit")?["commit"];
    if Sha::parse(string(recent_head, "oid")?)? != head.sha {
        bail!("head commit changed during snapshot");
    }
    let mut ci_fix_attempts = 0;
    for node in recent.iter().rev().take(20) {
        if string(&node["commit"], "message")?
            .lines()
            .any(|line| line == "Baby-Sitter: fix-ci")
        {
            ci_fix_attempts += 1;
        }
    }
    let rollup = &commit["statusCheckRollup"];
    let ci = if rollup.is_null() {
        Ci::NoChecks
    } else {
        let state = parse_enum!(string(rollup,"state")?, "SUCCESS" => RollupState::Success, "FAILURE" => RollupState::Failure, "ERROR" => RollupState::Error, "PENDING" => RollupState::Pending, "EXPECTED" => RollupState::Expected);
        let mut failing = Vec::new();
        let mut pending = Vec::new();
        for c in nodes(rollup, "contexts", "hasNextPage")? {
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
                        let source =
                            if c["checkSuite"]["app"]["slug"].as_str() == Some("github-actions") {
                                CheckSource::Actions {
                                    job_id: number(c, "databaseId")?,
                                }
                            } else {
                                CheckSource::External {
                                    url: c["detailsUrl"].as_str().map(str::to_owned),
                                }
                            };
                        failing.push(FailedCheck { name, source });
                    }
                }
                "StatusContext" => {
                    let name = string(c, "context")?.to_owned();
                    match string(c, "state")? {
                        "FAILURE" | "ERROR" => failing.push(FailedCheck {
                            name,
                            source: CheckSource::External {
                                url: c["targetUrl"].as_str().map(str::to_owned),
                            },
                        }),
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
    let mut reviews = Vec::new();
    for r in nodes(p, "reviews", "hasPreviousPage")? {
        reviews.push(Review { actor: actor(&r["author"], viewer)?, state: parse_enum!(string(r,"state")?, "APPROVED" => ReviewState::Approved, "CHANGES_REQUESTED" => ReviewState::ChangesRequested, "COMMENTED" => ReviewState::Commented, "DISMISSED" => ReviewState::Dismissed, "PENDING" => ReviewState::Pending) });
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
        // Null when the head branch is gone or GitHub cannot compare; only draft updates need it.
        commits_behind: p["headRef"]["compare"]["aheadBy"].as_u64(),
        review_decision,
        review_requests: requests,
        ci,
        ci_fix_attempts,
        threads,
        reviews,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        let mut v: Value =
            serde_json::from_slice(include_bytes!("../tests/fixtures/cli-15102.json")).unwrap();
        let p = &mut v["data"]["repository"]["pullRequest"];
        p["recent"] = serde_json::json!({"nodes":[{"commit":{
            "oid": p["headRefOid"], "message":"existing commit"
        }}]});
        v
    }
    #[test]
    fn parses_commits_behind_and_missing_head_ref() {
        let mut v = fixture();
        let p = &mut v["data"]["repository"]["pullRequest"];
        p["headRef"] = serde_json::json!({"compare": {"aheadBy": 3}});
        let snapshot = parse_snapshot(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(snapshot.commits_behind, Some(3));
        v["data"]["repository"]["pullRequest"]["headRef"] = Value::Null;
        let snapshot = parse_snapshot(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(snapshot.commits_behind, None);
    }
    #[test]
    fn top_level_feedback_does_not_block_merge() {
        let mut v = fixture();
        let p = &mut v["data"]["repository"]["pullRequest"];
        p["state"] = "OPEN".into();
        p["mergeStateStatus"] = "CLEAN".into();
        p["reviewDecision"] = "APPROVED".into();
        p["reviewRequests"]["nodes"] = serde_json::json!([]);
        p["reviewThreads"]["nodes"] = serde_json::json!([]);
        p["comments"] = serde_json::json!({"pageInfo":{"hasPreviousPage":false},"nodes":[{
            "databaseId":1,"author":{"__typename":"User","login":"alice"},
            "body":"Top-level feedback","url":"https://example.com/comment"
        }]});
        p["reviews"]["nodes"] = serde_json::json!([{
            "databaseId":2,"author":{"__typename":"User","login":"alice"},
            "state":"APPROVED","body":"Review feedback","url":"https://example.com/review"
        }]);
        let snapshot = parse_snapshot(&serde_json::to_vec(&v).unwrap()).unwrap();
        let facts = crate::decide::Facts {
            snap: &snapshot,
            local: None,
            method: MergeMethod::Squash,
        };
        assert_eq!(crate::decide::decide(&facts).rule, "merge");
        assert!(crate::decide::explain(&facts).contains("unhandled-items      pass"));
    }
    #[test]
    fn parses_public_response_and_rejects_truncation() {
        let original = fixture();
        let raw = serde_json::to_vec(&original).unwrap();
        let snapshot = parse_snapshot(&raw).unwrap();
        assert!(matches!(
            snapshot.threads[0].comments()[0].actor,
            Actor::Bot(_)
        ));
        assert!(snapshot.review_requests.contains(&Reviewer::Unknown));
        for (connection, page) in [
            ("reviewThreads", "hasNextPage"),
            ("reviewRequests", "hasNextPage"),
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
        modified["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"]
            ["contexts"]["pageInfo"]["hasNextPage"] = Value::Bool(true);
        assert!(
            parse_snapshot(&serde_json::to_vec(&modified).unwrap())
                .unwrap_err()
                .to_string()
                .contains("contexts is truncated")
        );
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
        let mut v = fixture();
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
    #[test]
    fn parses_failed_check_sources_and_recent_fix_commits() {
        let mut v = fixture();
        let ci: Value =
            serde_json::from_slice(include_bytes!("../tests/fixtures/ci-failures.json")).unwrap();
        let p = &mut v["data"]["repository"]["pullRequest"];
        p["recent"] = ci["recent"].clone();
        p["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = ci["rollup"].clone();
        let snapshot = parse_snapshot(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(snapshot.ci_fix_attempts, 2);
        assert_eq!(
            snapshot.ci,
            Ci::Rollup {
                state: RollupState::Failure,
                failing: vec![
                    FailedCheck {
                        name: "build".into(),
                        source: CheckSource::Actions { job_id: 42 },
                    },
                    FailedCheck {
                        name: "analysis".into(),
                        source: CheckSource::External {
                            url: Some("https://example.com/analysis".into()),
                        },
                    },
                    FailedCheck {
                        name: "deploy".into(),
                        source: CheckSource::External { url: None },
                    },
                ],
                pending: vec!["tests".into()],
            }
        );
        let p = &mut v["data"]["repository"]["pullRequest"];
        p["recent"]["nodes"][0]["commit"]["message"] = "fix: first\n\nBaby-Sitter: fix-ci".into();
        assert_eq!(
            parse_snapshot(&serde_json::to_vec(&v).unwrap())
                .unwrap()
                .ci_fix_attempts,
            3
        );
        v["data"]["repository"]["pullRequest"]["recent"]["nodes"][2]["commit"]["message"] =
            "fix: manual\n\nNot-Baby-Sitter: fix-ci".into();
        assert_eq!(
            parse_snapshot(&serde_json::to_vec(&v).unwrap())
                .unwrap()
                .ci_fix_attempts,
            2
        );
    }
    #[test]
    fn counts_fix_commits_separated_by_a_merge() {
        let mut v = fixture();
        let p = &mut v["data"]["repository"]["pullRequest"];
        p["recent"]["nodes"] = serde_json::json!([
            {"commit":{"oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","message":"fix: build\n\nBaby-Sitter: fix-ci"}},
            {"commit":{"oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","message":"Merge base branch"}},
            {"commit":{"oid":p["headRefOid"],"message":"fix: tests\n\nBaby-Sitter: fix-ci"}}
        ]);
        assert_eq!(
            parse_snapshot(&serde_json::to_vec(&v).unwrap())
                .unwrap()
                .ci_fix_attempts,
            2
        );
    }
    #[test]
    fn ignores_fix_commits_older_than_the_last_twenty() {
        let mut v = fixture();
        let p = &mut v["data"]["repository"]["pullRequest"];
        let mut recent = vec![serde_json::json!({"commit":{
            "oid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "message":"old fix\n\nBaby-Sitter: fix-ci"
        }})];
        recent.extend((0..17).map(|_| {
            serde_json::json!({"commit":{
                "oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb", "message":"ordinary commit"
            }})
        }));
        recent.extend([
            serde_json::json!({"commit":{
                "oid":"cccccccccccccccccccccccccccccccccccccccc",
                "message":"fix build\n\nBaby-Sitter: fix-ci"
            }}),
            serde_json::json!({"commit":{
                "oid":"dddddddddddddddddddddddddddddddddddddddd", "message":"merge base"
            }}),
            serde_json::json!({"commit":{
                "oid":p["headRefOid"], "message":"fix tests\n\nBaby-Sitter: fix-ci"
            }}),
        ]);
        p["recent"]["nodes"] = recent.into();
        assert_eq!(
            parse_snapshot(&serde_json::to_vec(&v).unwrap())
                .unwrap()
                .ci_fix_attempts,
            2
        );
    }
    #[test]
    fn rejects_inconsistent_recent_head_and_missing_actions_job_id() {
        let mut v = fixture();
        v["data"]["repository"]["pullRequest"]["recent"]["nodes"][0]["commit"]["oid"] =
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        assert!(parse_snapshot(&serde_json::to_vec(&v).unwrap()).is_err());

        let mut v = fixture();
        let ci: Value =
            serde_json::from_slice(include_bytes!("../tests/fixtures/ci-failures.json")).unwrap();
        let rollup = &mut v["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"];
        *rollup = ci["rollup"].clone();
        rollup["contexts"]["nodes"][0]["databaseId"] = Value::Null;
        assert!(parse_snapshot(&serde_json::to_vec(&v).unwrap()).is_err());
    }
}
