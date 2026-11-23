use crate::{
    gh::ChildEnv,
    notifications::{self, NotificationAccess},
};
use std::{
    os::unix::process::CommandExt,
    process::{Output, Stdio},
    sync::mpsc,
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Required,
    Optional,
    Permissions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fix {
    Copy(String),
    OpenSettings(&'static str),
    AllowNotifications,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckState {
    Ok(String),
    Missing { detail: String, fix: Fix },
    Failed(String),
}

pub struct CheckSpec {
    pub id: &'static str,
    pub label: &'static str,
    pub section: Section,
    pub run: fn(&ChildEnv) -> CheckState,
}

pub const CHECKS: &[CheckSpec] = &[
    CheckSpec {
        id: "gh",
        label: "GitHub CLI",
        section: Section::Required,
        run: check_gh,
    },
    CheckSpec {
        id: "gh-auth",
        label: "GitHub sign-in",
        section: Section::Required,
        run: check_gh_auth,
    },
    CheckSpec {
        id: "git",
        label: "Git",
        section: Section::Required,
        run: check_git,
    },
    CheckSpec {
        id: "codex",
        label: "Codex CLI",
        section: Section::Required,
        run: check_codex,
    },
    CheckSpec {
        id: "codex-auth",
        label: "Codex sign-in",
        section: Section::Required,
        run: check_codex_auth,
    },
    CheckSpec {
        id: "pnpm",
        label: "pnpm",
        section: Section::Optional,
        run: check_pnpm,
    },
    CheckSpec {
        id: "npm",
        label: "npm",
        section: Section::Optional,
        run: check_npm,
    },
    CheckSpec {
        id: "yarn",
        label: "Yarn",
        section: Section::Optional,
        run: check_yarn,
    },
    CheckSpec {
        id: "notifications",
        label: "Notifications",
        section: Section::Permissions,
        run: check_notifications,
    },
];

fn command(env: &ChildEnv, program: &str, args: &[&str]) -> Result<Output, String> {
    let mut command = env.command(program);
    let child = command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "not found".to_owned()
            } else {
                e.to_string()
            }
        })?;
    let pid = child.id();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => {
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
            Err("Check timed out".into())
        }
    }
}

fn missing(detail: &str, fix: &str) -> CheckState {
    CheckState::Missing {
        detail: detail.into(),
        fix: Fix::Copy(fix.into()),
    }
}

fn version_output(
    env: &ChildEnv,
    program: &str,
    fix: &str,
    detail: &str,
) -> Result<String, CheckState> {
    match command(env, program, &["--version"]) {
        Ok(output) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        Ok(output) => Err(CheckState::Failed(format!(
            "Command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))),
        Err(error) if error == "not found" => Err(missing(detail, fix)),
        Err(error) => Err(CheckState::Failed(error)),
    }
}

fn parse_gh_version(text: &str) -> Option<&str> {
    text.trim()
        .strip_prefix("gh version ")?
        .split_whitespace()
        .next()
}
fn parse_codex_version(text: &str) -> Option<&str> {
    text.trim()
        .strip_prefix("codex-cli ")?
        .split_whitespace()
        .next()
}
fn parse_gh_auth(text: &str) -> Option<(&str, Option<bool>)> {
    let login = text.lines().find_map(|line| {
        line.split_once("Logged in to github.com account ")
            .and_then(|(_, rest)| rest.split_whitespace().next())
    })?;
    let scopes = text.lines().find_map(|line| {
        line.split_once("Token scopes:").map(|(_, scopes)| {
            scopes
                .split(',')
                .any(|scope| scope.trim().trim_matches('\'') == "repo")
        })
    });
    Some((login, scopes))
}
fn check_gh(env: &ChildEnv) -> CheckState {
    let text = match version_output(env, "gh", "brew install gh", "Not installed") {
        Ok(text) => text,
        Err(state) => return state,
    };
    let Some(version) = parse_gh_version(&text) else {
        return CheckState::Failed("Could not parse GitHub CLI version".into());
    };
    let numbers: Option<Vec<u64>> = version.split('.').map(|part| part.parse().ok()).collect();
    match numbers.as_deref() {
        Some([major, minor, patch]) if (*major, *minor, *patch) >= (2, 97, 0) => {
            CheckState::Ok(format!("Version {version}"))
        }
        Some([_, _, _]) => CheckState::Failed(format!(
            "Version {version}; Baby Sitter needs 2.97.0 or later"
        )),
        _ => CheckState::Failed("Could not parse GitHub CLI version".into()),
    }
}
fn check_gh_auth(env: &ChildEnv) -> CheckState {
    match command(env, "gh", &["auth", "status", "--hostname", "github.com"]) {
        Ok(output) => {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            match parse_gh_auth(&text) {
                Some((login, Some(false))) if output.status.success() => {
                    CheckState::Failed(format!("Signed in as {login}; token lacks repo scope"))
                }
                Some((login, _)) if output.status.success() => {
                    CheckState::Ok(format!("Signed in as {login}"))
                }
                _ => missing("Not signed in", "gh auth login"),
            }
        }
        Err(error) if error == "not found" => missing("Not signed in", "gh auth login"),
        Err(error) => CheckState::Failed(error),
    }
}
fn check_git(env: &ChildEnv) -> CheckState {
    match version_output(env, "git", "xcode-select --install", "Not installed") {
        Ok(text) => match text.strip_prefix("git version ") {
            Some(version) => CheckState::Ok(format!("Version {version}")),
            None => CheckState::Failed("Could not parse Git version".into()),
        },
        Err(state) => state,
    }
}
fn check_codex(env: &ChildEnv) -> CheckState {
    match version_output(env, "codex", "brew install --cask codex", "Not installed") {
        Ok(text) => match parse_codex_version(&text) {
            Some(version) => CheckState::Ok(format!("Version {version}")),
            None => CheckState::Failed("Could not parse Codex version".into()),
        },
        Err(state) => state,
    }
}
fn check_codex_auth(env: &ChildEnv) -> CheckState {
    match command(env, "codex", &["login", "status"]) {
        Ok(output) if output.status.success() => {
            let text = if output.stdout.is_empty() {
                &output.stderr
            } else {
                &output.stdout
            };
            CheckState::Ok(
                String::from_utf8_lossy(text)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_owned(),
            )
        }
        Ok(_) => missing("Not signed in", "codex login"),
        Err(error) if error == "not found" => CheckState::Failed("Install Codex first".into()),
        Err(error) => CheckState::Failed(error),
    }
}
fn optional(env: &ChildEnv, program: &str, detail: &str, fix: &str) -> CheckState {
    match version_output(env, program, fix, detail) {
        Ok(version) => CheckState::Ok(format!("Version {version}")),
        Err(state) => state,
    }
}
fn check_pnpm(env: &ChildEnv) -> CheckState {
    optional(
        env,
        "pnpm",
        "Needed for repositories with pnpm-lock.yaml",
        "npm install -g pnpm or corepack enable",
    )
}
fn check_npm(env: &ChildEnv) -> CheckState {
    optional(
        env,
        "npm",
        "Needed for repositories with package-lock.json",
        "Install Node.js",
    )
}
fn check_yarn(env: &ChildEnv) -> CheckState {
    optional(
        env,
        "yarn",
        "Needed for repositories with yarn.lock",
        "corepack enable",
    )
}

const NOTIFICATION_SETTINGS_URL: &str = "x-apple.systempreferences:com.apple.Notifications-Settings.extension?id=com.github.vikrrantshah.baby-sitter";

fn notifications_state(access: NotificationAccess) -> CheckState {
    let detail = match access {
        NotificationAccess::Unbundled => {
            return CheckState::Failed("Available only in the bundled app".into());
        }
        NotificationAccess::NotDetermined => {
            return CheckState::Missing {
                detail: "Not requested yet".into(),
                fix: Fix::AllowNotifications,
            };
        }
        NotificationAccess::Denied => "Turned off in System Settings",
        NotificationAccess::Allowed {
            banners: true,
            notification_center: true,
        } => return CheckState::Ok("Banners and Notification Center".into()),
        NotificationAccess::Allowed {
            banners: false,
            notification_center: true,
        } => "Allowed, but banners are off",
        NotificationAccess::Allowed {
            banners: true,
            notification_center: false,
        } => "Allowed, but hidden from Notification Center",
        NotificationAccess::Allowed {
            banners: false,
            notification_center: false,
        } => "Allowed, but banners and Notification Center are off",
    };
    CheckState::Missing {
        detail: detail.into(),
        fix: Fix::OpenSettings(NOTIFICATION_SETTINGS_URL),
    }
}

fn check_notifications(_: &ChildEnv) -> CheckState {
    match notifications::access() {
        Ok(access) => notifications_state(access),
        Err(error) => CheckState::Failed(error.into()),
    }
}

pub fn required_failures<'a>(
    results: &'a [(&'static CheckSpec, CheckState)],
) -> impl Iterator<Item = &'a (&'static CheckSpec, CheckState)> {
    results.iter().filter(|(spec, state)| {
        spec.section == Section::Required && !matches!(state, CheckState::Ok(_))
    })
}

pub fn run_all(env: &ChildEnv) -> Vec<(&'static CheckSpec, CheckState)> {
    let mut results = Vec::with_capacity(CHECKS.len());
    for spec in CHECKS {
        let state = if spec.id == "codex-auth"
            && results
                .iter()
                .any(|(check, state): &(&CheckSpec, CheckState)| {
                    check.id == "codex" && matches!(state, CheckState::Missing { .. })
                }) {
            CheckState::Failed("Install Codex first".into())
        } else {
            (spec.run)(env)
        };
        results.push((spec, state));
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notification_access_reports_each_status_and_action() {
        let settings = |detail: &str| CheckState::Missing {
            detail: detail.into(),
            fix: Fix::OpenSettings(
                "x-apple.systempreferences:com.apple.Notifications-Settings.extension?id=com.github.vikrrantshah.baby-sitter",
            ),
        };
        let cases = [
            (
                NotificationAccess::Allowed {
                    banners: true,
                    notification_center: true,
                },
                CheckState::Ok("Banners and Notification Center".into()),
            ),
            (
                NotificationAccess::Allowed {
                    banners: false,
                    notification_center: true,
                },
                settings("Allowed, but banners are off"),
            ),
            (
                NotificationAccess::Allowed {
                    banners: true,
                    notification_center: false,
                },
                settings("Allowed, but hidden from Notification Center"),
            ),
            (
                NotificationAccess::Allowed {
                    banners: false,
                    notification_center: false,
                },
                settings("Allowed, but banners and Notification Center are off"),
            ),
            (
                NotificationAccess::Denied,
                settings("Turned off in System Settings"),
            ),
            (
                NotificationAccess::NotDetermined,
                CheckState::Missing {
                    detail: "Not requested yet".into(),
                    fix: Fix::AllowNotifications,
                },
            ),
            (
                NotificationAccess::Unbundled,
                CheckState::Failed("Available only in the bundled app".into()),
            ),
        ];
        for (access, expected) in cases {
            assert_eq!(notifications_state(access), expected);
        }
    }

    #[test]
    fn banner_failures_exclude_optional_and_permission_checks() {
        let results = vec![
            (&CHECKS[0], CheckState::Ok("ready".into())),
            (&CHECKS[1], CheckState::Failed("required".into())),
            (&CHECKS[5], CheckState::Failed("optional".into())),
            (&CHECKS[8], CheckState::Failed("permission".into())),
        ];
        let labels: Vec<_> = required_failures(&results)
            .map(|(spec, _)| spec.label)
            .collect();
        assert_eq!(labels, ["GitHub sign-in"]);
        assert_eq!(required_failures(&results[2..]).count(), 0);
    }

    #[test]
    fn parses_real_versions_and_authentication() {
        assert_eq!(
            parse_gh_version("gh version 2.103.3 (2026-10-26)"),
            Some("2.103.3")
        );
        assert_eq!(parse_codex_version("codex-cli 0.167.1"), Some("0.167.1"));
        let auth = "github.com\n  ✓ Logged in to github.com account vikrrantshah (keyring)\n  - Active account: true\n  - Git operations protocol: ssh\n  - Token: gho_****\n  - Token scopes: 'admin:public_key', 'delete_repo', 'gist', 'read:org', 'repo'";
        assert_eq!(parse_gh_auth(auth), Some(("vikrrantshah", Some(true))));
        assert_eq!(
            parse_gh_auth(
                "You are not logged into any GitHub hosts. To log in, run: gh auth login"
            ),
            None
        );
        assert_eq!(
            parse_gh_auth(
                "Logged in to github.com account test (keyring)\nToken scopes: 'read:org'"
            ),
            Some(("test", Some(false)))
        );
    }
}
