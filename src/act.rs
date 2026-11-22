use crate::{codex, decide::Action, domain::*, git::Git, github::Gh, proc};
use anyhow::{Context, ensure};
use std::{
    fs::{self, File, OpenOptions},
    time::Instant,
};
pub struct Env {
    pub gh: Gh,
    pub git: Git,
    pub deadline: Instant,
    pub _lock: Option<File>,
}
impl Env {
    pub fn new(arg: Option<&str>, dry_run: bool, deadline: Instant) -> anyhow::Result<Self> {
        let gh = Gh::resolve(arg, deadline)?;
        let git = Git::discover(deadline)?;
        let lock = if dry_run {
            None
        } else {
            let dir = git.state_dir();
            fs::create_dir_all(&dir)?;
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(dir.join(format!("pr-{}.lock", gh.pr.number)))?;
            file.try_lock()
                .context("another baby-sitter holds the PR lock")?;
            Some(file)
        };
        Ok(Self {
            gh,
            git,
            deadline,
            _lock: lock,
        })
    }
}
pub fn execute(env: &Env, action: Action, pr_id: &str) -> anyhow::Result<()> {
    match action {
        Action::FastForward(sha) => env.git.fast_forward(&sha),
        Action::ResolveThreads(ids) => {
            for id in ids {
                env.gh.resolve_thread(&id)?;
            }
            Ok(())
        }
        Action::UpdateBranch => env.gh.update_branch(),
        Action::Enqueue { head } => env.gh.enqueue(pr_id, &head),
        Action::Merge { head, method } => env.gh.merge(method, &head),
        Action::HandleComments { batch, head } => {
            ensure!(
                env.git.head()? == head.sha,
                "local HEAD moved since snapshot"
            );
            ensure!(
                env.git.is_clean()?,
                "working tree became dirty since snapshot"
            );
            let resolved = codex::run(&env.git.root, &env.git.state_dir(), &batch, env.deadline)?;
            ensure!(
                env.git.head()? == head.sha,
                "codex changed HEAD; inspect the checkout"
            );
            if env.git.commit_all()? {
                env.git.push(&head.branch)?;
            }
            for r in &resolved {
                let body = reply_body(r);
                env.gh.reply_to_thread(&r.item.id, &body)?;
            }
            let mut reviewers: Vec<_> = resolved
                .iter()
                .filter(|r| r.outcome == Outcome::Fixed)
                .filter_map(|r| r.item.rereview.clone())
                .collect();
            reviewers.sort();
            reviewers.dedup();
            if !reviewers.is_empty()
                && let Err(e) = env.gh.request_reviewers(&reviewers)
            {
                if e.downcast_ref::<proc::TimedOut>().is_some() {
                    return Err(e);
                }
                eprintln!("baby-sitter: warning: re-review request failed: {e:#}");
            }
            Ok(())
        }
    }
}

fn reply_body(resolution: &codex::Resolution) -> String {
    let marker = Marker {
        item: resolution.item.id.clone(),
        outcome: resolution.outcome,
        answers: resolution.item.answers,
    }
    .render();
    format!("{}\n\n{marker}", resolution.reply)
}
