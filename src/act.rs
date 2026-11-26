use crate::{
    agent,
    decide::Action,
    domain::*,
    git::{Git, MergeStart, is_ci_configuration},
    github::Gh,
    proc,
};
use anyhow::{Context, bail, ensure};
use std::{
    fs::{self, File, OpenOptions},
    path::PathBuf,
    time::Instant,
};
pub struct Env {
    pub gh: Gh,
    pub git: Git,
    pub prompts_path: Option<PathBuf>,
    pub agent: agent::AgentConfig,
    pub deadline: Instant,
    pub _lock: Option<File>,
}
impl Env {
    pub fn new(
        arg: Option<&str>,
        dry_run: bool,
        prompts_path: Option<PathBuf>,
        agent: agent::AgentConfig,
        deadline: Instant,
    ) -> anyhow::Result<Self> {
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
            prompts_path,
            agent,
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
        Action::RetargetBase { to, .. } => env.gh.retarget_base(&to),
        Action::UpdateBranch => env.gh.update_branch(),
        Action::Enqueue { head } => env.gh.enqueue(pr_id, &head),
        Action::Merge { head, method } => env.gh.merge(method, &head),
        Action::PushLocal { base, head } => {
            ensure!(
                env.git.head()? == head.sha,
                "local HEAD moved since snapshot"
            );
            let protected = env.git.protected_push_changes(&base)?;
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
            let resolved = agent::run(
                &env.git.root,
                &env.git.state_dir(),
                &batch,
                env.prompts_path.as_deref(),
                &env.agent,
                env.deadline,
            )?;
            ensure!(
                env.git.head()? == head.sha,
                "the coding agent changed HEAD; inspect the checkout"
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
        Action::ResolveConflicts { head, base } => {
            resolve_conflicts(&env.git, &head, &base, |files| {
                agent::resolve_conflicts(
                    &env.git.root,
                    &env.git.state_dir(),
                    &base,
                    files,
                    env.prompts_path.as_deref(),
                    &env.agent,
                    env.deadline,
                )
            })
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
            let result = agent::fix_ci(
                &env.git.root,
                &env.git.state_dir(),
                &checks_with_logs,
                env.prompts_path.as_deref(),
                &env.agent,
                env.deadline,
            );
            ensure!(
                env.git.head()? == head.sha,
                "the coding agent changed HEAD; inspect the checkout"
            );
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    ensure_ci_configuration_unchanged(&env.git)?;
                    return Err(error);
                }
            };
            if result.outcome == agent::CiFixOutcome::NeedsHuman && env.git.is_clean()? {
                bail!("CI needs a person: {names}: {}", result.summary.trim());
            }
            commit_ci_fix(env, &head, &result.summary)?;
            if result.outcome == agent::CiFixOutcome::NeedsHuman {
                eprintln!(
                    "baby-sitter: pushed a partial CI fix for {names}: {}",
                    result.summary.trim()
                );
            }
            Ok(())
        }
    }
}
pub(crate) fn resolve_conflicts(
    git: &Git,
    head: &Head,
    base: &str,
    resolve: impl FnOnce(&[String]) -> anyhow::Result<agent::ConflictFix>,
) -> anyhow::Result<()> {
    ensure!(git.head()? == head.sha, "local HEAD moved since snapshot");
    ensure!(
        !git.merge_in_progress(),
        "checkout already has a merge in progress"
    );
    ensure!(git.is_clean()?, "working tree became dirty since snapshot");
    git.fetch("origin", base)?;

    let result = (|| {
        let summary = match git.merge_no_commit(base)? {
            MergeStart::Clean => {
                ensure!(
                    git.merge_in_progress(),
                    "GitHub reports a conflict with {base}, but the branch already contains origin/{base}"
                );
                format!("Merge {base} into the PR branch")
            }
            MergeStart::Conflicted(files) => {
                let protected: Vec<_> = files
                    .iter()
                    .filter(|file| is_ci_configuration(file))
                    .collect();
                ensure!(
                    protected.is_empty(),
                    "Merge conflict in CI configuration: {}; resolve it by hand",
                    crate::decide::names(protected.into_iter())
                );
                let index = git.index_state()?;
                let fix = resolve(&files).with_context(|| {
                    format!("Merge conflicts need a person: {}", files.join(", "))
                })?;
                ensure!(
                    fix.outcome != agent::ConflictOutcome::NeedsHuman,
                    "Merge conflicts need a person: {}: {}",
                    files.join(", "),
                    fix.summary.trim()
                );
                ensure!(
                    git.head()? == head.sha,
                    "the coding agent changed HEAD; inspect the checkout"
                );
                ensure!(
                    git.index_state()? == index,
                    "the coding agent changed the Git index; inspect the checkout"
                );
                let outside: Vec<_> = git
                    .unstaged_paths()?
                    .into_iter()
                    .chain(git.untracked_paths()?)
                    .filter(|path| !files.contains(path))
                    .collect();
                ensure!(
                    outside.is_empty(),
                    "the coding agent edited files outside the conflicts: {}",
                    outside.join(", ")
                );
                ensure!(
                    !git.has_conflict_markers(&files)?,
                    "conflict markers remain in: {}",
                    files.join(", ")
                );
                fix.summary
            }
        };
        git.commit_merge(&conflict_commit_message(&summary))?;
        let protected = git.protected_push_changes(&head.sha)?;
        ensure!(
            protected.is_empty(),
            "merge commit touches CI configuration beyond the automatic merge: {}; commit left local, not pushed",
            protected.join(", ")
        );
        Ok(())
    })();
    if let Err(error) = result {
        return match git.merge_abort() {
            Ok(()) => Err(error),
            Err(abort_error) => {
                Err(error.context(format!("could not abort the merge: {abort_error:#}")))
            }
        };
    }
    git.push(&head.branch)
}

fn conflict_commit_message(summary: &str) -> String {
    let (subject, rest) = summary
        .trim()
        .split_once('\n')
        .unwrap_or((summary.trim(), ""));
    let rest = rest.trim();
    if rest.is_empty() {
        format!("{}\n\nBaby-Sitter: resolve-conflicts", subject.trim())
    } else {
        format!(
            "{}\n\n{rest}\n\nBaby-Sitter: resolve-conflicts",
            subject.trim()
        )
    }
}

fn ensure_ci_configuration_unchanged(git: &Git) -> anyhow::Result<()> {
    let protected = git.ci_configuration_changes()?;
    ensure!(
        protected.is_empty(),
        "the coding agent edited CI configuration: {}; changes left uncommitted for inspection",
        protected.join(", ")
    );
    Ok(())
}

fn commit_ci_fix(env: &Env, head: &Head, summary: &str) -> anyhow::Result<()> {
    ensure_ci_configuration_unchanged(&env.git)?;
    ensure!(
        env.git.commit_all_with(&ci_commit_message(summary))?,
        "the coding agent reported a fix but changed nothing"
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

fn reply_body(resolution: &agent::Resolution) -> String {
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
