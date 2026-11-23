use crate::model::{PrRef, PrRow, RepoName};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

fn merge_paths(login: Option<&OsStr>, current: &OsStr, home: Option<&OsStr>) -> OsString {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    let fallbacks = [
        Some(PathBuf::from("/opt/homebrew/bin")),
        Some(PathBuf::from("/usr/local/bin")),
        home.map(|home| PathBuf::from(home).join(".cargo/bin")),
    ];
    for path in login
        .into_iter()
        .flat_map(std::env::split_paths)
        .chain(std::env::split_paths(current))
        .chain(fallbacks.into_iter().flatten())
    {
        if seen.insert(path.as_os_str().to_owned()) {
            paths.push(path);
        }
    }
    std::env::join_paths(paths).unwrap_or_else(|_| current.to_owned())
}

fn login_shell_path(shell: &OsStr) -> Option<OsString> {
    let child = Command::new(shell)
        .args(["-lic", "printf '__BS_PATH__%s__BS_END__' \"$PATH\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let pid = child.id();
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = sender.send(child.wait_with_output());
    });
    let output = match receiver.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(output)) if output.status.success() => output,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
            return None;
        }
        _ => return None,
    };
    let stdout = std::str::from_utf8(&output.stdout).ok()?;
    let (_, rest) = stdout.split_once("__BS_PATH__")?;
    let (path, _) = rest.split_once("__BS_END__")?;
    (!path.is_empty()).then(|| OsString::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_paths_precede_current_paths_and_duplicates_are_removed() {
        assert_eq!(
            merge_paths(
                Some(OsStr::new("/node/bin:/shared:/node/bin:/opt/homebrew/bin")),
                OsStr::new("/shared:/usr/bin:/usr/local/bin:/usr/bin"),
                Some(OsStr::new("/home/user")),
            ),
            OsString::from(
                "/node/bin:/shared:/opt/homebrew/bin:/usr/bin:/usr/local/bin:/home/user/.cargo/bin"
            )
        );
    }

    #[test]
    fn missing_login_keeps_current_paths_and_adds_missing_fallbacks() {
        assert_eq!(
            merge_paths(
                None,
                OsStr::new("/usr/bin:/home/user/.cargo/bin:/usr/bin"),
                Some(OsStr::new("/home/user")),
            ),
            OsString::from("/usr/bin:/home/user/.cargo/bin:/opt/homebrew/bin:/usr/local/bin")
        );
    }

    #[test]
    fn missing_home_omits_cargo_fallback() {
        assert_eq!(
            merge_paths(None, OsStr::new("/usr/bin"), None),
            OsString::from("/usr/bin:/opt/homebrew/bin:/usr/local/bin")
        );
    }
}

#[derive(Clone)]
pub struct ChildEnv(BTreeMap<OsString, OsString>);
impl ChildEnv {
    pub fn new() -> Self {
        let mut vars: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
        for key in ["GIT_DIR", "GIT_WORK_TREE", "GH_REPO", "GH_HOST"] {
            vars.remove(&OsString::from(key));
        }
        let path = vars
            .get(&OsString::from("PATH"))
            .cloned()
            .unwrap_or_default();
        let shell = vars
            .get(OsStr::new("SHELL"))
            .map(OsString::as_os_str)
            .unwrap_or(OsStr::new("/bin/zsh"));
        let login = login_shell_path(shell);
        let merged = merge_paths(
            login.as_deref(),
            &path,
            vars.get(OsStr::new("HOME")).map(OsString::as_os_str),
        );
        vars.insert("PATH".into(), merged);
        Self(vars)
    }
    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(program);
        command.env_clear().envs(&self.0);
        command
    }
}

fn output(mut command: Command) -> Result<Vec<u8>> {
    let result = command.output().context("start command")?;
    if !result.status.success() {
        bail!("{}", String::from_utf8_lossy(&result.stderr).trim());
    }
    Ok(result.stdout)
}

/// Downloads the owner's GitHub profile picture once into `dir/avatars`.
/// GPUI here has no HTTP client for remote images, so the UI loads the file.
pub fn avatar(env: &ChildEnv, dir: &Path, owner: &str) -> Option<PathBuf> {
    let path = dir.join("avatars").join(format!("{owner}.png"));
    if path.is_file() {
        return Some(path);
    }
    std::fs::create_dir_all(path.parent()?).ok()?;
    let tmp = path.with_extension("download");
    let mut command = env.command("/usr/bin/curl");
    command
        .args(["-sfL", "--max-time", "10", "-o"])
        .arg(&tmp)
        .arg(format!("https://github.com/{owner}.png?size=64"));
    if output(command).is_ok() && std::fs::rename(&tmp, &path).is_ok() {
        Some(path)
    } else {
        let _ = std::fs::remove_file(&tmp);
        None
    }
}

#[derive(Deserialize)]
struct Repo {
    full_name: RepoName,
    archived: bool,
}
pub fn list_repos(env: &ChildEnv) -> Result<Vec<RepoName>> {
    let mut command = env.command("gh");
    command.args([
        "api",
        "--paginate",
        "--slurp",
        "user/repos?affiliation=owner,collaborator,organization_member&per_page=100",
    ]);
    let pages: Vec<Vec<Repo>> =
        serde_json::from_slice(&output(command)?).context("parse repositories")?;
    let mut repos: Vec<_> = pages
        .into_iter()
        .flatten()
        .filter(|repo| !repo.archived)
        .map(|repo| repo.full_name)
        .collect();
    repos.sort();
    repos.dedup();
    Ok(repos)
}

#[derive(Deserialize)]
struct SearchRepo {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: RepoName,
}
#[derive(Deserialize)]
struct SearchPr {
    number: u64,
    title: String,
    repository: SearchRepo,
    url: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    #[serde(rename = "updatedAt")]
    updated_at: String,
}
pub fn list_my_prs(env: &ChildEnv) -> Result<Vec<PrRow>> {
    let mut command = env.command("gh");
    command.args([
        "search",
        "prs",
        "--author",
        "@me",
        "--state",
        "open",
        "--limit",
        "1000",
        "--json",
        "number,title,repository,url,isDraft,updatedAt",
    ]);
    let rows: Vec<SearchPr> =
        serde_json::from_slice(&output(command)?).context("parse pull requests")?;
    Ok(rows
        .into_iter()
        .map(|row| PrRow {
            pr: PrRef {
                repo: row.repository.name_with_owner,
                number: row.number,
            },
            title: row.title,
            url: row.url,
            is_draft: row.is_draft,
            updated_at: row.updated_at,
        })
        .collect())
}

pub fn mark_ready(env: &ChildEnv, pr: &PrRef) -> Result<()> {
    let mut command = env.command("gh");
    command.args([
        "pr",
        "ready",
        &pr.number.to_string(),
        "--repo",
        &pr.repo.to_string(),
    ]);
    output(command)?;
    Ok(())
}

pub fn pr_state(env: &ChildEnv, url: &str) -> Result<String> {
    #[derive(Deserialize)]
    struct State {
        state: String,
    }
    let mut command = env.command("gh");
    command
        .args(["pr", "view", url, "--json", "state"])
        .process_group(0);
    let state: State = serde_json::from_slice(&output(command)?).context("parse PR state")?;
    Ok(state.state)
}

#[derive(Deserialize)]
pub struct PrHead {
    #[serde(rename = "headRefName")]
    pub head_ref_name: String,
    #[serde(rename = "isCrossRepository")]
    pub is_cross_repository: bool,
    pub state: String,
}
pub fn pr_head_command(env: &ChildEnv, url: &str) -> Command {
    let mut command = env.command("gh");
    command.args([
        "pr",
        "view",
        url,
        "--json",
        "headRefName,isCrossRepository,state",
    ]);
    command
}
