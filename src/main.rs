mod act;
mod codex;
mod decide;
mod deps;
mod domain;
mod git;
mod github;
mod proc;
mod wire;

use crate::{
    decide::{Facts, Verdict},
    domain::{Lifecycle, LocalState, MergeMethod, Snapshot},
};
use clap::Parser;
use std::{
    process::ExitCode,
    time::{Duration, Instant},
};

#[derive(Parser, Debug)]
#[command(name = "baby-sitter", about = "Drive a GitHub PR to merged")]
struct Cli {
    /// PR number or URL. Default: the PR for the current branch.
    pr: Option<String>,
    /// Print the current decision without applying it.
    #[arg(long)]
    dry_run: bool,
    /// Method to use when merging.
    #[arg(long, value_enum, default_value_t = MergeMethod::Squash)]
    merge_method: MergeMethod,
    /// Whole-run deadline, for example 90s, 10m, or 2h.
    #[arg(long, default_value = "2h", value_parser = parse_duration)]
    timeout: Duration,
    /// Time between snapshots while waiting.
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    interval: Duration,
    /// Print each merge gate's state before every decision.
    #[arg(long)]
    gates: bool,
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
    fn facts(&self, method: MergeMethod) -> Facts<'_> {
        Facts {
            snap: &self.snap,
            local: self.local.as_ref(),
            method,
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

fn main() -> ExitCode {
    let cli = Cli::parse();
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
    let env = act::Env::new(cli.pr.as_deref(), cli.dry_run, deadline)?;
    let mut consecutive_failures = 0;
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
                let cause = format!("{error:#}");
                eprintln!(
                    "[snapshot] wait: GitHub request failed ({} of {}): {}",
                    consecutive_failures,
                    MAX_OBSERVE_FAILURES,
                    cause.lines().next().unwrap_or("")
                );
                match after_failure(consecutive_failures) {
                    FailureStep::GiveUp => return Err(error),
                    FailureStep::Retry(_) => {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        std::thread::sleep(cli.interval.min(remaining));
                        continue;
                    }
                }
            }
        };
        let facts = observation.facts(cli.merge_method);
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
            Verdict::Act(action) => act::execute(&env, action, &observation.snap.pr_id)?,
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
    fn observation_failures_retry_twice_then_stop() {
        assert_eq!(after_failure(1), FailureStep::Retry(1));
        assert_eq!(after_failure(2), FailureStep::Retry(2));
        assert_eq!(after_failure(3), FailureStep::GiveUp);
        assert_eq!(after_failure(4), FailureStep::GiveUp);
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
