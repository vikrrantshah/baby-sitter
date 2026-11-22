use crate::model::{PrRef, PrRow, RepoName};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    os::unix::fs::PermissionsExt,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
};

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
        if !std::env::split_paths(&path).any(|dir| {
            dir.join("gh")
                .metadata()
                .is_ok_and(|entry| entry.is_file() && entry.permissions().mode() & 0o111 != 0)
        }) {
            let mut paths = vec![
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/usr/local/bin"),
            ];
            if let Some(home) = vars.get(&OsString::from("HOME")) {
                paths.push(PathBuf::from(home).join(".cargo/bin"));
            }
            paths.extend(std::env::split_paths(&path));
            if let Ok(path) = std::env::join_paths(paths) {
                vars.insert("PATH".into(), path);
            }
        }
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
