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
                match &r.item.id {
                    ItemId::Thread(id) => env.gh.reply_to_thread(id, &body)?,
                    _ => env.gh.comment(&body)?,
                }
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
    match (&resolution.item.id, &resolution.item.actor) {
        (ItemId::Comment(_) | ItemId::Review(_), Actor::Human(login) | Actor::Bot(login)) => {
            format!(
                "In reply to @{}: {}\n\n{}\n\n{marker}",
                login.0, resolution.item.url, resolution.reply
            )
        }
        _ => format!("{}\n\n{marker}", resolution.reply),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decide::Item;

    #[test]
    fn top_level_replies_link_and_mention_the_author() {
        for id in [
            ItemId::Comment(CommentId(123)),
            ItemId::Review(ReviewId(456)),
        ] {
            let resolution = codex::Resolution {
                item: Item {
                    id,
                    answers: 123,
                    rereview: None,
                    actor: Actor::Human(Login("alice".into())),
                    url: "https://github.com/o/r/pull/1#issuecomment-123".into(),
                    path: None,
                    line: None,
                    context: String::new(),
                },
                outcome: Outcome::Fixed,
                reply: "Fixed.".into(),
            };
            let body = reply_body(&resolution);
            assert!(body.starts_with(
                "In reply to @alice: https://github.com/o/r/pull/1#issuecomment-123\n\nFixed.\n\n"
            ));
            assert!(
                body.ends_with(
                    &Marker {
                        item: resolution.item.id.clone(),
                        outcome: Outcome::Fixed,
                        answers: 123,
                    }
                    .render()
                )
            );
        }
    }
}
