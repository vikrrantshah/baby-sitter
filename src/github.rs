use crate::{domain::*, proc, wire};
use anyhow::{Context, bail};
use std::{process::Command, time::Instant};
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
                &format!("query={}", wire::SNAPSHOT_QUERY),
            ]),
            None,
            self.deadline,
        )?;
        wire::parse_snapshot(&self.pr, out.as_bytes())
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
    pub fn comment(&self, body: &str) -> anyhow::Result<()> {
        proc::run(
            Command::new("gh").args([
                "pr",
                "comment",
                &self.number(),
                "-R",
                &self.repo(),
                "--body-file",
                "-",
            ]),
            Some(body),
            self.deadline,
        )?;
        Ok(())
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
