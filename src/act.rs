use crate::{codex, decide::Action, domain::*, git::Git, github::Gh, proc};
use anyhow::{Context, bail, ensure};
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
        Action::PushLocal { base, head } => {
            ensure!(
                env.git.head()? == head.sha,
                "local HEAD moved since snapshot"
            );
            let protected = env.git.committed_ci_changes(&base)?;
            ensure!(
                protected.is_empty(),
                "local commits touch CI configuration: {}; not pushed",
                protected.join(", ")
            );
            env.git.push(&head.branch)
        }
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
            if env.git.commit_all_with(crate::git::REVIEW_COMMIT_MESSAGE)? {
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
        Action::RerunCi { runs, .. } => {
            for run_id in runs {
                if env.gh.run_attempt(run_id)? >= 3 {
                    bail!(
                        "CI run {run_id} was cancelled on 3 attempts; find out why it keeps getting cancelled"
                    );
                }
                env.gh.rerun_failed(run_id)?;
            }
            Ok(())
        }
        Action::FixCi { checks, head } => {
            ensure!(
                env.git.head()? == head.sha,
                "local HEAD moved since snapshot"
            );
            ensure!(
                env.git.is_clean()?,
                "working tree became dirty since snapshot"
            );
            let names = check_names(&checks);
            let mut checks_with_logs = Vec::with_capacity(checks.len());
            for check in checks {
                let log = match &check.source {
                    CheckSource::Actions { job_id } => {
                        optional_job_log(env.gh.failed_job_log(*job_id))
                            .with_context(|| format!("fetching log for {}", check.name))?
                    }
                    CheckSource::External { .. } => None,
                };
                checks_with_logs.push((check, log));
            }
            let result = codex::fix_ci(
                &env.git.root,
                &env.git.state_dir(),
                &checks_with_logs,
                env.deadline,
            );
            ensure!(
                env.git.head()? == head.sha,
                "codex changed HEAD; inspect the checkout"
            );
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    ensure_ci_configuration_unchanged(&env.git)?;
                    return Err(error);
                }
            };
            if result.outcome == codex::CiFixOutcome::NeedsHuman && env.git.is_clean()? {
                bail!("CI needs a person: {names}: {}", result.summary.trim());
            }
            commit_ci_fix(env, &head, &result.summary)?;
            if result.outcome == codex::CiFixOutcome::NeedsHuman {
                eprintln!(
                    "baby-sitter: pushed a partial CI fix for {names}: {}",
                    result.summary.trim()
                );
            }
            Ok(())
        }
    }
}
fn ensure_ci_configuration_unchanged(git: &Git) -> anyhow::Result<()> {
    let protected = git.ci_configuration_changes()?;
    ensure!(
        protected.is_empty(),
        "codex edited CI configuration: {}; changes left uncommitted for inspection",
        protected.join(", ")
    );
    Ok(())
}

fn commit_ci_fix(env: &Env, head: &Head, summary: &str) -> anyhow::Result<()> {
    ensure_ci_configuration_unchanged(&env.git)?;
    ensure!(
        env.git.commit_all_with(&ci_commit_message(summary))?,
        "codex reported a fix but changed nothing"
    );
    let protected = env.git.committed_ci_changes(&head.sha)?;
    ensure!(
        protected.is_empty(),
        "fix commit touches CI configuration: {}; commit left local, not pushed",
        protected.join(", ")
    );
    env.git.push(&head.branch)
}

fn optional_job_log(result: anyhow::Result<String>) -> anyhow::Result<Option<String>> {
    match result {
        Ok(log) => Ok(Some(log)),
        Err(error) if error.downcast_ref::<proc::TimedOut>().is_some() => Err(error),
        Err(_) => Ok(None),
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

fn ci_commit_message(summary: &str) -> String {
    const MAX_SUBJECT: usize = 67;
    let summary = summary.trim();
    let (first_line, rest) = summary.split_once('\n').unwrap_or((summary, ""));
    let first_line = first_line.trim();
    let subject = if first_line.chars().count() <= MAX_SUBJECT {
        first_line.to_owned()
    } else {
        let cut: String = first_line.chars().take(MAX_SUBJECT).collect();
        cut.rsplit_once(' ')
            .map_or(cut.clone(), |(head, _)| head.to_owned())
    };
    let rest = rest.trim();
    let body = if rest.is_empty() && subject == first_line {
        String::new()
    } else {
        format!("{}\n\n", if rest.is_empty() { first_line } else { rest })
    };
    format!("fix: {}\n\n{body}Baby-Sitter: fix-ci", subject.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_job_logs_do_not_hide_timeouts() {
        assert_eq!(
            optional_job_log(Ok("build failed".into())).unwrap(),
            Some("build failed".into())
        );
        assert_eq!(
            optional_job_log(Err(anyhow::anyhow!("log not found"))).unwrap(),
            None
        );
        assert!(
            optional_job_log(Err(proc::TimedOut.into()))
                .unwrap_err()
                .downcast_ref::<proc::TimedOut>()
                .is_some()
        );
    }

    #[test]
    fn ci_fix_commit_subject_breaks_at_a_word_and_keeps_detail() {
        let message = ci_commit_message(
            "Fixed an off-by-one error in calc.py so mean([1, 2, 3, 4]) now divides by four",
        );
        let subject = message.lines().next().unwrap();
        assert_eq!(
            subject,
            "fix: Fixed an off-by-one error in calc.py so mean([1, 2, 3, 4]) now"
        );
        assert!(message.contains(
            "\n\nFixed an off-by-one error in calc.py so mean([1, 2, 3, 4]) now divides by four\n\n"
        ));
        assert!(message.ends_with("\n\nBaby-Sitter: fix-ci"));
        assert_eq!(
            ci_commit_message(" correct the divisor in mean \nLogs showed 2.0 != 2.5."),
            "fix: correct the divisor in mean\n\nLogs showed 2.0 != 2.5.\n\nBaby-Sitter: fix-ci"
        );
        assert_eq!(
            ci_commit_message("repair tests"),
            "fix: repair tests\n\nBaby-Sitter: fix-ci"
        );
    }
}
