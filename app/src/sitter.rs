use crate::{
    gh::{self, ChildEnv},
    model::{self, Outcome, PrRef, RepoName, RepoSettings, SitState, unix_now},
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

pub fn remove_merged_worktree(
    env: &ChildEnv,
    data: &Path,
    locks: &RepoLocks,
    pr: &PrRef,
) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let lock = repo_lock(locks, &pr.repo);
    let _guard = lock
        .lock()
        .map_err(|error| anyhow::anyhow!("repository lock: {error}"))?;
    let clone = clone_path(data, &pr.repo);
    let worktree = worktree_path(data, pr);
    let remove = git_output(
        env,
        &clone,
        &[
            "worktree".as_ref(),
            "remove".as_ref(),
            "--force".as_ref(),
            worktree.as_os_str(),
        ],
    )?;
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
    let prune = git_output(env, &clone, &["worktree".as_ref(), "prune".as_ref()])?;
    if !prune.status.success() {
        bail!(
            "prune worktrees: {}",
            String::from_utf8_lossy(&prune.stderr).trim()
        );
    }
    if !remove.status.success() && worktree.exists() {
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
        if list
            .stdout
            .split(|byte| *byte == 0)
            .any(|entry| entry.strip_prefix(b"worktree ") == Some(worktree.as_os_str().as_bytes()))
        {
            bail!(
                "remove worktree: {}",
                String::from_utf8_lossy(&remove.stderr).trim()
            );
        }
        std::fs::remove_dir_all(&worktree).context("remove unregistered worktree")?;
    }
    Ok(())
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
                    && remove_merged_worktree(env, data, locks, &pr).is_ok()
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
    #[cfg(test)]
    Quiet,
}

struct Runner {
    pr: PrRef,
    data: PathBuf,
    env: ChildEnv,
    deadline: u64,
    settings: RepoSettings,
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
        let mut drain_until = None;
        loop {
            while let Ok(event) = read_rx.try_recv() {
                match event {
                    ReadEvent::Stdout(bytes) => stdout = Some(bytes),
                    ReadEvent::Stderr(line) => {
                        stderr.push_str(&line);
                        stderr.push('\n');
                        if matches!(mode, ProcessMode::Sit) && interrupted.is_none() {
                            emit(
                                &self.tx,
                                &self.pr,
                                SitState::Running {
                                    last_line: Some(line),
                                },
                            );
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
    fn prepare(&self) -> Result<Option<Outcome>> {
        let head: gh::PrHead =
            serde_json::from_slice(&self.checked(gh::pr_head_command(&self.env, &self.pr.url()))?)
                .context("parse PR head")?;
        if head.state == "MERGED" {
            return Ok(Some(Outcome::Merged));
        }
        if head.state == "CLOSED" {
            return Ok(Some(Outcome::Stopped("PR closed without merging".into())));
        }
        if head.is_cross_repository {
            return Ok(Some(Outcome::Failed("fork PRs are not supported".into())));
        }
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
            &format!(
                "+refs/heads/{0}:refs/remotes/origin/{0}",
                head.head_ref_name
            ),
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
                .arg(format!("origin/{}", head.head_ref_name));
            self.checked(add)?;
        }
        Ok(None)
    }
    fn execute(&self) -> Result<Outcome> {
        if let Some(reason) = self.interrupted() {
            return Ok(reason.outcome());
        }
        if let Some(outcome) = self.prepare()? {
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
            emit(&self.tx, &self.pr, SitState::Running { last_line: None });
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
    settings: RepoSettings,
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
        if outcome == Outcome::Merged {
            let _ = remove_merged_worktree(&runner.env, &runner.data, &runner.locks, &runner.pr);
        }
        if QUITTING.load(Ordering::SeqCst) {
            return;
        }
        notify(&runner.env, &runner.pr, &outcome);
        emit(&runner.tx, &runner.pr, SitState::Finished(outcome));
    });
    SitHandle { control }
}

fn notify(env: &ChildEnv, pr: &PrRef, outcome: &Outcome) {
    let (status, body) = match outcome {
        Outcome::Merged => ("Merged", "Pull request merged"),
        Outcome::TimedOut => ("Timed out", "Babysitting timed out"),
        Outcome::Stopped(reason) => ("Needs you", reason.lines().next().unwrap_or("")),
        Outcome::Failed(reason) => ("Needs you", reason.lines().next_back().unwrap_or("")),
        Outcome::Cancelled => return,
    };
    let _ = env
        .command("/usr/bin/osascript")
        .args([
            "-e",
            "on run argv",
            "-e",
            "display notification (item 2 of argv) with title (item 1 of argv)",
            "-e",
            "end run",
            &format!("{pr}: {status}"),
            body,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::process::CommandExt};
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

    #[test]
    fn removes_merged_worktree_with_untracked_files_and_branch() {
        let data = std::env::temp_dir().join(format!(
            "app-merged-worktree-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = data.join("source");
        fs::create_dir_all(&source).unwrap();
        let run = |args: &[&str]| {
            let output = Command::new("git").args(args).output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        let source_str = source.to_str().unwrap();
        run(&["init", source_str]);
        run(&[
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
        let pr: PrRef = "owner/repo#42".parse().unwrap();
        let clone = clone_path(&data, &pr.repo);
        fs::create_dir_all(clone.parent().unwrap()).unwrap();
        run(&[
            "clone",
            "--no-checkout",
            source_str,
            clone.to_str().unwrap(),
        ]);
        let tree = worktree_path(&data, &pr);
        fs::create_dir_all(tree.parent().unwrap()).unwrap();
        run(&[
            "-C",
            clone.to_str().unwrap(),
            "worktree",
            "add",
            "-b",
            "baby-sitter/pr-42",
            tree.to_str().unwrap(),
            "HEAD",
        ]);
        fs::write(tree.join("untracked"), "scratch").unwrap();
        let locks = Arc::new(Mutex::new(BTreeMap::new()));
        remove_merged_worktree(&ChildEnv::new(), &data, &locks, &pr).unwrap();
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
        fs::remove_dir_all(data).unwrap();
    }

    fn fixture(mode: &str) -> (Child, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "app-cancel-{}-{}-{mode}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
        let dir = std::env::temp_dir().join(format!(
            "app-cancel-runner-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
            settings: RepoSettings::default(),
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
