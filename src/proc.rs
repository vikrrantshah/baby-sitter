use anyhow::{Context, bail};
use std::{
    collections::VecDeque,
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
    run_with_limit(cmd, input, deadline, None)
}
pub fn run_tail(cmd: &mut Command, limit: usize, deadline: Instant) -> anyhow::Result<String> {
    run_with_limit(cmd, None, deadline, Some(limit))
}
fn run_with_limit(
    cmd: &mut Command,
    input: Option<&str>,
    deadline: Instant,
    stdout_limit: Option<usize>,
) -> anyhow::Result<String> {
    let (ok, stdout, stderr) = output(cmd, input, deadline, stdout_limit)?;
    if !ok {
        bail!("{cmd:?} failed: {stderr}");
    }
    Ok(stdout)
}
pub fn succeeds(cmd: &mut Command, deadline: Instant) -> anyhow::Result<bool> {
    let (ok, _, stderr) = output(cmd, None, deadline, None)?;
    if !ok && !stderr.trim().is_empty() {
        bail!("{cmd:?} failed: {stderr}");
    }
    Ok(ok)
}
fn output(
    cmd: &mut Command,
    input: Option<&str>,
    deadline: Instant,
    stdout_limit: Option<usize>,
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
        let b = if let Some(limit) = stdout_limit {
            read_tail(&mut stdout, limit)
        } else {
            let mut b = Vec::new();
            let _ = stdout.read_to_end(&mut b);
            b
        };
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
    Ok((status.success(), utf8_lossy(stdout), utf8_lossy(stderr)))
}

fn read_tail(reader: &mut impl Read, limit: usize) -> Vec<u8> {
    let mut tail = VecDeque::new();
    let mut chunk = [0; 8192];
    while let Ok(n) = reader.read(&mut chunk) {
        if n == 0 {
            break;
        }
        let bytes = &chunk[n.saturating_sub(limit)..n];
        let discard = (tail.len() + bytes.len()).saturating_sub(limit);
        tail.drain(..discard);
        tail.extend(bytes.iter().copied());
    }
    tail.into()
}

fn utf8_lossy(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes)
        .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_only_the_end_of_large_process_output() {
        let input = format!("{}last", "x".repeat(100_000));
        assert_eq!(read_tail(&mut input.as_bytes(), 6), b"xxlast");
        assert!(read_tail(&mut input.as_bytes(), 0).is_empty());
    }
}
