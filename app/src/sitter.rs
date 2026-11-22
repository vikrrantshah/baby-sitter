use crate::{
    gh::{self, ChildEnv},
    model::{self, Outcome, PrRef, RepoName, RepoSettings, SitState},
};
use anyhow::{Context, Result, bail};
use async_channel::Sender;
use std::{
    collections::BTreeMap,
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
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
        } else if now() >= self.deadline {
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
            bail!(match interrupted {
                Interrupted::Cancelled => "cancelled",
                Interrupted::Deadline => "timed out",
            });
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
            .with_context(|| format!("start {command:?}"))?;
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
            } else if Instant::now() >= drain_until.unwrap() {
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
            bail!(match reason {
                Interrupted::Cancelled => "cancelled",
                Interrupted::Deadline => "timed out",
            });
        }
        if !output.status.success() {
            bail!("{}", output.stderr.trim());
        }
        Ok(output.stdout)
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
        let lock = {
            let mut locks = self.locks.lock().unwrap();
            locks
                .entry(self.pr.repo.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = loop {
            match lock.try_lock() {
                Ok(guard) => break guard,
                Err(std::sync::TryLockError::WouldBlock) => {
                    if let Some(reason) = self.interrupted() {
                        bail!(match reason {
                            Interrupted::Cancelled => "cancelled",
                            Interrupted::Deadline => "timed out",
                        });
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
                .args(["--", "--no-checkout", "--filter=blob:none"]);
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
            return Ok(match reason {
                Interrupted::Cancelled => Outcome::Cancelled,
                Interrupted::Deadline => Outcome::TimedOut,
            });
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
                return Ok(match reason {
                    Interrupted::Cancelled => Outcome::Cancelled,
                    Interrupted::Deadline => Outcome::TimedOut,
                });
            }
            emit(&self.tx, &self.pr, SitState::Running { last_line: None });
            let mut command = self.env.command(&cli);
            command
                .arg(self.pr.url())
                .args([
                    "--timeout",
                    &format!("{}s", self.deadline.saturating_sub(now())),
                    "--interval",
                    &format!("{}s", self.settings.interval.as_secs()),
                ])
                .current_dir(worktree_path(&self.data, &self.pr));
            let output = self.run(command, ProcessMode::Sit)?;
            if model::lock_held(output.status.code(), &output.stderr)
                && output.interrupted.is_none()
            {
                emit(&self.tx, &self.pr, SitState::Preparing);
                self.pause(
                    self.settings
                        .interval
                        .min(Duration::from_secs(self.deadline.saturating_sub(now()))),
                );
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
    fn remove_worktree(&self) {
        let worktree = worktree_path(&self.data, &self.pr);
        if !worktree.exists() {
            return;
        }
        let clone = clone_path(&self.data, &self.pr.repo);
        let lock = {
            let mut locks = self.locks.lock().unwrap();
            locks
                .entry(self.pr.repo.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let Ok(_guard) = lock.lock() else { return };
        let mut command = self.env.command("git");
        command
            .arg("-C")
            .arg(clone)
            .args(["worktree", "remove"])
            .arg(worktree);
        let _ = self.run(command, ProcessMode::Quiet);
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
                Some(Interrupted::Cancelled) => Outcome::Cancelled,
                Some(Interrupted::Deadline) => Outcome::TimedOut,
                None => Outcome::Failed(format!("{error:#}")),
            },
        };
        if QUITTING.load(Ordering::SeqCst) {
            return;
        }
        if outcome == Outcome::Merged {
            runner.remove_worktree();
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
        Outcome::Stopped(reason) | Outcome::Failed(reason) => {
            ("Needs you", reason.lines().next().unwrap_or(""))
        }
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
            deadline: now() + 60,
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
