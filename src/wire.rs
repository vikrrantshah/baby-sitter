use crate::domain::*;
use anyhow::{Context, bail};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const SNAPSHOT_QUERY: &str = r#"query($owner:String!,$repo:String!,$n:Int!,$base:String!){
 viewer{login} repository(owner:$owner,name:$repo){
 mergeQueue(branch:$base){configuration{mergeMethod}}
 pullRequest(number:$n){
 id state isDraft headRefName headRefOid baseRefName mergeable mergeStateStatus reviewDecision author{__typename login}
 headRef{compare(headRef:$base){aheadBy}}
 mergeQueueEntry{state position}
 timelineItems(itemTypes:[ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT],last:1){nodes{__typename ... on RemovedFromMergeQueueEvent{reason beforeCommit{oid}}}}
 reviewRequests(first:100){pageInfo{hasNextPage} nodes{requestedReviewer{__typename ... on User{login} ... on Bot{login} ... on Team{slug}}}}
 commits(last:1){nodes{commit{oid committedDate checkSuites(first:100){pageInfo{hasNextPage} nodes{status app{slug}}} statusCheckRollup{state contexts(first:100){pageInfo{hasNextPage} nodes{__typename ... on CheckRun{name conclusion status databaseId detailsUrl checkSuite{app{slug} workflowRun{databaseId}}} ... on StatusContext{context state targetUrl}}}}}}}
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
fn commit_age(value: &str, now: SystemTime) -> Option<Duration> {
    let bytes = value.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return None;
    }
    let digits = |slice: &[u8]| {
        slice.iter().try_fold(0_i64, |n, &b| {
            b.is_ascii_digit().then(|| n * 10 + i64::from(b - b'0'))
        })
    };
    let year = digits(&bytes[0..4])?;
    let month = digits(&bytes[5..7])?;
    let day = digits(&bytes[8..10])?;
    let hour = digits(&bytes[11..13])?;
    let minute = digits(&bytes[14..16])?;
    let second = digits(&bytes[17..19])?;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=month_days).contains(&day) || hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let march_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * march_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second;
    let offset = Duration::from_secs(seconds.unsigned_abs());
    let committed = if seconds < 0 {
        UNIX_EPOCH.checked_sub(offset)?
    } else {
        UNIX_EPOCH.checked_add(offset)?
    };
    Some(now.duration_since(committed).unwrap_or(Duration::ZERO))
}
pub fn parse_snapshot(json: &[u8], now: SystemTime) -> anyhow::Result<Snapshot> {
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
        let mut pending = Vec::new();
        for suite in nodes(commit, "checkSuites", "hasNextPage")? {
            if string(suite, "status")? != "COMPLETED" {
                let app = &suite["app"];
                let name = if app.is_null() {
                    "CI"
                } else {
                    string(app, "slug")?
                };
                pending.push(format!("{name} checks"));
            }
        }
        if pending.is_empty() {
            Ci::NoChecks
        } else {
            Ci::Rollup {
                state: RollupState::Pending,
                failing: Vec::new(),
                cancelled: Vec::new(),
                pending,
            }
        }
    } else {
        let state = parse_enum!(string(rollup,"state")?, "SUCCESS" => RollupState::Success, "FAILURE" => RollupState::Failure, "ERROR" => RollupState::Error, "PENDING" => RollupState::Pending, "EXPECTED" => RollupState::Expected);
        let mut failing = Vec::new();
        let mut cancelled = Vec::new();
        let mut pending = Vec::new();
        for c in nodes(rollup, "contexts", "hasNextPage")? {
            match string(c, "__typename")? {
                "CheckRun" => {
                    let name = string(c, "name")?.to_owned();
                    if string(c, "status")? != "COMPLETED" {
                        pending.push(name);
                    } else if matches!(
                        c["conclusion"].as_str(),
                        Some("CANCELLED" | "STARTUP_FAILURE")
                    ) && c["checkSuite"]["app"]["slug"].as_str() == Some("github-actions")
                        && let Some(run_id) = c["checkSuite"]["workflowRun"]["databaseId"].as_u64()
                    {
                        cancelled.push(CancelledCheck { name, run_id });
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
            cancelled,
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
        head_age: commit["committedDate"]
            .as_str()
            .and_then(|date| commit_age(date, now)),
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
        p["commits"]["nodes"][0]["commit"]["checkSuites"] =
            serde_json::json!({"pageInfo":{"hasNextPage":false},"nodes":[]});
        v
    }
    fn parse_snapshot(json: &[u8]) -> anyhow::Result<Snapshot> {
        super::parse_snapshot(json, std::time::UNIX_EPOCH)
    }
    #[test]
    fn parses_head_age_from_validated_commit_date() {
        use std::time::{Duration, UNIX_EPOCH};

        let now = UNIX_EPOCH + Duration::from_secs(1_709_251_200);
        let mut v = fixture();
        let commit = &mut v["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"];
        commit["committedDate"] = "2024-02-29T23:59:50Z".into();
        let snapshot = super::parse_snapshot(&serde_json::to_vec(&v).unwrap(), now).unwrap();
        assert_eq!(snapshot.head_age, Some(Duration::from_secs(10)));

        v["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["committedDate"] =
            "2024-03-01T00:00:10Z".into();
        let snapshot = super::parse_snapshot(&serde_json::to_vec(&v).unwrap(), now).unwrap();
        assert_eq!(snapshot.head_age, Some(Duration::ZERO));

        v["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["committedDate"] =
            "1969-12-31T23:59:50Z".into();
        let snapshot = super::parse_snapshot(&serde_json::to_vec(&v).unwrap(), UNIX_EPOCH).unwrap();
        assert_eq!(snapshot.head_age, Some(Duration::from_secs(10)));
    }
    #[test]
    fn absent_or_invalid_head_commit_dates_have_unknown_age() {
        use std::time::UNIX_EPOCH;

        for value in [
            Value::Null,
            "2023-02-29T12:00:00Z".into(),
            "2024-02-30T12:00:00Z".into(),
            "2024-01-01T24:00:00Z".into(),
            "2024-01-01T00:60:00Z".into(),
            "2024-01-01T00:00:60Z".into(),
            "2024-01-01 00:00:00Z".into(),
            "not a date".into(),
        ] {
            let mut v = fixture();
            v["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["committedDate"] =
                value;
            let snapshot =
                super::parse_snapshot(&serde_json::to_vec(&v).unwrap(), UNIX_EPOCH).unwrap();
            assert_eq!(snapshot.head_age, None);
        }
        let snapshot =
            super::parse_snapshot(&serde_json::to_vec(&fixture()).unwrap(), UNIX_EPOCH).unwrap();
        assert_eq!(snapshot.head_age, None);
    }
    #[test]
    fn waits_for_queued_suite_before_rollup_exists() {
        let mut v = fixture();
        let commit = &mut v["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"];
        commit["statusCheckRollup"] = Value::Null;
        commit["checkSuites"]["nodes"] = serde_json::json!([
            {"status":"COMPLETED","app":{"slug":"other"}},
            {"status":"QUEUED","app":{"slug":"github-actions"}},
            {"status":"IN_PROGRESS","app":null}
        ]);
        assert_eq!(
            parse_snapshot(&serde_json::to_vec(&v).unwrap()).unwrap().ci,
            Ci::Rollup {
                state: RollupState::Pending,
                failing: vec![],
                cancelled: vec![],
                pending: vec!["github-actions checks".into(), "CI checks".into()],
            }
        );
        v["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["checkSuites"]["pageInfo"]
            ["hasNextPage"] = Value::Bool(true);
        assert!(
            parse_snapshot(&serde_json::to_vec(&v).unwrap())
                .unwrap_err()
                .to_string()
                .contains("checkSuites is truncated")
        );
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
                cancelled: vec![],
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
    fn separates_cancelled_actions_runs_from_other_failures() {
        let mut v = fixture();
        v["data"]["repository"]["pullRequest"]["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = serde_json::json!({
            "state": "FAILURE",
            "contexts": {
                "pageInfo": {"hasNextPage": false},
                "nodes": [
                    {"__typename": "CheckRun", "name": "build", "status": "COMPLETED",
                     "conclusion": "CANCELLED", "databaseId": 10,
                     "checkSuite": {"app": {"slug": "github-actions"},
                                    "workflowRun": {"databaseId": 200}}},
                    {"__typename": "CheckRun", "name": "startup", "status": "COMPLETED",
                     "conclusion": "STARTUP_FAILURE", "databaseId": 11,
                     "checkSuite": {"app": {"slug": "github-actions"},
                                    "workflowRun": {"databaseId": 201}}},
                    {"__typename": "CheckRun", "name": "external", "status": "COMPLETED",
                     "conclusion": "CANCELLED", "databaseId": 12,
                     "detailsUrl": "https://example.com/external",
                     "checkSuite": {"app": {"slug": "other"},
                                    "workflowRun": {"databaseId": 202}}},
                    {"__typename": "CheckRun", "name": "no run", "status": "COMPLETED",
                     "conclusion": "CANCELLED", "databaseId": 13,
                     "checkSuite": {"app": {"slug": "github-actions"}, "workflowRun": null}},
                    {"__typename": "CheckRun", "name": "failed", "status": "COMPLETED",
                     "conclusion": "FAILURE", "databaseId": 14,
                     "checkSuite": {"app": {"slug": "github-actions"},
                                    "workflowRun": {"databaseId": 200}}}
                ]
            }
        });
        let snapshot = parse_snapshot(&serde_json::to_vec(&v).unwrap()).unwrap();
        assert_eq!(
            snapshot.ci,
            Ci::Rollup {
                state: RollupState::Failure,
                failing: vec![
                    FailedCheck {
                        name: "external".into(),
                        source: CheckSource::External {
                            url: Some("https://example.com/external".into()),
                        },
                    },
                    FailedCheck {
                        name: "no run".into(),
                        source: CheckSource::Actions { job_id: 13 },
                    },
                    FailedCheck {
                        name: "failed".into(),
                        source: CheckSource::Actions { job_id: 14 },
                    },
                ],
                cancelled: vec![
                    CancelledCheck {
                        name: "build".into(),
                        run_id: 200,
                    },
                    CancelledCheck {
                        name: "startup".into(),
                        run_id: 201,
                    },
                ],
                pending: vec![],
            }
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
