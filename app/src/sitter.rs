use crate::{
    gh::{self, ChildEnv},
    model::{self, Outcome, PrRef, RepoName, Resolved, SitState, unix_now},
};
use anyhow::{Context, Result, bail};
use async_channel::Sender;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{BufRead, BufReader, Read},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub type Registry = Arc<Mutex<BTreeMap<PrRef, i32>>>;
pub type RepoLocks = Arc<Mutex<BTreeMap<RepoName, Arc<Mutex<()>>>>>;
static QUITTING: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
pub struct SitHandle {
    control: Arc<Control>,
}
struct Control {
    cancelled: AtomicBool,
    wake: Condvar,
    mutex: Mutex<()>,
}
impl SitHandle {
    pub fn cancel(&self) {
        let _guard = self.control.mutex.lock().unwrap();
        self.control.cancelled.store(true, Ordering::SeqCst);
        self.control.wake.notify_all();
    }
}
#[derive(Clone)]
pub struct SitEvent {
    pub pr: PrRef,
    pub state: SitState,
}

pub fn worktree_path(data: &Path, pr: &PrRef) -> PathBuf {
    data.join("worktrees")
        .join(&pr.repo.owner)
        .join(&pr.repo.name)
        .join(format!("pr-{}", pr.number))
}
/// Records the setup command that last finished in a PR's worktree. It lives outside the
/// worktree so it never shows up as an untracked file.
fn setup_marker(data: &Path, pr: &PrRef) -> PathBuf {
    data.join("setup")
        .join(&pr.repo.owner)
        .join(&pr.repo.name)
        .join(format!("pr-{}", pr.number))
}
fn clone_path(data: &Path, repo: &RepoName) -> PathBuf {
    data.join("repos").join(&repo.owner).join(&repo.name)
}

fn repo_lock(locks: &RepoLocks, repo: &RepoName) -> Arc<Mutex<()>> {
    locks
        .lock()
        .unwrap()
        .entry(repo.clone())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

fn parse_worktree_path(root: &Path, path: &Path) -> Option<PrRef> {
    let mut parts = path.strip_prefix(root).ok()?.components();
    let owner = parts.next()?.as_os_str().to_str()?;
    let repo = parts.next()?.as_os_str().to_str()?;
    let dir = parts.next()?.as_os_str().to_str()?;
    if parts.next().is_some() {
        return None;
    }
    let number = dir.strip_prefix("pr-")?;
    let pr: PrRef = format!("{owner}/{repo}#{number}").parse().ok()?;
    (dir == format!("pr-{}", pr.number)).then_some(pr)
}

fn git_output(
    env: &ChildEnv,
    clone: &Path,
    args: &[&std::ffi::OsStr],
) -> Result<std::process::Output> {
    let mut command = env.command("git");
    command
        .arg("-C")
        .arg(clone)
        .args(args)
        .process_group(0)
        .stdin(Stdio::null());
    command.output().context("start git")
}

pub fn sweep_trash(data: &Path) {
    let trash = data.join("trash");
    thread::spawn(move || {
        if let Ok(entries) = std::fs::read_dir(trash) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    let _ = std::fs::remove_dir_all(path);
                } else {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    });
}

pub fn remove_worktree(env: &ChildEnv, data: &Path, locks: &RepoLocks, pr: &PrRef) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let mut trashed = None;
    let result = (|| {
        let lock = repo_lock(locks, &pr.repo);
        let _guard = lock
            .lock()
            .map_err(|error| anyhow::anyhow!("repository lock: {error}"))?;
        let clone = clone_path(data, &pr.repo);
        if !clone.exists() {
            return Ok(());
        }
        let worktree = worktree_path(data, pr);
        let remove = if worktree.exists() {
            let trash = data.join("trash");
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
            let destination = trash.join(format!(
                "{}-{}-pr-{}-{nanos}",
                pr.repo.owner, pr.repo.name, pr.number
            ));
            if std::fs::create_dir_all(&trash)
                .and_then(|()| std::fs::rename(&worktree, &destination))
                .is_ok()
            {
                trashed = Some(destination);
                None
            } else {
                Some(git_output(
                    env,
                    &clone,
                    &[
                        "worktree".as_ref(),
                        "remove".as_ref(),
                        "--force".as_ref(),
                        worktree.as_os_str(),
                    ],
                )?)
            }
        } else {
            None
        };
        let prune = git_output(env, &clone, &["worktree".as_ref(), "prune".as_ref()])?;
        if !prune.status.success() {
            bail!(
                "prune worktrees: {}",
                String::from_utf8_lossy(&prune.stderr).trim()
            );
        }
        let branch = format!("baby-sitter/pr-{}", pr.number);
        let reference = format!("refs/heads/{branch}");
        let exists = git_output(
            env,
            &clone,
            &[
                "show-ref".as_ref(),
                "--verify".as_ref(),
                "--quiet".as_ref(),
                reference.as_ref(),
            ],
        )?;
        if exists.status.success() {
            let deleted = git_output(
                env,
                &clone,
                &["branch".as_ref(), "-D".as_ref(), branch.as_ref()],
            )?;
            if !deleted.status.success() {
                bail!(
                    "delete branch: {}",
                    String::from_utf8_lossy(&deleted.stderr).trim()
                );
            }
        } else if exists.status.code() != Some(1) {
            bail!(
                "check branch: {}",
                String::from_utf8_lossy(&exists.stderr).trim()
            );
        }
        if let Some(remove) = remove
            && !remove.status.success()
            && worktree.exists()
        {
            let list = git_output(
                env,
                &clone,
                &[
                    "worktree".as_ref(),
                    "list".as_ref(),
                    "--porcelain".as_ref(),
                    "-z".as_ref(),
                ],
            )?;
            if !list.status.success() {
                bail!(
                    "list worktrees: {}",
                    String::from_utf8_lossy(&list.stderr).trim()
                );
            }
            if list.stdout.split(|byte| *byte == 0).any(|entry| {
                entry.strip_prefix(b"worktree ") == Some(worktree.as_os_str().as_bytes())
            }) {
                bail!(
                    "remove worktree: {}",
                    String::from_utf8_lossy(&remove.stderr).trim()
                );
            }
            std::fs::remove_dir_all(&worktree).context("remove unregistered worktree")?;
        }
        Ok(())
    })();
    if result.is_ok() {
        let _ = std::fs::remove_file(setup_marker(data, pr));
    }
    if let Some(path) = trashed {
        thread::spawn(move || {
            let _ = std::fs::remove_dir_all(path);
        });
    }
    result
}

pub fn sweep_merged(
    env: &ChildEnv,
    data: &Path,
    locks: &RepoLocks,
    keep: &BTreeSet<PrRef>,
) -> Vec<PrRef> {
    fn directories(path: &Path) -> impl Iterator<Item = PathBuf> {
        std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                entry
                    .file_type()
                    .ok()
                    .filter(|kind| kind.is_dir())
                    .map(|_| entry.path())
            })
    }
    let root = data.join("worktrees");
    let mut removed = Vec::new();
    for owner in directories(&root) {
        for repo in directories(&owner) {
            for path in directories(&repo) {
                let Some(pr) = parse_worktree_path(&root, &path) else {
                    continue;
                };
                if !keep.contains(&pr)
                    && gh::pr_state(env, &pr.url()).is_ok_and(|state| state == "MERGED")
                    && remove_worktree(env, data, locks, &pr).is_ok()
                {
                    removed.push(pr);
                }
            }
        }
    }
    removed
}
fn emit(tx: &Sender<SitEvent>, pr: &PrRef, state: SitState) {
    let _ = tx.send_blocking(SitEvent {
        pr: pr.clone(),
        state,
    });
}

pub fn terminate_all(registry: &Registry) {
    QUITTING.store(true, Ordering::SeqCst);
    if let Ok(registry) = registry.lock() {
        for pgid in registry.values().copied().filter(|pgid| *pgid > 0) {
            unsafe {
                libc::kill(-pgid, libc::SIGTERM);
            }
        }
    }
}

fn group_exists(pgid: i32) -> bool {
    if pgid <= 0 {
        return false;
    }
    let result = unsafe { libc::kill(-pgid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
fn terminate_group(pgid: i32, child: &mut Child, grace: Duration) -> Option<ExitStatus> {
    if pgid <= 0 {
        return None;
    }
    unsafe {
        libc::kill(-pgid, libc::SIGTERM);
    }
    let until = Instant::now() + grace;
    let mut status = None;
    loop {
        if status.is_none() {
            status = child.try_wait().ok().flatten();
        }
        if !group_exists(pgid) {
            break;
        }
        if Instant::now() >= until {
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
            break;
        }
        thread::sleep(
            Duration::from_millis(100).min(until.saturating_duration_since(Instant::now())),
        );
    }
    if status.is_none() {
        status = child.wait().ok();
    }
    status
}

#[derive(Clone, Copy)]
enum Interrupted {
    Cancelled,
    Deadline,
}
impl Interrupted {
    fn message(self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::Deadline => "timed out",
        }
    }

    fn outcome(self) -> Outcome {
        match self {
            Self::Cancelled => Outcome::Cancelled,
            Self::Deadline => Outcome::TimedOut,
        }
    }
}
struct ProcessOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: String,
    interrupted: Option<Interrupted>,
}
enum ReadEvent {
    Stdout(Vec<u8>),
    Stderr(String),
    StderrDone,
}
#[derive(Clone, Copy)]
enum ProcessMode {
    Capture,
    Sit,
    Quiet,
}

enum Preparation {
    Ready,
    Finished(Outcome),
}

struct Runner {
    pr: PrRef,
    data: PathBuf,
    env: ChildEnv,
    deadline: u64,
    settings: Resolved,
    registry: Registry,
    locks: RepoLocks,
    control: Arc<Control>,
    tx: Sender<SitEvent>,
}
impl Runner {
    fn interrupted(&self) -> Option<Interrupted> {
        if self.control.cancelled.load(Ordering::SeqCst) {
            Some(Interrupted::Cancelled)
        } else if unix_now() >= self.deadline {
            Some(Interrupted::Deadline)
        } else {
            None
        }
    }
    fn pause(&self, duration: Duration) {
        let lock = self.control.mutex.lock().unwrap();
        if !self.control.cancelled.load(Ordering::SeqCst) {
            let _guard = self.control.wake.wait_timeout(lock, duration).unwrap();
        }
    }
    fn run(&self, mut command: Command, mode: ProcessMode) -> Result<ProcessOutput> {
        if let Some(interrupted) = self.interrupted() {
            bail!(interrupted.message());
        }
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(if matches!(mode, ProcessMode::Capture) {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stderr(Stdio::piped());
        let mut registry = self.registry.lock().unwrap();
        if QUITTING.load(Ordering::SeqCst) {
            bail!("application is quitting");
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("start {}", command.get_program().to_string_lossy()))?;
        let pgid = child.id() as i32;
        registry.insert(self.pr.clone(), pgid);
        drop(registry);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take().unwrap();
        let (read_tx, read_rx) = mpsc::channel();
        if let Some(mut stdout) = stdout {
            let stdout_tx = read_tx.clone();
            thread::spawn(move || {
                let mut bytes = Vec::new();
                let _ = stdout.read_to_end(&mut bytes);
                let _ = stdout_tx.send(ReadEvent::Stdout(bytes));
            });
        }
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                match line {
                    Ok(line) => {
                        let _ = read_tx.send(ReadEvent::Stderr(line));
                    }
                    Err(error) => {
                        let _ = read_tx.send(ReadEvent::Stderr(error.to_string()));
                        break;
                    }
                }
            }
            let _ = read_tx.send(ReadEvent::StderrDone);
        });
        let mut stdout = if matches!(mode, ProcessMode::Capture) {
            None
        } else {
            Some(Vec::new())
        };
        let mut stderr = String::new();
        let mut stderr_done = false;
        let mut status = None;
        let mut interrupted = None;
        let mut last_running = SitState::Running {
            last_line: None,
            since: Instant::now(),
        };
        let mut drain_until = None;
        loop {
            while let Ok(event) = read_rx.try_recv() {
                match event {
                    ReadEvent::Stdout(bytes) => stdout = Some(bytes),
                    ReadEvent::Stderr(line) => {
                        stderr.push_str(&line);
                        stderr.push('\n');
                        if matches!(mode, ProcessMode::Sit) && interrupted.is_none() {
                            if model::parse_gates(&line).is_some() {
                                emit(
                                    &self.tx,
                                    &self.pr,
                                    SitState::Running {
                                        last_line: Some(line),
                                        since: Instant::now(),
                                    },
                                );
                            } else {
                                let since = match &last_running {
                                    SitState::Running {
                                        last_line: Some(previous),
                                        since,
                                    } if previous == &line => *since,
                                    _ => Instant::now(),
                                };
                                last_running = SitState::Running {
                                    last_line: Some(line),
                                    since,
                                };
                                emit(&self.tx, &self.pr, last_running.clone());
                            }
                        }
                    }
                    ReadEvent::StderrDone => stderr_done = true,
                }
            }
            if status.is_none() {
                match child.try_wait() {
                    Ok(result) => status = result,
                    Err(error) => {
                        terminate_group(pgid, &mut child, Duration::from_secs(10));
                        self.registry.lock().unwrap().remove(&self.pr);
                        return Err(error).context("wait for process");
                    }
                }
            }
            if status.is_some() && stdout.is_some() && stderr_done {
                // The CLI's own timeout kills only its direct child, so drain the group after any exit.
                if group_exists(pgid) {
                    terminate_group(pgid, &mut child, Duration::from_secs(10));
                    if self.control.cancelled.load(Ordering::SeqCst) {
                        interrupted = Some(Interrupted::Cancelled);
                    }
                }
                break;
            }
            if drain_until.is_none() {
                if let Some(reason) = self.interrupted() {
                    let exited = status.is_some();
                    let terminated = terminate_group(pgid, &mut child, Duration::from_secs(10));
                    if status.is_none() {
                        status = terminated;
                    }
                    if !exited || matches!(reason, Interrupted::Cancelled) {
                        interrupted = Some(reason);
                    }
                    drain_until = Some(Instant::now() + Duration::from_secs(1));
                }
            } else if drain_until.is_some_and(|until| Instant::now() >= until) {
                break;
            }
            if drain_until.is_some() {
                thread::sleep(Duration::from_millis(25));
            } else {
                self.pause(Duration::from_millis(100));
            }
        }
        self.registry.lock().unwrap().remove(&self.pr);
        Ok(ProcessOutput {
            status: status.context("wait for process")?,
            stdout: stdout.unwrap_or_default(),
            stderr,
            interrupted,
        })
    }
    fn checked(&self, command: Command) -> Result<Vec<u8>> {
        let output = self.run(command, ProcessMode::Capture)?;
        if let Some(reason) = output.interrupted {
            bail!(reason.message());
        }
        if !output.status.success() {
            bail!("{}", output.stderr.trim());
        }
        Ok(output.stdout)
    }
    fn repo_lock(&self) -> Arc<Mutex<()>> {
        repo_lock(&self.locks, &self.pr.repo)
    }
    fn prepare(&self) -> Result<Preparation> {
        let head: gh::PrHead =
            serde_json::from_slice(&self.checked(gh::pr_head_command(&self.env, &self.pr.url()))?)
                .context("parse PR head")?;
        if head.state == "MERGED" {
            return Ok(Preparation::Finished(Outcome::Merged));
        }
        if head.state == "CLOSED" {
            return Ok(Preparation::Finished(Outcome::Stopped(
                "PR closed without merging".into(),
            )));
        }
        if head.is_cross_repository {
            return Ok(Preparation::Finished(Outcome::Failed(
                "fork PRs are not supported".into(),
            )));
        }
        self.prepare_worktree(&head.head_ref_name)?;
        Ok(Preparation::Ready)
    }
    fn prepare_worktree(&self, head_ref_name: &str) -> Result<()> {
        let lock = self.repo_lock();
        let _guard = loop {
            match lock.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::WouldBlock) => {
                    if let Some(reason) = self.interrupted() {
                        bail!(reason.message());
                    }
                    self.pause(Duration::from_millis(100));
                }
                Err(error) => bail!("repository lock: {error}"),
            }
        };
        let clone = clone_path(&self.data, &self.pr.repo);
        let parent = clone.parent().context("repository path has no parent")?;
        std::fs::create_dir_all(parent)?;
        if !clone.exists() {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let tmp = parent.join(format!(
                ".{}-{}-{unique}.clone",
                self.pr.repo.name,
                std::process::id()
            ));
            let mut command = self.env.command("gh");
            command
                .args(["repo", "clone", &self.pr.repo.to_string()])
                .arg(&tmp)
                .args(["--", "--no-checkout"]);
            let result = self
                .checked(command)
                .and_then(|_| std::fs::rename(&tmp, &clone).map_err(Into::into));
            if result.is_err() && tmp.exists() {
                let _ = std::fs::remove_dir_all(&tmp);
            }
            result?;
        }
        let mut fetch = self.env.command("git");
        fetch.arg("-C").arg(&clone).args([
            "fetch",
            "origin",
            &format!("+refs/heads/{head_ref_name}:refs/remotes/origin/{head_ref_name}"),
        ]);
        self.checked(fetch)?;
        let mut prune = self.env.command("git");
        prune.arg("-C").arg(&clone).args(["worktree", "prune"]);
        self.checked(prune)?;
        let worktree = worktree_path(&self.data, &self.pr);
        if !worktree.exists() {
            std::fs::create_dir_all(worktree.parent().context("worktree has no parent")?)?;
            let mut add = self.env.command("git");
            add.arg("-C")
                .arg(&clone)
                .args([
                    "worktree",
                    "add",
                    "-B",
                    &format!("baby-sitter/pr-{}", self.pr.number),
                ])
                .arg(&worktree)
                .arg(format!("origin/{head_ref_name}"));
            self.checked(add)?;
        }
        Ok(())
    }
    /// Runs the repository's setup command unless this worktree already finished the same
    /// command, so a setup cut short by a quit or crash runs again on the next start.
    fn setup(&self) -> Option<Outcome> {
        let setup = self.settings.setup_command.as_deref()?;
        let marker = setup_marker(&self.data, &self.pr);
        if std::fs::read_to_string(&marker).is_ok_and(|done| done == setup) {
            return None;
        }
        emit(
            &self.tx,
            &self.pr,
            SitState::Running {
                last_line: Some(format!("Running setup: {setup}")),
                since: Instant::now(),
            },
        );
        let mut command = self.env.command("/bin/sh");
        command
            .args(["-c", setup])
            .current_dir(worktree_path(&self.data, &self.pr));
        let mut outcome = match self.run(command, ProcessMode::Quiet) {
            Ok(output) => {
                if let Some(reason) = output.interrupted {
                    reason.outcome()
                } else if output.status.success() {
                    let recorded = marker
                        .parent()
                        .map_or(Ok(()), std::fs::create_dir_all)
                        .and_then(|()| std::fs::write(&marker, setup));
                    if let Err(error) = recorded {
                        eprintln!("Could not record the finished setup command: {error}");
                    }
                    return None;
                } else {
                    let cause = output
                        .stderr
                        .lines()
                        .rev()
                        .map(str::trim)
                        .find(|line| !line.is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| match output.status.code() {
                            Some(code) => format!("exit status {code}"),
                            None => output.status.to_string(),
                        });
                    Outcome::Stopped(format!("Setup command failed: {cause}"))
                }
            }
            Err(error) => match self.interrupted() {
                Some(reason) => reason.outcome(),
                None => Outcome::Stopped(format!("Setup command failed: {error:#}")),
            },
        };
        if let Err(error) = remove_worktree(&self.env, &self.data, &self.locks, &self.pr) {
            if let Outcome::Stopped(message) = &mut outcome {
                message.push_str(&format!("; could not remove worktree: {error:#}"));
            } else {
                eprintln!("Could not remove worktree after interrupted setup: {error:#}");
            }
        }
        Some(outcome)
    }
    fn execute(&self) -> Result<Outcome> {
        if let Some(reason) = self.interrupted() {
            return Ok(reason.outcome());
        }
        if let Preparation::Finished(outcome) = self.prepare()? {
            return Ok(outcome);
        }
        if let Some(outcome) = self.setup() {
            return Ok(outcome);
        }
        let cli = std::env::current_exe()?
            .parent()
            .context("executable has no parent")?
            .join("baby-sitter");
        loop {
            if let Some(reason) = self.interrupted() {
                return Ok(reason.outcome());
            }
            emit(
                &self.tx,
                &self.pr,
                SitState::Running {
                    last_line: None,
                    since: Instant::now(),
                },
            );
            let mut command = self.env.command(&cli);
            command
                .arg(self.pr.url())
                .args([
                    "--timeout",
                    &format!("{}s", self.deadline.saturating_sub(unix_now())),
                    "--interval",
                    &format!("{}s", self.settings.interval.as_secs()),
                    "--gates",
                ])
                .current_dir(worktree_path(&self.data, &self.pr));
            command.arg("--prompts").arg(self.data.join("prompts.json"));
            let output = self.run(command, ProcessMode::Sit)?;
            if model::lock_held(output.status.code(), &output.stderr)
                && output.interrupted.is_none()
            {
                emit(&self.tx, &self.pr, SitState::Preparing);
                self.pause(self.settings.interval.min(Duration::from_secs(
                    self.deadline.saturating_sub(unix_now()),
                )));
                continue;
            }
            if matches!(output.interrupted, Some(Interrupted::Deadline)) {
                return Ok(Outcome::TimedOut);
            }
            let outcome = model::classify(
                output.status.code(),
                &output.stderr,
                self.control.cancelled.load(Ordering::SeqCst),
            );
            return Ok(outcome);
        }
    }
}

pub fn spawn(
    pr: PrRef,
    settings: Resolved,
    deadline: u64,
    data: PathBuf,
    env: ChildEnv,
    registry: Registry,
    locks: RepoLocks,
    tx: Sender<SitEvent>,
) -> SitHandle {
    let control = Arc::new(Control {
        cancelled: AtomicBool::new(false),
        wake: Condvar::new(),
        mutex: Mutex::new(()),
    });
    let runner = Runner {
        pr,
        data,
        env,
        deadline,
        settings,
        registry,
        locks,
        control: control.clone(),
        tx,
    };
    thread::spawn(move || {
        emit(&runner.tx, &runner.pr, SitState::Preparing);
        let outcome = match runner.execute() {
            Ok(outcome) => outcome,
            Err(error) => match runner.interrupted() {
                Some(reason) => reason.outcome(),
                None => Outcome::Failed(format!("{error:#}")),
            },
        };
        if QUITTING.load(Ordering::SeqCst) {
            return;
        }
        notify(&runner.pr, &outcome);
        if !QUITTING.load(Ordering::SeqCst) {
            emit(&runner.tx, &runner.pr, SitState::Finished(outcome));
        }
    });
    SitHandle { control }
}

fn notify(pr: &PrRef, outcome: &Outcome) {
    let (status, body) = match outcome {
        Outcome::Merged => ("Merged", "Pull request merged"),
        Outcome::TimedOut => ("Timed out", "Babysitting timed out"),
        Outcome::Stopped(reason) => ("Needs you", reason.lines().next().unwrap_or("")),
        Outcome::Failed(reason) => ("Needs you", reason.lines().next_back().unwrap_or("")),
        Outcome::Cancelled => return,
    };
    crate::notifications::post(&format!("{pr}: {status}"), body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::process::CommandExt};
    #[test]
    fn repeated_status_keeps_elapsed_time_until_status_changes() {
        let (tx, rx) = async_channel::unbounded();
        let runner = Runner {
            pr: "owner/repo#1".parse().unwrap(),
            data: PathBuf::new(),
            env: ChildEnv::new(),
            deadline: unix_now() + 60,
            settings: model::RepoSettings::default().resolve(&model::Defaults::default()),
            registry: Arc::new(Mutex::new(BTreeMap::new())),
            locks: Arc::new(Mutex::new(BTreeMap::new())),
            control: Arc::new(Control {
                cancelled: AtomicBool::new(false),
                wake: Condvar::new(),
                mutex: Mutex::new(()),
            }),
            tx,
        };
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "echo '[ci] wait: pending' >&2; sleep 0.2; echo '[gates] reviews=met checks=waiting branch=met merge=pending' >&2; echo '[ci] wait: pending' >&2; sleep 0.2; echo '[fix] Fixing CI' >&2",
        ]);
        runner.run(command, ProcessMode::Sit).unwrap();
        let events: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|event| event.state)
            .collect();
        let [
            SitState::Running { since: first, .. },
            SitState::Running { .. },
            SitState::Running {
                since: repeated, ..
            },
            SitState::Running { since: changed, .. },
        ] = events.as_slice()
        else {
            panic!("expected four running events, got {events:?}");
        };
        assert_eq!(first, repeated);
        assert!(changed > repeated);
    }

    #[test]
    fn parses_only_worktree_paths() {
        let root = Path::new("/data/worktrees");
        assert_eq!(
            parse_worktree_path(root, Path::new("/data/worktrees/owner/repo/pr-42")),
            Some(PrRef {
                repo: "owner/repo".parse().unwrap(),
                number: 42
            })
        );
        assert_eq!(
            parse_worktree_path(root, Path::new("/data/worktrees/owner/repo/pr-x")),
            None
        );
        assert_eq!(
            parse_worktree_path(root, Path::new("/data/worktrees/owner/repo/extra/pr-42")),
            None
        );
        assert_eq!(
            parse_worktree_path(root, Path::new("/data/worktrees/owner/pr-42")),
            None
        );
    }

    fn run_git(args: &[&str]) {
        let output = Command::new("git").args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn worktree_fixture(name: &str) -> (PathBuf, PrRef) {
        let data = crate::test_support::unique_test_dir(name);
        let source = data.join("source");
        fs::create_dir_all(&source).unwrap();
        let source_str = source.to_str().unwrap();
        run_git(&["init", source_str]);
        run_git(&[
            "-C",
            source_str,
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        run_git(&["-C", source_str, "branch", "-M", "setup-test"]);
        let pr: PrRef = "owner/repo#42".parse().unwrap();
        let clone = clone_path(&data, &pr.repo);
        fs::create_dir_all(clone.parent().unwrap()).unwrap();
        run_git(&[
            "clone",
            "--no-checkout",
            source_str,
            clone.to_str().unwrap(),
        ]);
        let tree = worktree_path(&data, &pr);
        fs::create_dir_all(tree.parent().unwrap()).unwrap();
        run_git(&[
            "-C",
            clone.to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "baby-sitter/pr-42",
            tree.to_str().unwrap(),
            "HEAD",
        ]);
        (data, pr)
    }

    #[test]
    fn removes_worktree_with_untracked_files_and_branch() {
        let (data, pr) = worktree_fixture("app-merged-worktree");
        let clone = clone_path(&data, &pr.repo);
        let tree = worktree_path(&data, &pr);
        fs::write(tree.join("untracked"), "scratch").unwrap();
        let locks = Arc::new(Mutex::new(BTreeMap::new()));
        remove_worktree(&ChildEnv::new(), &data, &locks, &pr).unwrap();
        assert!(!tree.exists());
        let list = Command::new("git")
            .args([
                "-C",
                clone.to_str().unwrap(),
                "worktree",
                "list",
                "--porcelain",
            ])
            .output()
            .unwrap();
        assert!(list.status.success());
        assert!(!String::from_utf8_lossy(&list.stdout).contains(tree.to_str().unwrap()));
        remove_worktree(&ChildEnv::new(), &data, &locks, &pr).unwrap();
        assert!(!tree.exists());
        let status = Command::new("git")
            .args([
                "-C",
                clone.to_str().unwrap(),
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/baby-sitter/pr-42",
            ])
            .status()
            .unwrap();
        assert!(!status.success());
        run_git(&[
            "-C",
            clone.to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "baby-sitter/pr-42",
            tree.to_str().unwrap(),
            "HEAD",
        ]);
        fs::remove_dir_all(&tree).unwrap();
        remove_worktree(&ChildEnv::new(), &data, &locks, &pr).unwrap();
        remove_worktree(&ChildEnv::new(), &data, &locks, &pr).unwrap();
        let status = Command::new("git")
            .args([
                "-C",
                clone.to_str().unwrap(),
                "show-ref",
                "--verify",
                "--quiet",
                "refs/heads/baby-sitter/pr-42",
            ])
            .status()
            .unwrap();
        assert!(!status.success());
        fs::remove_dir_all(&clone).unwrap();
        remove_worktree(&ChildEnv::new(), &data, &locks, &pr).unwrap();
        fs::remove_dir_all(data).unwrap();
    }

    fn setup_runner(
        data: PathBuf,
        pr: PrRef,
        setup: &str,
    ) -> (Runner, async_channel::Receiver<SitEvent>) {
        let (tx, rx) = async_channel::unbounded();
        let settings = model::RepoSettings {
            setup_command: Some(setup.to_owned()),
            ..Default::default()
        }
        .resolve(&model::Defaults::default());
        (
            Runner {
                pr,
                data,
                env: ChildEnv::new(),
                deadline: unix_now() + 60,
                settings,
                registry: Arc::new(Mutex::new(BTreeMap::new())),
                locks: Arc::new(Mutex::new(BTreeMap::new())),
                control: Arc::new(Control {
                    cancelled: AtomicBool::new(false),
                    wake: Condvar::new(),
                    mutex: Mutex::new(()),
                }),
                tx,
            },
            rx,
        )
    }

    #[test]
    fn setup_runs_until_it_finishes_once_per_command() {
        let (data, pr) = worktree_fixture("app-setup-success");
        let tree = worktree_path(&data, &pr);
        let command = "sh -c 'touch ran; echo \"[ci] wait: pending\" >&2'";
        let (mut runner, rx) = setup_runner(data.clone(), pr.clone(), command);

        runner.settings.setup_command = None;
        assert_eq!(runner.setup(), None);
        assert!(!tree.join("ran").exists(), "no command runs no setup");
        assert!(rx.try_recv().is_err());

        runner.settings.setup_command = Some(command.into());
        let lock = runner.repo_lock();
        assert!(lock.try_lock().is_ok());
        assert_eq!(runner.setup(), None);
        assert!(
            tree.join("ran").exists(),
            "a worktree without a finished setup runs it"
        );
        assert!(matches!(
            rx.try_recv().unwrap().state,
            SitState::Running { last_line: Some(line), .. }
                if line == format!("Running setup: {command}")
        ));

        fs::remove_file(tree.join("ran")).unwrap();
        assert_eq!(runner.setup(), None);
        assert!(
            !tree.join("ran").exists(),
            "a finished setup does not run again"
        );

        runner.settings.setup_command = Some("sh -c 'touch changed'".into());
        assert_eq!(runner.setup(), None);
        assert!(tree.join("changed").exists(), "a changed command runs");

        remove_worktree(&runner.env, &data, &runner.locks, &pr).unwrap();
        runner.prepare_worktree("setup-test").unwrap();
        assert_eq!(runner.setup(), None);
        assert!(
            tree.join("changed").exists(),
            "a recreated worktree runs the setup again"
        );
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn failed_setup_reports_last_stderr_and_allows_retry() {
        let (data, pr) = worktree_fixture("app-setup-failure");
        let tree = worktree_path(&data, &pr);
        let (mut runner, _) = setup_runner(
            data.clone(),
            pr,
            "sh -c 'echo earlier >&2; echo boom >&2; echo >&2; exit 3'",
        );

        assert_eq!(
            runner.setup(),
            Some(Outcome::Stopped("Setup command failed: boom".into()))
        );
        assert!(!tree.exists());
        runner.prepare_worktree("setup-test").unwrap();
        runner.settings.setup_command = Some("sh -c 'touch ran'".into());
        assert_eq!(runner.setup(), None);
        assert!(tree.join("ran").exists());
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn failed_setup_without_stderr_reports_exit_status() {
        let (data, pr) = worktree_fixture("app-setup-exit-status");
        let tree = worktree_path(&data, &pr);
        let (runner, _) = setup_runner(data.clone(), pr, "sh -c 'exit 3'");
        assert_eq!(
            runner.setup(),
            Some(Outcome::Stopped(
                "Setup command failed: exit status 3".into()
            ))
        );
        assert!(!tree.exists());
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn setup_spawn_error_removes_worktree_registration() {
        let (data, pr) = worktree_fixture("app-setup-spawn-error");
        let tree = worktree_path(&data, &pr);
        let (runner, _) = setup_runner(data.clone(), pr, "touch ran");
        fs::remove_dir_all(&tree).unwrap();
        assert!(matches!(
            runner.setup(),
            Some(Outcome::Stopped(message))
                if message.starts_with("Setup command failed: start /bin/sh")
        ));
        runner.prepare_worktree("setup-test").unwrap();
        assert!(tree.exists());
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn setup_deadline_removes_worktree_and_returns_timeout() {
        let (data, pr) = worktree_fixture("app-setup-deadline");
        let tree = worktree_path(&data, &pr);
        let (mut runner, _) = setup_runner(data.clone(), pr, "touch ran");
        runner.deadline = unix_now();
        assert_eq!(runner.setup(), Some(Outcome::TimedOut));
        assert!(!tree.exists());
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn setup_deadline_interrupts_a_running_process() {
        let (data, pr) = worktree_fixture("app-setup-running-deadline");
        let tree = worktree_path(&data, &pr);
        let (mut runner, _) = setup_runner(data.clone(), pr, "touch started; sleep 30");
        runner.deadline = unix_now() + 3;
        let worker = thread::spawn(move || runner.setup());
        let until = Instant::now() + Duration::from_secs(2);
        while !tree.join("started").exists() && Instant::now() < until {
            thread::sleep(Duration::from_millis(20));
        }
        let started = tree.join("started").exists();
        let outcome = worker.join().unwrap();
        assert!(started, "setup did not start");
        assert_eq!(outcome, Some(Outcome::TimedOut));
        assert!(!tree.exists());
        fs::remove_dir_all(data).unwrap();
    }

    #[test]
    fn cancelling_setup_removes_worktree_and_returns_cancelled() {
        let (data, pr) = worktree_fixture("app-setup-cancel");
        let tree = worktree_path(&data, &pr);
        let (runner, _) = setup_runner(data.clone(), pr, "touch started; sleep 30");
        let handle = SitHandle {
            control: runner.control.clone(),
        };
        let worker = thread::spawn(move || runner.setup());
        let until = Instant::now() + Duration::from_secs(5);
        while !tree.join("started").exists() && Instant::now() < until {
            thread::sleep(Duration::from_millis(20));
        }
        let started = tree.join("started").exists();
        handle.cancel();
        let outcome = worker.join().unwrap();
        assert!(started, "setup did not start");
        assert_eq!(outcome, Some(Outcome::Cancelled));
        assert!(!tree.exists());
        fs::remove_dir_all(data).unwrap();
    }

    fn fixture(mode: &str) -> (Child, PathBuf) {
        let dir = crate::test_support::unique_test_dir(&format!("app-cancel-{mode}"));
        fs::create_dir_all(&dir).unwrap();
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cancel.sh");
        let mut child = Command::new("/bin/sh")
            .arg(path)
            .arg(mode)
            .arg(&dir)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !dir.join("ready").exists() && Instant::now() < until {
            thread::sleep(Duration::from_millis(20));
        }
        if !dir.join("ready").exists() {
            let _ = child.kill();
            panic!("fixture did not start");
        }
        (child, dir)
    }
    #[test]
    fn cancel_waits_for_group_after_leader_exits() {
        let dir = crate::test_support::unique_test_dir("app-cancel-runner");
        fs::create_dir_all(&dir).unwrap();
        let pr = PrRef {
            repo: "owner/repo".parse().unwrap(),
            number: 1,
        };
        let registry: Registry = Arc::new(Mutex::new(BTreeMap::new()));
        let control = Arc::new(Control {
            cancelled: AtomicBool::new(false),
            wake: Condvar::new(),
            mutex: Mutex::new(()),
        });
        let handle = SitHandle {
            control: control.clone(),
        };
        let (tx, _rx) = async_channel::unbounded();
        let runner = Runner {
            pr: pr.clone(),
            data: dir.clone(),
            env: ChildEnv::new(),
            deadline: unix_now() + 60,
            settings: model::RepoSettings::default().resolve(&model::Defaults::default()),
            registry: registry.clone(),
            locks: Arc::new(Mutex::new(BTreeMap::new())),
            control,
            tx,
        };
        let mut command = Command::new("/bin/sh");
        command
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/cancel.sh"))
            .arg("graceful")
            .arg(&dir);
        let worker = thread::spawn(move || runner.run(command, ProcessMode::Quiet).unwrap());
        let until = Instant::now() + Duration::from_secs(5);
        while !dir.join("ready").exists() && Instant::now() < until {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(dir.join("ready").exists(), "fixture did not start");
        let pgid = registry.lock().unwrap()[&pr];
        handle.cancel();
        let output = worker.join().unwrap();
        assert_eq!(
            model::classify(
                output.status.code(),
                &output.stderr,
                matches!(output.interrupted, Some(Interrupted::Cancelled))
            ),
            Outcome::Cancelled
        );
        assert!(
            dir.join("finished").exists(),
            "member must complete its SIGTERM cleanup"
        );
        assert!(!group_exists(pgid), "process group must be empty");
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn cancel_kills_members_that_ignore_sigterm() {
        let (mut child, dir) = fixture("stubborn");
        let pgid = child.id() as i32;
        terminate_group(pgid, &mut child, Duration::from_millis(500));
        let until = Instant::now() + Duration::from_secs(3);
        while group_exists(pgid) && Instant::now() < until {
            thread::sleep(Duration::from_millis(25));
        }
        assert!(!group_exists(pgid), "SIGKILL must empty the process group");
        fs::remove_dir_all(dir).unwrap();
    }
}
