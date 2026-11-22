use crate::{domain::*, proc};
use anyhow::{Context, ensure};
use std::{path::PathBuf, process::Command, time::Instant};

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
        || message
            .lines()
            .any(|line| matches!(line.trim(), "Baby-Sitter: fix-ci" | "Baby-Sitter: review"))
}

fn is_ci_configuration(path: &str) -> bool {
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

        let root = std::env::temp_dir().join(format!(
            "baby-sitter-ci-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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

        let root = std::env::temp_dir().join(format!(
            "baby-sitter-own-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
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
