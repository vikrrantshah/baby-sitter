mod act;
mod agent;
mod decide;
mod domain;
mod git;
mod github;
mod proc;
mod wire;

use crate::{
    decide::{Facts, Verdict},
    domain::{Lifecycle, LocalState, MergeMethod, Snapshot, StackMode},
};
use clap::Parser;
use std::{
    path::PathBuf,
    process::ExitCode,
    time::{Duration, Instant},
};

#[derive(Parser, Debug)]
#[command(name = "baby-sitter", about = "Help merge a GitHub PR")]
struct Cli {
    /// PR number or URL. Uses the PR for the current branch by default.
    pr: Option<String>,
    /// Print the current decision without applying it.
    #[arg(long)]
    dry_run: bool,
    /// Method to use to merge the PR.
    #[arg(long, value_enum, default_value_t = MergeMethod::Squash)]
    merge_method: MergeMethod,
    /// Order in which stacked PRs merge.
    #[arg(long, value_enum, default_value_t = StackMode::BottomUp)]
    stack: StackMode,
    /// Deadline for the entire run, for example 90s, 10m, or 2h.
    #[arg(long, default_value = "2h", value_parser = parse_duration)]
    timeout: Duration,
    /// Time between snapshots while waiting.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    interval: Duration,
    /// Print each merge gate's state before every decision.
    #[arg(long)]
    gates: bool,
    /// Read instruction overrides from a JSON file.
    #[arg(long, value_name = "FILE")]
    prompts: Option<PathBuf>,
    /// Coding agent to run.
    #[arg(long, value_enum, default_value = "codex")]
    agent: agent::AgentKind,
    /// Model to use instead of the agent's default.
    #[arg(long)]
    model: Option<String>,
    /// Print the default instruction texts as JSON and exit.
    #[arg(long)]
    print_default_prompts: bool,
}
struct Observation {
    snap: Snapshot,
    local: Option<LocalState>,
}
impl Observation {
    fn take(env: &act::Env) -> anyhow::Result<Self> {
        let snap = env.gh.snapshot()?;
        let local = if snap.lifecycle == Lifecycle::Open {
            Some(env.git.observe(&snap.head.sha)?)
        } else {
            None
        };
        Ok(Self { snap, local })
    }
    fn facts(&self, method: MergeMethod, stack: StackMode) -> Facts<'_> {
        Facts {
            snap: &self.snap,
            local: self.local.as_ref(),
            method,
            stack,
        }
    }
}
const MAX_OBSERVE_FAILURES: u32 = 3;

#[derive(Debug, PartialEq, Eq)]
enum FailureStep {
    Retry(u32),
    GiveUp,
}

fn after_failure(consecutive_failures: u32) -> FailureStep {
    if consecutive_failures >= MAX_OBSERVE_FAILURES {
        FailureStep::GiveUp
    } else {
        FailureStep::Retry(consecutive_failures)
    }
}

/// Errors that a later pass outlives: GitHub's own server failures, and a merge that
/// an earlier failed request already started. A later pass decides the action again from
/// a fresh snapshot, so retrying cannot repeat a step that already landed.
fn github_transient_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}");
    message.contains("Something went wrong while executing your query")
        || message.contains("Merge already in progress")
        || ["HTTP 50", "status code: 50"]
            .iter()
            .any(|status| message.contains(status))
}

/// Reports a failed GitHub request as a wait. GitHub's own server errors are retried
/// until the deadline, because an outage can outlast any fixed number of attempts.
fn failed_request(label: &str, failures: u32, error: &anyhow::Error) -> FailureStep {
    let cause = format!("{error:#}");
    let cause = cause.lines().next().unwrap_or("");
    if github_transient_error(error) {
        eprintln!("[{label}] wait: GitHub is failing, retrying (attempt {failures}): {cause}");
        FailureStep::Retry(failures)
    } else {
        eprintln!(
            "[{label}] wait: GitHub request failed ({failures} of {MAX_OBSERVE_FAILURES}): {cause}"
        );
        after_failure(failures)
    }
}

fn main() -> ExitCode {
    run_cli(Cli::parse())
}

fn run_cli(cli: Cli) -> ExitCode {
    if cli.print_default_prompts {
        println!("{}", agent::default_prompts_json());
        return ExitCode::SUCCESS;
    }
    let deadline = match Instant::now().checked_add(cli.timeout) {
        Some(d) => d,
        None => {
            eprintln!("baby-sitter: timeout is too large");
            return ExitCode::from(1);
        }
    };
    match run(&cli, deadline) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.downcast_ref::<proc::TimedOut>().is_some() => {
            eprintln!("baby-sitter: timed out");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("baby-sitter: stopped: {e:#}");
            ExitCode::from(1)
        }
    }
}
fn run(cli: &Cli, deadline: Instant) -> anyhow::Result<()> {
    if let Some(path) = cli.prompts.as_deref() {
        agent::load(path)?;
    }
    let env = act::Env::new(
        cli.pr.as_deref(),
        cli.dry_run,
        cli.prompts.clone(),
        agent::AgentConfig {
            kind: cli.agent,
            model: cli.model.clone(),
        },
        deadline,
    )?;
    let mut consecutive_failures = 0;
    let mut action_failures = 0;
    loop {
        if Instant::now() >= deadline {
            return Err(proc::TimedOut.into());
        }
        let observation = match Observation::take(&env) {
            Ok(observation) => {
                consecutive_failures = 0;
                observation
            }
            Err(error) => {
                if error.downcast_ref::<proc::TimedOut>().is_some() {
                    return Err(error);
                }
                consecutive_failures += 1;
                if failed_request("snapshot", consecutive_failures, &error) == FailureStep::GiveUp {
                    return Err(error);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(cli.interval.min(remaining));
                continue;
            }
        };
        let facts = observation.facts(cli.merge_method, cli.stack);
        let decision = decide::decide(&facts);
        if cli.dry_run {
            print!("{}", decide::explain(&facts));
            if let Verdict::Stop(reason) = decision.verdict {
                anyhow::bail!("{reason}");
            }
            return Ok(());
        }
        if cli.gates {
            eprintln!("[gates] {}", decide::gates(&facts));
        }
        if let (
            Verdict::Act(decide::Action::Enqueue { .. }),
            domain::MergeRoute::Queue { method },
        ) = (&decision.verdict, observation.snap.route)
        {
            eprintln!(
                "[{}] {} (queue merges with {method})",
                decision.rule, decision.verdict
            );
        } else {
            eprintln!("[{}] {}", decision.rule, decision.verdict);
        }
        match decision.verdict {
            Verdict::Done => return Ok(()),
            Verdict::Stop(reason) => anyhow::bail!("{reason}"),
            Verdict::Wait(_) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(cli.interval.min(remaining));
            }
            Verdict::Act(action) => match act::execute(&env, action, &observation.snap.pr_id) {
                Ok(()) => action_failures = 0,
                Err(error) if github_transient_error(&error) => {
                    action_failures += 1;
                    failed_request(decision.rule, action_failures, &error);
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(cli.interval.min(remaining));
                }
                Err(error) => return Err(error),
            },
        }
    }
}
fn parse_duration(s: &str) -> Result<Duration, String> {
    let split = s
        .len()
        .checked_sub(1)
        .ok_or_else(|| "empty duration".to_owned())?;
    if !s.is_char_boundary(split) {
        return Err(format!("invalid duration {s:?}"));
    }
    let (n, unit) = s.split_at(split);
    let n: u64 = n.parse().map_err(|_| format!("invalid duration {s:?}"))?;
    let factor = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(format!("invalid duration {s:?}; use s, m, or h")),
    };
    n.checked_mul(factor)
        .map(Duration::from_secs)
        .ok_or_else(|| "duration overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_and_model_options_are_visible_and_parse() {
        use clap::CommandFactory;
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--agent <AGENT>"));
        assert!(help.contains("--model <MODEL>"));
        for (name, kind) in [
            ("codex", agent::AgentKind::Codex),
            ("claude", agent::AgentKind::Claude),
            ("pi", agent::AgentKind::Pi),
            ("omp", agent::AgentKind::Omp),
        ] {
            assert!(help.contains(name));
            let cli = Cli::try_parse_from(["baby-sitter", "--agent", name, "--model", "example"])
                .unwrap();
            assert_eq!(cli.agent, kind);
            assert_eq!(cli.model.as_deref(), Some("example"));
        }
        let cli = Cli::try_parse_from(["baby-sitter"]).unwrap();
        assert_eq!(cli.agent, agent::AgentKind::Codex);
        assert!(cli.model.is_none());
    }

    #[test]
    fn stack_option_defaults_to_bottom_up_and_accepts_top_down() {
        assert_eq!(
            Cli::try_parse_from(["baby-sitter"]).unwrap().stack,
            StackMode::BottomUp
        );
        assert_eq!(
            Cli::try_parse_from(["baby-sitter", "--stack", "top-down"])
                .unwrap()
                .stack,
            StackMode::TopDown
        );
        assert!(Cli::try_parse_from(["baby-sitter", "--stack", "sideways"]).is_err());
        use clap::CommandFactory;
        let help = Cli::command().render_long_help().to_string();
        assert!(help.contains("--stack <STACK>"));
        assert!(help.contains("bottom-up") && help.contains("top-down"));
    }

    #[test]
    fn cli_parses_prompt_options() {
        let cli = Cli::try_parse_from(["baby-sitter", "--prompts", "prompts.json", "42"]).unwrap();
        assert_eq!(
            cli.prompts.as_deref(),
            Some(std::path::Path::new("prompts.json"))
        );
        assert_eq!(cli.pr.as_deref(), Some("42"));
        assert!(!cli.print_default_prompts);
        assert!(Cli::try_parse_from(["baby-sitter", "--prompts"]).is_err());

        let cli = Cli::try_parse_from(["baby-sitter", "--print-default-prompts"]).unwrap();
        assert!(cli.print_default_prompts);
        assert_eq!(run_cli(cli), ExitCode::SUCCESS);
    }

    #[test]
    fn invalid_prompt_file_fails_before_discovering_a_checkout() {
        let root = test_support::unique_test_dir("baby-sitter-cli-prompts");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("prompts.json");
        std::fs::write(&path, r#"{"ci":null}"#).unwrap();
        let cli = Cli::try_parse_from([
            std::ffi::OsString::from("baby-sitter"),
            "--prompts".into(),
            path.as_os_str().to_owned(),
        ])
        .unwrap();
        let error = run(&cli, Instant::now() + Duration::from_secs(10))
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("invalid type: null"), "{error}");
        assert!(error.contains(path.to_string_lossy().as_ref()));
        assert_eq!(run_cli(cli), ExitCode::from(1));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn observation_failures_retry_twice_then_stop() {
        assert_eq!(after_failure(1), FailureStep::Retry(1));
        assert_eq!(after_failure(2), FailureStep::Retry(2));
        assert_eq!(after_failure(3), FailureStep::GiveUp);
        assert_eq!(after_failure(4), FailureStep::GiveUp);
    }

    #[test]
    fn github_server_errors_retry_until_the_deadline_and_others_give_up() {
        for message in [
            "GraphQL: Something went wrong while executing your query on 2026-11-25T16:07:51Z. Please include `E826:77AA:ACB409F:FED0FB1:6B0707D6` when reporting this issue. (gh pr update-branch)",
            "HTTP 502: Bad Gateway (https://api.github.com/graphql) (gh api graphql)",
            "HTTP 503: Service Unavailable (gh pr merge)",
            "non-200 OK status code: 502 Bad Gateway body: \"<html>502 Bad Gateway</html>\" (gh pr merge)",
            "GraphQL: Merge already in progress (mergePullRequest) (gh pr merge)",
        ] {
            let error = anyhow::anyhow!(message.to_owned());
            assert!(github_transient_error(&error), "{message}");
            assert_eq!(
                failed_request("merge", 10, &error),
                FailureStep::Retry(10),
                "{message}"
            );
        }
        for message in [
            "GraphQL: Pull request is not mergeable (gh pr merge)",
            "HTTP 422: Validation Failed (gh pr edit)",
            "another baby-sitter holds the PR lock",
        ] {
            let error = anyhow::anyhow!(message.to_owned());
            assert!(!github_transient_error(&error), "{message}");
            assert_eq!(
                failed_request("snapshot", 3, &error),
                FailureStep::GiveUp,
                "{message}"
            );
        }
    }
}
#[cfg(test)]
pub(crate) mod test_support {
    use std::{
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    /// Temporary directory path no other test uses, even within the same nanosecond.
    pub(crate) fn unique_test_dir(prefix: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
