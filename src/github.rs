use crate::{domain::*, proc, wire};
use anyhow::{Context, bail};
use std::{process::Command, time::Instant};

const MAX_JOB_LOG_BYTES: usize = 60_000;
pub struct Gh {
    pub pr: PrRef,
    deadline: Instant,
}
impl Gh {
    pub fn resolve(arg: Option<&str>, deadline: Instant) -> anyhow::Result<Self> {
        let mut cmd = Command::new("gh");
        cmd.args(["pr", "view"]);
        if let Some(arg) = arg {
            cmd.arg(arg);
        }
        cmd.args(["--json", "number,url"]);
        let v: serde_json::Value = serde_json::from_str(&proc::run(&mut cmd, None, deadline)?)?;
        let url = v["url"].as_str().context("missing PR URL")?;
        let path = url
            .strip_prefix("https://github.com/")
            .context("unsupported PR URL")?;
        let parts: Vec<_> = path.split('/').collect();
        if parts.len() < 4 || parts[2] != "pull" {
            bail!("invalid PR URL {url}");
        }
        Ok(Self {
            pr: PrRef {
                owner: parts[0].into(),
                repo: parts[1].into(),
                number: v["number"].as_u64().context("missing PR number")?,
            },
            deadline,
        })
    }
    fn repo(&self) -> String {
        format!("{}/{}", self.pr.owner, self.pr.repo)
    }
    fn number(&self) -> String {
        self.pr.number.to_string()
    }
    pub fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let base_response = proc::run(
            Command::new("gh").args([
                "api", "graphql", "-f", &format!("owner={}", self.pr.owner),
                "-f", &format!("repo={}", self.pr.repo), "-F", &format!("n={}", self.pr.number),
                "-f", "query=query($owner:String!,$repo:String!,$n:Int!){repository(owner:$owner,name:$repo){pullRequest(number:$n){baseRefName}}}",
            ]),
            None,
            self.deadline,
        )?;
        let base_json: serde_json::Value = serde_json::from_str(&base_response)?;
        let base = base_json["data"]["repository"]["pullRequest"]["baseRefName"]
            .as_str()
            .context("missing PR base branch")?;
        let out = proc::run(
            Command::new("gh").args([
                "api",
                "graphql",
                "-f",
                &format!("owner={}", self.pr.owner),
                "-f",
                &format!("repo={}", self.pr.repo),
                "-F",
                &format!("n={}", self.pr.number),
                "-f",
                &format!("base={base}"),
                "-f",
                &format!("query={}", wire::SNAPSHOT_QUERY),
            ]),
            None,
            self.deadline,
        )?;
        let response: serde_json::Value = serde_json::from_str(&out)?;
        if response["data"]["repository"]["pullRequest"]["baseRefName"].as_str() != Some(base) {
            bail!("base branch changed during snapshot");
        }
        wire::parse_snapshot(out.as_bytes())
    }
    pub fn failed_job_log(&self, job_id: u64) -> anyhow::Result<String> {
        let path = format!("repos/{}/actions/jobs/{job_id}/logs", self.repo());
        let raw = proc::run_tail(
            Command::new("gh").args(["api", "--allow-escape-sequences", &path]),
            MAX_JOB_LOG_BYTES,
            self.deadline,
        )?;
        let mut log = strip_terminal_escapes(&raw);
        truncate_job_log(&mut log);
        Ok(log)
    }
    fn mutation(&self, query: &str, fields: &[(&str, &str)]) -> anyhow::Result<()> {
        let mut cmd = Command::new("gh");
        cmd.args(["api", "graphql", "-f", &format!("query={query}")]);
        for (key, value) in fields {
            cmd.args(["-f", &format!("{key}={value}")]);
        }
        let out = proc::run(&mut cmd, None, self.deadline)?;
        let v: serde_json::Value = serde_json::from_str(&out)?;
        if let Some(errors) = v["errors"].as_array().filter(|x| !x.is_empty()) {
            bail!("GraphQL mutation failed: {errors:?}");
        }
        Ok(())
    }
    pub fn reply_to_thread(&self, id: &ThreadId, body: &str) -> anyhow::Result<()> {
        self.mutation("mutation($id:ID!,$body:String!){addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$id,body:$body}){comment{id}}}", &[("id", &id.0), ("body", body)])
    }
    pub fn resolve_thread(&self, id: &ThreadId) -> anyhow::Result<()> {
        self.mutation(
            "mutation($id:ID!){resolveReviewThread(input:{threadId:$id}){thread{id}}}",
            &[("id", &id.0)],
        )
    }
    pub fn request_reviewers(&self, users: &[Login]) -> anyhow::Result<()> {
        let path = format!(
            "repos/{}/pulls/{}/requested_reviewers",
            self.repo(),
            self.number()
        );
        let mut cmd = Command::new("gh");
        cmd.args(["api", "-X", "POST", &path]);
        for user in users {
            cmd.args(["-f", &format!("reviewers[]={}", user.0)]);
        }
        proc::run(&mut cmd, None, self.deadline)?;
        Ok(())
    }
    pub fn update_branch(&self) -> anyhow::Result<()> {
        proc::run(
            Command::new("gh").args(["pr", "update-branch", &self.number(), "-R", &self.repo()]),
            None,
            self.deadline,
        )?;
        Ok(())
    }
    pub fn enqueue(&self, pr_id: &str, head: &Sha) -> anyhow::Result<()> {
        let result = self.mutation(
            "mutation($id:ID!,$head:GitObjectID!){enqueuePullRequest(input:{pullRequestId:$id,expectedHeadOid:$head}){mergeQueueEntry{id}}}",
            &[("id", pr_id), ("head", head.as_str())],
        );
        if let Err(error) = result {
            let queued = proc::run(
                Command::new("gh").args([
                    "api", "graphql", "-f", &format!("owner={}", self.pr.owner),
                    "-f", &format!("repo={}", self.pr.repo), "-F", &format!("n={}", self.pr.number),
                    "-f", "query=query($owner:String!,$repo:String!,$n:Int!){repository(owner:$owner,name:$repo){pullRequest(number:$n){mergeQueueEntry{headCommit{oid}}}}}",
                ]),
                None,
                self.deadline,
            ).ok().and_then(|out| serde_json::from_str::<serde_json::Value>(&out).ok())
                .and_then(|v| v["data"]["repository"]["pullRequest"]["mergeQueueEntry"]["headCommit"]["oid"].as_str().map(str::to_owned))
                .is_some_and(|oid| oid.eq_ignore_ascii_case(head.as_str()));
            if !queued {
                return Err(error);
            }
        }
        Ok(())
    }
    pub fn merge(&self, method: MergeMethod, head: &Sha) -> anyhow::Result<()> {
        let flag = match method {
            MergeMethod::Squash => "--squash",
            MergeMethod::Merge => "--merge",
            MergeMethod::Rebase => "--rebase",
        };
        proc::run(
            Command::new("gh").args([
                "pr",
                "merge",
                &self.number(),
                "-R",
                &self.repo(),
                flag,
                "--match-head-commit",
                head.as_str(),
            ]),
            None,
            self.deadline,
        )?;
        Ok(())
    }
}

/// Actions logs carry ANSI color codes; they waste prompt space and can drive a terminal.
fn strip_terminal_escapes(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.next() == Some('[') {
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
            }
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

fn truncate_job_log(log: &mut String) {
    const MAX_LOG_BYTES: usize = MAX_JOB_LOG_BYTES;
    const MAX_LOG_LINES: usize = 400;
    let end = log.strip_suffix('\n').map_or(log.len(), str::len);
    let line_start = log[..end]
        .rmatch_indices('\n')
        .nth(MAX_LOG_LINES - 1)
        .map_or(0, |(index, _)| index + 1);
    let mut start = line_start.max(log.len().saturating_sub(MAX_LOG_BYTES));
    while !log.is_char_boundary(start) {
        start += 1;
    }
    log.drain(..start);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_logs_keep_the_last_400_lines() {
        let mut log = (1..=405).map(|n| format!("line {n}\n")).collect::<String>();
        truncate_job_log(&mut log);
        assert_eq!(log.lines().count(), 400);
        assert!(log.starts_with("line 6\n"));
        assert!(log.ends_with("line 405\n"));
    }

    #[test]
    fn job_logs_drop_terminal_escapes() {
        assert_eq!(
            strip_terminal_escapes("\u{1b}[31mFAIL\u{1b}[0m: test_mean\r\nok\u{1b}=\n"),
            "FAIL: test_mean\nok\n"
        );
    }

    #[test]
    fn job_logs_keep_the_last_60_kb_without_splitting_utf8() {
        let mut log = format!("{}end", "\u{1F600}".repeat(20_000));
        truncate_job_log(&mut log);
        assert!(log.len() <= 60_000);
        assert!(log.ends_with("end"));
        assert_eq!(log.chars().next(), Some('\u{1F600}'));
    }
}
