use crate::{domain::*, proc};
use anyhow::{Context, ensure};
use std::{
    fs,
    io::ErrorKind,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};

const PROTECTED_CI_PATHS: &[&str] = &[
    ".github/workflows/",
    ".github/actions/",
    ".circleci/",
    ".buildkite/",
    ".gitlab/",
    ".gitlab-ci.yml",
    ".travis.yml",
    "azure-pipelines.yml",
    "bitbucket-pipelines.yml",
    "Jenkinsfile",
    ".drone.yml",
    ".woodpecker.yml",
    ".github/dependabot.yml",
];
#[derive(Debug, PartialEq, Eq)]
pub enum MergeStart {
    Clean,
    Conflicted(Vec<String>),
}

pub struct Git {
    pub root: PathBuf,
    pub git_dir: PathBuf,
    remote: Option<String>,
    pub deadline: Instant,
}
impl Git {
    pub fn discover(deadline: Instant) -> anyhow::Result<Self> {
        let run = |args: &[&str]| proc::run(Command::new("git").args(args), None, deadline);
        let root = PathBuf::from(run(&["rev-parse", "--show-toplevel"])?.trim());
        let git_dir = PathBuf::from(run(&["rev-parse", "--absolute-git-dir"])?.trim());
        let branch = optional(run(&["symbolic-ref", "--quiet", "--short", "HEAD"]))?;
        let remote = match branch {
            Some(b) => optional(run(&[
                "config",
                "--get",
                &format!("branch.{}.remote", b.trim()),
            ]))?,
            None => None,
        }
        .map(|s| s.trim().to_owned());
        Ok(Self {
            root,
            git_dir,
            remote,
            deadline,
        })
    }
    pub fn state_dir(&self) -> PathBuf {
        self.git_dir.join("baby-sitter")
    }
    fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        proc::run(
            Command::new("git").current_dir(&self.root).args(args),
            None,
            self.deadline,
        )
    }
    pub fn head(&self) -> anyhow::Result<Sha> {
        Sha::parse(&self.run(&["rev-parse", "HEAD"])?)
    }
    pub fn is_clean(&self) -> anyhow::Result<bool> {
        Ok(self.run(&["status", "--porcelain"])?.is_empty())
    }
    pub fn ci_configuration_changes(&self) -> anyhow::Result<Vec<String>> {
        let status = self.run(&["status", "--porcelain=v1", "-z", "--untracked-files=all"])?;
        Ok(status_paths(&status)?
            .into_iter()
            .filter(|path| is_ci_configuration(path))
            .collect())
    }
    pub fn committed_ci_changes(&self, since: &Sha) -> anyhow::Result<Vec<String>> {
        let paths = self.run(&[
            "diff",
            "--name-only",
            "-z",
            "--no-renames",
            since.as_str(),
            "HEAD",
        ])?;
        Ok(paths
            .split_terminator('\0')
            .filter(|path| is_ci_configuration(path))
            .map(str::to_owned)
            .collect())
    }
    pub fn protected_push_changes(&self, since: &Sha) -> anyhow::Result<Vec<String>> {
        let commits = self.run(&[
            "rev-list",
            "--first-parent",
            "--reverse",
            &format!("{}..HEAD", since.as_str()),
        ])?;
        let mut paths = Vec::new();
        for commit in commits.lines() {
            let metadata = self.run(&["show", "-s", "--format=%P%n%B", commit])?;
            let (parents, message) = metadata
                .split_once('\n')
                .context("missing commit parents")?;
            let changed = if parents.split_whitespace().count() == 2
                && message
                    .lines()
                    .any(|line| line.trim() == "Baby-Sitter: resolve-conflicts")
            {
                self.run(&[
                    "show",
                    "--remerge-diff",
                    "--format=",
                    "--name-only",
                    "--no-renames",
                    "-z",
                    commit,
                ])?
            } else {
                self.run(&[
                    "diff",
                    "--name-only",
                    "--no-renames",
                    "-z",
                    &format!("{commit}^"),
                    commit,
                ])?
            };
            paths.extend(
                changed
                    .split_terminator('\0')
                    .filter(|path| is_ci_configuration(path))
                    .map(str::to_owned),
            );
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
    pub fn observe(&self, remote_head: &Sha) -> anyhow::Result<LocalState> {
        let branch = optional(self.run(&["symbolic-ref", "--quiet", "--short", "HEAD"]))?;
        let upstream = match branch {
            Some(b) => {
                optional(self.run(&["config", "--get", &format!("branch.{}.merge", b.trim())]))?
            }
            None => None,
        }
        .and_then(|s| s.trim().strip_prefix("refs/heads/").map(str::to_owned));
        let fetched = if let (Some(remote), Some(branch)) = (&self.remote, &upstream) {
            self.run(&["fetch", "--no-tags", remote, branch])?;
            Some(Sha::parse(&self.run(&["rev-parse", "FETCH_HEAD"])?)?)
        } else {
            None
        };
        let head = self.head()?;
        let ancestor = |a: &Sha, b: &Sha| {
            proc::succeeds(
                Command::new("git").current_dir(&self.root).args([
                    "merge-base",
                    "--is-ancestor",
                    a.as_str(),
                    b.as_str(),
                ]),
                self.deadline,
            )
        };
        let relation = if upstream.is_none() || head == *remote_head {
            Relation::Equal
        } else if ancestor(&head, remote_head)? {
            Relation::Behind
        } else if ancestor(remote_head, &head)? {
            if fetched.as_ref() == Some(&head) {
                Relation::Pushed
            } else if self.only_own_commits_since(remote_head)? {
                Relation::Unpushed
            } else {
                Relation::Ahead
            }
        } else {
            Relation::Diverged
        };
        let dirty = self
            .run(&["status", "--porcelain"])?
            .lines()
            .map(str::to_owned)
            .collect();
        Ok(LocalState {
            upstream_branch: upstream,
            head,
            dirty,
            relation,
        })
    }
    pub fn fast_forward(&self, sha: &Sha) -> anyhow::Result<()> {
        self.run(&["merge", "--ff-only", sha.as_str()])?;
        Ok(())
    }
    pub fn fetch(&self, remote: &str, base: &str) -> anyhow::Result<()> {
        self.run(&[
            "fetch",
            "--no-tags",
            remote,
            &format!("+refs/heads/{base}:refs/remotes/{remote}/{base}"),
        ])?;
        Ok(())
    }
    pub fn merge_no_commit(&self, base: &str) -> anyhow::Result<MergeStart> {
        match self.run(&["merge", "--no-ff", "--no-commit", &format!("origin/{base}")]) {
            Ok(_) => Ok(MergeStart::Clean),
            Err(error) => {
                if error.downcast_ref::<proc::TimedOut>().is_some() || !self.merge_in_progress() {
                    return Err(error);
                }
                let files = self.run(&["diff", "--name-only", "--diff-filter=U", "-z"])?;
                let files: Vec<_> = files.split_terminator('\0').map(str::to_owned).collect();
                if files.is_empty() {
                    Err(error)
                } else {
                    Ok(MergeStart::Conflicted(files))
                }
            }
        }
    }
    pub fn merge_in_progress(&self) -> bool {
        self.git_dir.join("MERGE_HEAD").exists()
    }
    pub fn merge_abort(&self) -> anyhow::Result<()> {
        if self.merge_in_progress() {
            proc::run(
                Command::new("git")
                    .current_dir(&self.root)
                    .args(["merge", "--abort"]),
                None,
                Instant::now() + Duration::from_secs(30),
            )?;
        }
        Ok(())
    }
    pub fn unstaged_paths(&self) -> anyhow::Result<Vec<String>> {
        let paths = self.run(&["diff", "--name-only", "--no-renames", "-z"])?;
        Ok(paths.split_terminator('\0').map(str::to_owned).collect())
    }
    pub fn untracked_paths(&self) -> anyhow::Result<Vec<String>> {
        let paths = self.run(&["ls-files", "--others", "--exclude-standard", "-z"])?;
        Ok(paths.split_terminator('\0').map(str::to_owned).collect())
    }
    pub fn index_state(&self) -> anyhow::Result<String> {
        self.run(&["ls-files", "--stage", "-z"])
    }
    pub fn has_conflict_markers(&self, files: &[String]) -> anyhow::Result<bool> {
        for file in files {
            let path = self.root.join(file);
            let metadata = match fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(error).with_context(|| format!("reading {file}")),
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            let contents = fs::read(&path).with_context(|| format!("reading {file}"))?;
            if contents.split(|byte| *byte == b'\n').any(|line| {
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                line.starts_with(b"<<<<<<< ") || line == b"=======" || line.starts_with(b">>>>>>> ")
            }) {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub fn commit_merge(&self, message: &str) -> anyhow::Result<()> {
        self.run(&["add", "-A"])?;
        self.run(&["commit", "-m", message])?;
        Ok(())
    }
    pub fn commit_all_with(&self, message: &str) -> anyhow::Result<bool> {
        if self.is_clean()? {
            return Ok(false);
        }
        self.run(&["add", "-A"])?;
        self.run(&["commit", "-m", message])?;
        Ok(true)
    }
    pub fn push(&self, branch: &str) -> anyhow::Result<()> {
        let remote = self.remote.as_deref().context("no upstream remote")?;
        self.run(&["push", remote, &format!("HEAD:refs/heads/{branch}")])?;
        Ok(())
    }
    /// True when `base..HEAD` is non-empty and every commit in it is one baby-sitter made.
    fn only_own_commits_since(&self, base: &Sha) -> anyhow::Result<bool> {
        let log = self.run(&[
            "log",
            "--first-parent",
            "--format=%B%x00",
            &format!("{}..HEAD", base.as_str()),
        ])?;
        let mut messages = log
            .split('\0')
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .peekable();
        Ok(messages.peek().is_some() && messages.all(is_own_commit))
    }
}
fn optional(result: anyhow::Result<String>) -> anyhow::Result<Option<String>> {
    match result {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.downcast_ref::<proc::TimedOut>().is_some() => Err(e),
        Err(_) => Ok(None),
    }
}
pub const REVIEW_COMMIT_MESSAGE: &str =
    "baby-sitter: address review feedback\n\nBaby-Sitter: review";
fn is_own_commit(message: &str) -> bool {
    message.lines().next() == Some("baby-sitter: address review feedback")
        || message.lines().any(|line| {
            matches!(
                line.trim(),
                "Baby-Sitter: fix-ci" | "Baby-Sitter: review" | "Baby-Sitter: resolve-conflicts"
            )
        })
}

pub(crate) fn is_ci_configuration(path: &str) -> bool {
    PROTECTED_CI_PATHS.iter().any(|protected| {
        if protected.ends_with('/') {
            path.starts_with(protected)
        } else {
            path == *protected
        }
    })
}

fn status_paths(status: &str) -> anyhow::Result<Vec<String>> {
    let mut entries = status.split_terminator('\0');
    let mut paths = Vec::new();
    while let Some(entry) = entries.next() {
        let bytes = entry.as_bytes();
        ensure!(
            bytes.len() >= 3 && bytes[2] == b' ',
            "invalid git status entry"
        );
        paths.push(entry[3..].to_owned());
        if matches!(bytes[0], b'R' | b'C') || matches!(bytes[1], b'R' | b'C') {
            paths.push(
                entries
                    .next()
                    .context("missing git rename source")?
                    .to_owned(),
            );
        }
    }
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRepo {
        root: PathBuf,
        git: Git,
    }

    impl TestRepo {
        fn new(conflict: Option<&str>, base_ci: bool) -> Self {
            let root = crate::test_support::unique_test_dir("baby-sitter-merge");
            let work = root.join("work");
            fs::create_dir_all(&work).unwrap();
            let git = Git {
                git_dir: work.join(".git"),
                root: work.clone(),
                remote: Some("origin".into()),
                deadline: Instant::now() + Duration::from_secs(60),
            };
            git.run(&["init", "-q", "-b", "main"]).unwrap();
            git.run(&["config", "user.name", "Test"]).unwrap();
            git.run(&["config", "user.email", "test@example.com"])
                .unwrap();
            git.run(&["config", "commit.gpgsign", "false"]).unwrap();
            fs::write(work.join("src.txt"), "original\n").unwrap();
            if let Some(path) = conflict.filter(|path| *path != "src.txt") {
                fs::create_dir_all(work.join(path).parent().unwrap()).unwrap();
                fs::write(work.join(path), "original\n").unwrap();
            }
            git.commit_all_with("initial").unwrap();
            git.run(&["branch", "feature"]).unwrap();
            if let Some(path) = conflict {
                fs::write(work.join(path), "base version\n").unwrap();
            } else {
                fs::write(work.join("base.txt"), "base version\n").unwrap();
            }
            if base_ci {
                let path = work.join(".github/workflows/build.yml");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, "name: base CI\n").unwrap();
            }
            git.commit_all_with("base change").unwrap();
            let bare = root.join("origin.git");
            git.run(&["init", "--bare", "-q", bare.to_str().unwrap()])
                .unwrap();
            git.run(&["remote", "add", "origin", bare.to_str().unwrap()])
                .unwrap();
            git.run(&["push", "-q", "origin", "main"]).unwrap();
            git.run(&["checkout", "-q", "feature"]).unwrap();
            fs::write(
                work.join(conflict.unwrap_or("src.txt")),
                "feature version\n",
            )
            .unwrap();
            git.commit_all_with("feature change").unwrap();
            git.run(&["push", "-q", "-u", "origin", "feature"]).unwrap();
            Self { root, git }
        }

        fn head(&self) -> Head {
            Head {
                branch: "feature".into(),
                sha: self.git.head().unwrap(),
            }
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn conflicted_merge_lists_paths_and_abort_restores_the_checkout() {
        let repo = TestRepo::new(Some("file with space.txt"), false);
        repo.git.fetch("origin", "main").unwrap();
        assert_eq!(
            repo.git.merge_no_commit("main").unwrap(),
            MergeStart::Conflicted(vec!["file with space.txt".into()])
        );
        assert!(repo.git.merge_in_progress());
        assert!(
            repo.git
                .has_conflict_markers(&["file with space.txt".into()])
                .unwrap()
        );
        repo.git.merge_abort().unwrap();
        assert!(!repo.git.merge_in_progress());
        assert!(repo.git.is_clean().unwrap());
        assert_eq!(
            fs::read_to_string(repo.git.root.join("file with space.txt")).unwrap(),
            "feature version\n"
        );
    }

    #[test]
    fn conflict_marker_detection_checks_each_marker_line() {
        let repo = TestRepo::new(None, false);
        let file = repo.git.root.join("markers.txt");
        let files = vec!["markers.txt".into()];
        for marker in ["<<<<<<< HEAD\n", "=======\n", ">>>>>>> main\n"] {
            fs::write(&file, marker).unwrap();
            assert!(repo.git.has_conflict_markers(&files).unwrap());
        }
        fs::write(&file, "======= text\n <<<<<<< HEAD\n").unwrap();
        assert!(!repo.git.has_conflict_markers(&files).unwrap());
    }

    #[test]
    fn own_commit_recognizes_the_conflict_resolution_trailer() {
        assert!(is_own_commit(
            "Resolve conflicts\n\nBaby-Sitter: resolve-conflicts"
        ));
        assert!(!is_own_commit(
            "Resolve conflicts\n\nNot-Baby-Sitter: resolve-conflicts"
        ));
    }

    #[test]
    fn conflict_resolution_commits_and_pushes_with_base_ci_changes() {
        let repo = TestRepo::new(Some("src.txt"), true);
        let head = repo.head();
        crate::act::resolve_conflicts(&repo.git, &head, "main", |files| {
            assert_eq!(files, &["src.txt"]);
            fs::write(repo.git.root.join("src.txt"), "both versions\n").unwrap();
            Ok(crate::codex::ConflictFix {
                outcome: crate::codex::ConflictOutcome::Resolved,
                summary: "Keep both versions\nExplain the resolution".into(),
            })
        })
        .unwrap();
        let local = repo.git.head().unwrap();
        let remote = repo
            .git
            .run(&["ls-remote", "origin", "refs/heads/feature"])
            .unwrap();
        assert!(remote.starts_with(local.as_str()));
        assert!(repo.git.is_clean().unwrap());
        assert!(!repo.git.merge_in_progress());
        let message = repo.git.run(&["log", "-1", "--format=%B"]).unwrap();
        assert!(message.starts_with("Keep both versions\n\nExplain the resolution\n\n"));
        assert!(is_own_commit(&message));
        assert!(repo.git.only_own_commits_since(&head.sha).unwrap());
        assert!(
            repo.git
                .protected_push_changes(&head.sha)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn clean_merge_is_committed_and_pushed_without_invoking_codex() {
        let repo = TestRepo::new(None, true);
        let head = repo.head();
        crate::act::resolve_conflicts(&repo.git, &head, "main", |_| {
            panic!("a clean merge must not invoke codex");
        })
        .unwrap();
        assert_ne!(repo.git.head().unwrap(), head.sha);
        assert!(repo.git.is_clean().unwrap());
        assert!(repo.git.only_own_commits_since(&head.sha).unwrap());
        assert!(
            repo.git
                .protected_push_changes(&head.sha)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn stale_conflict_report_with_an_already_merged_base_stops_without_a_commit() {
        let repo = TestRepo::new(None, false);
        let head = repo.head();
        crate::act::resolve_conflicts(&repo.git, &head, "main", |_| {
            panic!("a clean merge must not invoke codex");
        })
        .unwrap();
        let next = repo.head();
        let error = crate::act::resolve_conflicts(&repo.git, &next, "main", |_| {
            panic!("an already merged base must not invoke codex");
        })
        .unwrap_err();
        assert!(error.to_string().contains("already contains origin/main"));
        assert_eq!(repo.git.head().unwrap(), next.sha);
        assert!(repo.git.is_clean().unwrap());
    }

    #[test]
    fn conflict_resolution_refuses_a_dirty_or_changed_checkout() {
        let repo = TestRepo::new(Some("src.txt"), false);
        let head = repo.head();
        fs::write(repo.git.root.join("src.txt"), "uncommitted\n").unwrap();
        let error = crate::act::resolve_conflicts(&repo.git, &head, "main", |_| {
            panic!("a dirty checkout must not invoke codex");
        })
        .unwrap_err();
        assert!(error.to_string().contains("working tree became dirty"));
        assert!(!repo.git.merge_in_progress());
        fs::write(repo.git.root.join("src.txt"), "feature version\n").unwrap();
        let wrong_head = Head {
            branch: head.branch,
            sha: Sha::parse(&"a".repeat(40)).unwrap(),
        };
        let error = crate::act::resolve_conflicts(&repo.git, &wrong_head, "main", |_| {
            panic!("a changed HEAD must not invoke codex");
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("local HEAD moved since snapshot")
        );
        assert!(!repo.git.merge_in_progress());
    }

    #[test]
    fn interrupted_merge_push_can_be_retried_without_blocking_base_ci() {
        let mut repo = TestRepo::new(Some("src.txt"), true);
        let head = repo.head();
        repo.git.remote = Some("missing-remote".into());
        assert!(
            crate::act::resolve_conflicts(&repo.git, &head, "main", |_| {
                fs::write(repo.git.root.join("src.txt"), "resolved\n").unwrap();
                Ok(crate::codex::ConflictFix {
                    outcome: crate::codex::ConflictOutcome::Resolved,
                    summary: "Resolve the conflict".into(),
                })
            })
            .is_err()
        );
        assert!(repo.git.is_clean().unwrap());
        assert!(repo.git.only_own_commits_since(&head.sha).unwrap());
        assert!(
            repo.git
                .protected_push_changes(&head.sha)
                .unwrap()
                .is_empty()
        );
        repo.git.remote = Some("origin".into());
        repo.git.push("feature").unwrap();
        fs::write(
            repo.git.root.join(".github/workflows/build.yml"),
            "name: changed locally\n",
        )
        .unwrap();
        repo.git
            .commit_all_with("fix tests\n\nBaby-Sitter: fix-ci")
            .unwrap();
        assert_eq!(
            repo.git.protected_push_changes(&head.sha).unwrap(),
            [".github/workflows/build.yml"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn commit_hook_cannot_smuggle_ci_changes_into_a_merge_or_a_retry() {
        use std::os::unix::fs::PermissionsExt;

        let repo = TestRepo::new(Some("src.txt"), true);
        let head = repo.head();
        let hooks = repo.git.git_dir.join("test-hooks");
        fs::create_dir_all(&hooks).unwrap();
        repo.git
            .run(&["config", "core.hooksPath", ".git/test-hooks"])
            .unwrap();
        let hook = hooks.join("pre-commit");
        fs::write(
            &hook,
            "#!/bin/sh\nprintf 'name: injected\\n' > .github/workflows/build.yml\ngit add .github/workflows/build.yml\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        let error = crate::act::resolve_conflicts(&repo.git, &head, "main", |_| {
            fs::write(repo.git.root.join("src.txt"), "resolved\n").unwrap();
            Ok(crate::codex::ConflictFix {
                outcome: crate::codex::ConflictOutcome::Resolved,
                summary: "Resolve the conflict".into(),
            })
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("merge commit touches CI configuration")
        );
        assert!(!repo.git.merge_in_progress());
        assert!(repo.git.only_own_commits_since(&head.sha).unwrap());
        assert_eq!(
            repo.git.protected_push_changes(&head.sha).unwrap(),
            [".github/workflows/build.yml"]
        );
        let remote = repo
            .git
            .run(&["ls-remote", "origin", "refs/heads/feature"])
            .unwrap();
        assert!(remote.starts_with(head.sha.as_str()));
    }

    #[test]
    fn unsafe_or_unresolved_conflicts_abort_the_merge() {
        for failure in ["human", "error", "markers", "outside", "index"] {
            let repo = TestRepo::new(Some("src.txt"), false);
            let head = repo.head();
            let error = crate::act::resolve_conflicts(&repo.git, &head, "main", |_| {
                if failure == "error" {
                    anyhow::bail!("codex failed");
                }
                if failure == "outside" {
                    fs::write(repo.git.root.join("outside.txt"), "not allowed\n").unwrap();
                }
                if failure == "index" {
                    repo.git.run(&["add", "src.txt"]).unwrap();
                }
                if !matches!(failure, "markers" | "index") {
                    fs::write(repo.git.root.join("src.txt"), "resolved\n").unwrap();
                }
                Ok(crate::codex::ConflictFix {
                    outcome: if failure == "human" {
                        crate::codex::ConflictOutcome::NeedsHuman
                    } else {
                        crate::codex::ConflictOutcome::Resolved
                    },
                    summary: "A person must decide".into(),
                })
            })
            .unwrap_err();
            let error = format!("{error:#}");
            match failure {
                "human" | "error" => assert!(error.contains("Merge conflicts need a person")),
                "markers" => assert!(error.contains("conflict markers remain")),
                "outside" => assert!(error.contains("codex edited files outside the conflicts")),
                "index" => assert!(error.contains("codex changed the Git index")),
                _ => unreachable!(),
            }
            assert!(!repo.git.merge_in_progress(), "{failure}");
            assert_eq!(repo.git.head().unwrap(), head.sha);
        }
    }

    #[test]
    fn conflicting_ci_configuration_is_left_for_a_person() {
        let repo = TestRepo::new(Some(".github/workflows/build.yml"), false);
        let head = repo.head();
        let error = crate::act::resolve_conflicts(&repo.git, &head, "main", |_| {
            panic!("codex must not receive CI conflicts");
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Merge conflict in CI configuration: .github/workflows/build.yml")
        );
        assert!(!repo.git.merge_in_progress());
        assert!(repo.git.is_clean().unwrap());
    }

    #[test]
    fn abort_still_runs_after_the_main_deadline() {
        let mut repo = TestRepo::new(Some("src.txt"), false);
        let head = repo.head();
        repo.git.deadline = Instant::now() + Duration::from_secs(2);
        let error = crate::act::resolve_conflicts(&repo.git, &head, "main", |_| {
            std::thread::sleep(
                repo.git.deadline.saturating_duration_since(Instant::now())
                    + Duration::from_millis(10),
            );
            Err(proc::TimedOut.into())
        })
        .unwrap_err();
        assert!(error.downcast_ref::<proc::TimedOut>().is_some());
        assert!(!repo.git.merge_in_progress());
        repo.git.deadline = Instant::now() + Duration::from_secs(10);
        assert!(repo.git.is_clean().unwrap());
    }

    #[test]
    fn protects_ci_configuration_paths_only() {
        for path in [
            ".github/workflows/build.yml",
            ".github/actions/test/action.yml",
            ".circleci/config.yml",
            ".buildkite/pipeline.yml",
            ".gitlab/ci/test.yml",
            ".gitlab-ci.yml",
            ".travis.yml",
            "azure-pipelines.yml",
            "bitbucket-pipelines.yml",
            "Jenkinsfile",
            ".drone.yml",
            ".woodpecker.yml",
            ".github/dependabot.yml",
        ] {
            assert!(is_ci_configuration(path), "{path}");
        }
        for path in [
            ".pre-commit-config.yaml",
            ".github/workflows-extra/build.yml",
            ".github/actions-extra/action.yml",
            "src/Jenkinsfile",
            ".gitlab-ci.yml.backup",
            "README.md",
        ] {
            assert!(!is_ci_configuration(path), "{path}");
        }
    }

    #[test]
    fn porcelain_paths_include_untracked_and_both_sides_of_renames() {
        let paths = status_paths(
            "?? .github/workflows/new job.yml\0R  src/file\0.github/actions/old/action.yml\0",
        )
        .unwrap();
        assert_eq!(
            paths,
            [
                ".github/workflows/new job.yml",
                "src/file",
                ".github/actions/old/action.yml"
            ]
        );
        let protected: Vec<_> = paths
            .iter()
            .filter(|path| is_ci_configuration(path))
            .collect();
        assert_eq!(protected.len(), 2);
        assert!(status_paths("R  src/file\0").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn committed_ci_changes_detects_files_added_by_a_commit_hook() {
        use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

        let root = crate::test_support::unique_test_dir("baby-sitter-ci");
        fs::create_dir(&root).unwrap();
        let git = Git {
            git_dir: root.join(".git"),
            root: root.clone(),
            remote: None,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        git.run(&["init", "-q"]).unwrap();
        git.run(&["config", "user.name", "Test"]).unwrap();
        git.run(&["config", "user.email", "test@example.com"])
            .unwrap();
        git.run(&["config", "commit.gpgsign", "false"]).unwrap();
        let hooks = git.git_dir.join("baby-sitter-test-hooks");
        fs::create_dir_all(&hooks).unwrap();
        git.run(&["config", "core.hooksPath", ".git/baby-sitter-test-hooks"])
            .unwrap();
        git.run(&["commit", "--allow-empty", "-m", "initial"])
            .unwrap();
        let before = git.head().unwrap();

        let hook = hooks.join("pre-commit");
        fs::write(
            &hook,
            "#!/bin/sh\nmkdir -p .github/workflows\nprintf 'name: CI\\n' > .github/workflows/ci.yml\ngit add .github/workflows/ci.yml\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("code.rs"), "fn main() {}\n").unwrap();
        assert!(git.ci_configuration_changes().unwrap().is_empty());
        assert!(git.commit_all_with("fix CI").unwrap());
        assert_eq!(
            git.committed_ci_changes(&before).unwrap(),
            [".github/workflows/ci.yml"]
        );

        fs::remove_file(hook).unwrap();
        let before_rename = git.head().unwrap();
        git.run(&["mv", ".github/workflows/ci.yml", "ci.yml"])
            .unwrap();
        git.run(&["commit", "-m", "move config"]).unwrap();
        assert_eq!(
            git.committed_ci_changes(&before_rename).unwrap(),
            [".github/workflows/ci.yml"]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn only_own_commits_since_accepts_baby_sitter_commits_and_rejects_yours() {
        use std::{fs, time::Duration};

        let root = crate::test_support::unique_test_dir("baby-sitter-own");
        fs::create_dir(&root).unwrap();
        let git = Git {
            git_dir: root.join(".git"),
            root: root.clone(),
            remote: None,
            deadline: Instant::now() + Duration::from_secs(60),
        };
        git.run(&["init", "-q"]).unwrap();
        git.run(&["config", "user.name", "Test"]).unwrap();
        git.run(&["config", "user.email", "test@example.com"])
            .unwrap();
        git.run(&["config", "commit.gpgsign", "false"]).unwrap();
        git.run(&["commit", "--allow-empty", "-m", "initial"])
            .unwrap();
        let base = git.head().unwrap();
        assert!(!git.only_own_commits_since(&base).unwrap());

        fs::write(root.join("a.txt"), "fix\n").unwrap();
        assert!(
            git.commit_all_with("fix: repair tests\n\nBaby-Sitter: fix-ci")
                .unwrap()
        );
        fs::write(root.join("b.txt"), "review\n").unwrap();
        assert!(git.commit_all_with(REVIEW_COMMIT_MESSAGE).unwrap());
        assert!(git.only_own_commits_since(&base).unwrap());

        fs::write(root.join("c.txt"), "mine\n").unwrap();
        assert!(git.commit_all_with("wip: my local change").unwrap());
        assert!(!git.only_own_commits_since(&base).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
