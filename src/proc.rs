use anyhow::{Context, bail};
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
#[derive(Debug)]
pub struct TimedOut;
impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("deadline exceeded")
    }
}
impl std::error::Error for TimedOut {}
pub fn run(cmd: &mut Command, input: Option<&str>, deadline: Instant) -> anyhow::Result<String> {
    let (ok, stdout, stderr) = output(cmd, input, deadline)?;
    if !ok {
        bail!("{cmd:?} failed: {stderr}");
    }
    Ok(stdout)
}
pub fn succeeds(cmd: &mut Command, deadline: Instant) -> anyhow::Result<bool> {
    let (ok, _, stderr) = output(cmd, None, deadline)?;
    if !ok && !stderr.trim().is_empty() {
        bail!("{cmd:?} failed: {stderr}");
    }
    Ok(ok)
}
fn output(
    cmd: &mut Command,
    input: Option<&str>,
    deadline: Instant,
) -> anyhow::Result<(bool, String, String)> {
    if Instant::now() >= deadline {
        return Err(TimedOut.into());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    let mut child = cmd.spawn().with_context(|| format!("spawning {cmd:?}"))?;
    let mut stdout = child.stdout.take().context("stdout pipe")?;
    let mut stderr = child.stderr.take().context("stderr pipe")?;
    let (out_tx, out_rx) = std::sync::mpsc::channel();
    let (err_tx, err_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stdout.read_to_end(&mut b);
        let _ = out_tx.send(b);
    });
    std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = stderr.read_to_end(&mut b);
        let _ = err_tx.send(b);
    });
    if let Some(s) = input {
        let bytes = s.as_bytes().to_vec();
        let mut stdin = child.stdin.take().expect("stdin pipe");
        std::thread::spawn(move || {
            let _ = stdin.write_all(&bytes);
        });
    }
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TimedOut.into());
        }
        std::thread::sleep(
            Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
        );
    };
    let stdout = out_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| TimedOut)?;
    let stderr = err_rx
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| TimedOut)?;
    Ok((
        status.success(),
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    ))
}
