use crate::model::{PrRef, PrRow, PrState, RepoName};
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
    #[test]
    fn graphql_search_parses_all_pull_request_states() {
        let fixture = r#"[
          {"data":{"search":{"nodes":[
            {"number":1,"title":"draft","url":"https://example.com/1","baseRefName":"main","headRefName":"branch-1","isDraft":true,"isInMergeQueue":false,"mergeQueueEntry":null,"state":"OPEN","repository":{"nameWithOwner":"owner/repo"}},
            {"number":2,"title":"ready","url":"https://example.com/2","baseRefName":"branch-1","headRefName":"branch-2","isDraft":false,"isInMergeQueue":false,"mergeQueueEntry":null,"state":"OPEN","repository":{"nameWithOwner":"owner/repo"}},
            {"number":3,"title":"queued","url":"https://example.com/3","baseRefName":"main","headRefName":"branch-3","isDraft":true,"isInMergeQueue":true,"mergeQueueEntry":{"position":2},"state":"OPEN","repository":{"nameWithOwner":"owner/repo"}},
            {"number":4,"title":"merged","url":"https://example.com/4","baseRefName":"main","headRefName":"branch-4","isDraft":true,"isInMergeQueue":true,"mergeQueueEntry":null,"state":"MERGED","repository":{"nameWithOwner":"owner/repo"}},
            {"number":5,"title":"closed","url":"https://example.com/5","baseRefName":"main","headRefName":"branch-5","isDraft":true,"isInMergeQueue":true,"mergeQueueEntry":null,"state":"CLOSED","repository":{"nameWithOwner":"owner/repo"}}
          ]}}}
        ]"#;
        let rows = parse_pr_search(fixture.as_bytes()).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.state).collect::<Vec<_>>(),
            [
                PrState::Draft,
                PrState::Ready,
                PrState::Queued,
                PrState::Merged,
                PrState::Closed
            ]
        );
        assert_eq!(
            rows.iter()
                .map(|row| row.queue_position)
                .collect::<Vec<_>>(),
            [None, None, Some(2), None, None]
        );
        assert_eq!((&*rows[1].base, &*rows[1].head), ("branch-1", "branch-2"));
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
/// GPUI has no HTTP client for remote images here, so the UI loads the file.
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

const PR_SEARCH_QUERY: &str = r#"query($endCursor: String) {
  search(query: "is:pr author:@me is:open", type: ISSUE, first: 100, after: $endCursor) {
    pageInfo { hasNextPage endCursor }
    nodes {
      ... on PullRequest {
        number title url baseRefName headRefName isDraft isInMergeQueue state
        mergeQueueEntry { position }
        repository { nameWithOwner }
      }
    }
  }
}"#;

#[derive(Deserialize)]
struct SearchPage {
    data: SearchData,
}
#[derive(Deserialize)]
struct SearchData {
    search: SearchResults,
}
#[derive(Deserialize)]
struct SearchResults {
    nodes: Vec<SearchPr>,
}

#[derive(Deserialize)]
struct SearchRepo {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: RepoName,
}
#[derive(Deserialize)]
struct QueueEntry {
    position: u32,
}
#[derive(Deserialize)]
struct SearchPr {
    number: u64,
    title: String,
    repository: SearchRepo,
    url: String,
    #[serde(rename = "baseRefName")]
    base: String,
    #[serde(rename = "headRefName")]
    head: String,
    #[serde(rename = "isDraft")]
    is_draft: bool,
    #[serde(rename = "isInMergeQueue")]
    is_in_merge_queue: bool,
    #[serde(rename = "mergeQueueEntry")]
    merge_queue_entry: Option<QueueEntry>,
    state: SearchPrState,
}
#[derive(Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum SearchPrState {
    Open,
    Merged,
    Closed,
}

fn parse_pr_search(bytes: &[u8]) -> Result<Vec<PrRow>> {
    let pages: Vec<SearchPage> = serde_json::from_slice(bytes).context("parse pull requests")?;
    Ok(pages
        .into_iter()
        .flat_map(|page| page.data.search.nodes)
        .map(|row| PrRow {
            pr: PrRef {
                repo: row.repository.name_with_owner,
                number: row.number,
            },
            title: row.title,
            url: row.url,
            base: row.base,
            head: row.head,
            state: match row.state {
                SearchPrState::Merged => PrState::Merged,
                SearchPrState::Closed => PrState::Closed,
                SearchPrState::Open if row.is_in_merge_queue => PrState::Queued,
                SearchPrState::Open if row.is_draft => PrState::Draft,
                SearchPrState::Open => PrState::Ready,
            },
            queue_position: row.merge_queue_entry.map(|entry| entry.position),
        })
        .collect())
}

pub fn list_my_prs(env: &ChildEnv) -> Result<Vec<PrRow>> {
    let mut command = env.command("gh");
    command.args([
        "api",
        "graphql",
        "--paginate",
        "--slurp",
        "-f",
        &format!("query={PR_SEARCH_QUERY}"),
    ]);
    parse_pr_search(&output(command)?)
}

#[derive(Clone, Copy)]
pub enum GhPrAction {
    Ready,
    Draft,
    Close,
}

pub fn pr_action(env: &ChildEnv, pr: &PrRef, action: GhPrAction) -> Result<()> {
    let mut command = env.command("gh");
    command.args([
        "pr",
        match action {
            GhPrAction::Ready | GhPrAction::Draft => "ready",
            GhPrAction::Close => "close",
        },
    ]);
    command.arg(pr.number.to_string());
    if matches!(action, GhPrAction::Draft) {
        command.arg("--undo");
    }
    command.args(["--repo", &pr.repo.to_string()]);
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
