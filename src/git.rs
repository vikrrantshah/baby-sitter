use crate::{domain::*, proc};
use anyhow::Context;
use std::{path::PathBuf, process::Command, time::Instant};
pub struct Git {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    remote: Option<String>,
    pub deadline: Instant,
}
impl Git {
    pub fn discover(deadline: Instant) -> anyhow::Result<Self> {
        let run = |args: &[&str]| proc::run(Command::new("git").args(args), None, deadline);
        let root = PathBuf::from(run(&["rev-parse", "--show-toplevel"])?.trim());
        let git_dir = PathBuf::from(run(&["rev-parse", "--absolute-git-dir"])?.trim());
        let branch = optional(run(&["symbolic-ref", "--quiet", "--short", "HEAD"]))?;
        let remote = match branch {
            Some(b) => optional(run(&[
                "config",
                "--get",
                &format!("branch.{}.remote", b.trim()),
            ]))?,
            None => None,
        }
        .map(|s| s.trim().to_owned());
        Ok(Self {
            root,
            git_dir,
            remote,
            deadline,
        })
    }
    pub fn state_dir(&self) -> PathBuf {
        self.git_dir.join("baby-sitter")
    }
    fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        proc::run(
            Command::new("git").current_dir(&self.root).args(args),
            None,
            self.deadline,
        )
    }
    pub fn head(&self) -> anyhow::Result<Sha> {
        Sha::parse(&self.run(&["rev-parse", "HEAD"])?)
    }
    pub fn is_clean(&self) -> anyhow::Result<bool> {
        Ok(self.run(&["status", "--porcelain"])?.is_empty())
    }
    pub fn observe(&self, remote_head: &Sha) -> anyhow::Result<LocalState> {
        let branch = optional(self.run(&["symbolic-ref", "--quiet", "--short", "HEAD"]))?;
        let upstream = match branch {
            Some(b) => {
                optional(self.run(&["config", "--get", &format!("branch.{}.merge", b.trim())]))?
            }
            None => None,
        }
        .and_then(|s| s.trim().strip_prefix("refs/heads/").map(str::to_owned));
        if let (Some(remote), Some(branch)) = (&self.remote, &upstream) {
            self.run(&["fetch", "--no-tags", remote, branch])?;
        }
        let head = self.head()?;
        let ancestor = |a: &Sha, b: &Sha| {
            proc::succeeds(
                Command::new("git").current_dir(&self.root).args([
                    "merge-base",
                    "--is-ancestor",
                    a.as_str(),
                    b.as_str(),
                ]),
                self.deadline,
            )
        };
        let relation = if upstream.is_none() || head == *remote_head {
            Relation::Equal
        } else if ancestor(&head, remote_head)? {
            Relation::Behind
        } else if ancestor(remote_head, &head)? {
            Relation::Ahead
        } else {
            Relation::Diverged
        };
        let dirty = self
            .run(&["status", "--porcelain"])?
            .lines()
            .map(str::to_owned)
            .collect();
        Ok(LocalState {
            upstream_branch: upstream,
            head,
            dirty,
            relation,
        })
    }
    pub fn fast_forward(&self, sha: &Sha) -> anyhow::Result<()> {
        self.run(&["merge", "--ff-only", sha.as_str()])?;
        Ok(())
    }
    pub fn commit_all(&self) -> anyhow::Result<bool> {
        if self.is_clean()? {
            return Ok(false);
        }
        self.run(&["add", "-A"])?;
        self.run(&["commit", "-m", "baby-sitter: address review feedback"])?;
        Ok(true)
    }
    pub fn push(&self, branch: &str) -> anyhow::Result<()> {
        let remote = self.remote.as_deref().context("no upstream remote")?;
        self.run(&["push", remote, &format!("HEAD:refs/heads/{branch}")])?;
        Ok(())
    }
}
fn optional(result: anyhow::Result<String>) -> anyhow::Result<Option<String>> {
    match result {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.downcast_ref::<proc::TimedOut>().is_some() => Err(e),
        Err(_) => Ok(None),
    }
}
