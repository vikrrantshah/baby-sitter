use crate::proc;
use anyhow::Context;
use std::{
    collections::hash_map::DefaultHasher,
    fs,
    hash::{Hash, Hasher},
    io::ErrorKind,
    path::Path,
    process::Command,
    time::Instant,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Installer {
    Pnpm,
    Npm,
    Yarn,
}

struct InstallerSpec {
    lockfile: &'static str,
    installer: Installer,
    program: &'static str,
    args: &'static [&'static str],
}

const INSTALLERS: &[InstallerSpec] = &[
    InstallerSpec {
        lockfile: "pnpm-lock.yaml",
        installer: Installer::Pnpm,
        program: "pnpm",
        args: &["install", "--frozen-lockfile", "--prefer-offline"],
    },
    InstallerSpec {
        lockfile: "package-lock.json",
        installer: Installer::Npm,
        program: "npm",
        args: &["ci", "--prefer-offline", "--no-audit", "--no-fund"],
    },
    InstallerSpec {
        lockfile: "yarn.lock",
        installer: Installer::Yarn,
        program: "yarn",
        args: &["install", "--frozen-lockfile"],
    },
];

impl Installer {
    pub fn name(self) -> &'static str {
        INSTALLERS
            .iter()
            .find(|spec| spec.installer == self)
            .expect("each installer has a specification")
            .program
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Prepared {
    Installed(Installer),
    NotNeeded,
    Failed(String),
}

fn detect(root: &Path) -> std::io::Result<Option<&'static InstallerSpec>> {
    for spec in INSTALLERS {
        if root.join(spec.lockfile).try_exists()? {
            return Ok(Some(spec));
        }
    }
    Ok(None)
}

pub fn prepare(root: &Path, state_dir: &Path, deadline: Instant) -> anyhow::Result<Prepared> {
    match prepare_inner(root, state_dir, deadline) {
        Err(error) if error.downcast_ref::<proc::TimedOut>().is_some() => Err(error),
        Err(error) => Ok(Prepared::Failed(failure_reason(&format!("{error:#}")))),
        result => result,
    }
}

fn prepare_inner(root: &Path, state_dir: &Path, deadline: Instant) -> anyhow::Result<Prepared> {
    let Some(spec) = detect(root).context("detecting dependency lockfile")? else {
        return Ok(Prepared::NotNeeded);
    };
    let lockfile = fs::read(root.join(spec.lockfile)).context("reading dependency lockfile")?;
    let mut hasher = DefaultHasher::new();
    spec.program.hash(&mut hasher);
    lockfile.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    let marker = state_dir.join("deps.lock-hash");
    if fs::read_to_string(&marker).is_ok_and(|previous| previous == hash)
        && root
            .join("node_modules")
            .try_exists()
            .context("checking node_modules")?
    {
        return Ok(Prepared::Installed(spec.installer));
    }

    fs::create_dir_all(state_dir).context("creating dependency state directory")?;
    match fs::remove_file(&marker) {
        Ok(()) => (),
        Err(error) if error.kind() == ErrorKind::NotFound => (),
        Err(error) => return Err(error).context("removing stale dependency hash"),
    }

    let mut command = Command::new(spec.program);
    command.args(spec.args).current_dir(root);
    if let Err(error) = proc::run(&mut command, None, deadline) {
        if error.downcast_ref::<proc::TimedOut>().is_some() {
            return Err(error);
        }
        let text = format!("{error:#}");
        let suffix = format!(" ({})", proc::command_label(&command));
        let reason = text.strip_suffix(&suffix).unwrap_or(&text);
        return Ok(Prepared::Failed(failure_reason(reason)));
    }
    fs::write(&marker, hash).context("writing dependency hash")?;
    Ok(Prepared::Installed(spec.installer))
}

fn failure_reason(text: &str) -> String {
    let text = text.trim();
    let start = text
        .char_indices()
        .rev()
        .nth(499)
        .map_or(0, |(index, _)| index);
    let tail = text[start..].trim();
    if tail.is_empty() {
        "installer exited unsuccessfully".to_owned()
    } else {
        tail.to_owned()
    }
}

pub fn codex_note(prepared: &Prepared) -> String {
    match prepared {
        Prepared::Installed(_) => "Dependencies are installed. Before you answer, run the narrowest type checks, linters, and tests that cover the files you changed, and fix what they report. Do not install or add packages; there is no network.".to_owned(),
        Prepared::NotNeeded => "Before you answer, run the narrowest checks that cover the files you changed if the project's tooling is available. There is no network.".to_owned(),
        Prepared::Failed(reason) => format!("Dependencies could not be installed ({reason}). Do not try to install them. Edit carefully and say in your summary that checks were not run."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::PathBuf, time::Duration};

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path = crate::test_support::unique_test_dir("baby-sitter-deps");
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn no_lockfile_needs_no_dependencies() {
        let dir = TestDir::new();
        assert_eq!(
            prepare(
                &dir.0,
                &dir.0.join("state"),
                Instant::now() + Duration::from_secs(5)
            )
            .unwrap(),
            Prepared::NotNeeded
        );
        assert!(!dir.0.join("state").exists());
    }

    #[test]
    fn unreadable_lockfile_is_reported_as_a_failure() {
        let dir = TestDir::new();
        fs::create_dir(dir.0.join("pnpm-lock.yaml")).unwrap();
        let result = prepare(
            &dir.0,
            &dir.0.join("state"),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert!(
            matches!(result, Prepared::Failed(reason) if reason.contains("reading dependency lockfile"))
        );
    }

    #[test]
    fn lockfile_detection_prefers_pnpm_then_npm_then_yarn() {
        let dir = TestDir::new();
        fs::write(dir.0.join("yarn.lock"), "yarn").unwrap();
        let yarn = detect(&dir.0).unwrap().unwrap();
        assert_eq!(
            (yarn.installer, yarn.program, yarn.args),
            (
                Installer::Yarn,
                "yarn",
                &["install", "--frozen-lockfile"][..]
            )
        );

        fs::write(dir.0.join("package-lock.json"), "npm").unwrap();
        let npm = detect(&dir.0).unwrap().unwrap();
        assert_eq!(
            (npm.installer, npm.program, npm.args),
            (
                Installer::Npm,
                "npm",
                &["ci", "--prefer-offline", "--no-audit", "--no-fund"][..]
            )
        );

        fs::write(dir.0.join("pnpm-lock.yaml"), "pnpm").unwrap();
        let pnpm = detect(&dir.0).unwrap().unwrap();
        assert_eq!(
            (pnpm.installer, pnpm.program, pnpm.args),
            (
                Installer::Pnpm,
                "pnpm",
                &["install", "--frozen-lockfile", "--prefer-offline"][..]
            )
        );
    }

    #[test]
    fn codex_notes_describe_each_preparation_outcome() {
        assert_eq!(
            codex_note(&Prepared::Installed(Installer::Pnpm)),
            "Dependencies are installed. Before you answer, run the narrowest type checks, linters, and tests that cover the files you changed, and fix what they report. Do not install or add packages; there is no network."
        );
        assert_eq!(
            codex_note(&Prepared::NotNeeded),
            "Before you answer, run the narrowest checks that cover the files you changed if the project's tooling is available. There is no network."
        );
        assert_eq!(
            codex_note(&Prepared::Failed("network unavailable".into())),
            "Dependencies could not be installed (network unavailable). Do not try to install them. Edit carefully and say in your summary that checks were not run."
        );
    }

    #[cfg(unix)]
    #[test]
    fn installer_runs_only_when_dependencies_need_refreshing() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TestDir::new();
        let bin = dir.0.join("bin");
        fs::create_dir(&bin).unwrap();
        for program in ["pnpm", "npm", "yarn"] {
            let path = bin.join(program);
            fs::write(&path, "#!/bin/sh\nprintf '%s %s\\n' \"${0##*/}\" \"$*\" >> calls\nif [ -f fail ]; then /bin/cat failure.stderr >&2; exit 1; fi\nif [ -f slow ]; then exec /bin/sleep 5; fi\n/bin/mkdir -p node_modules\n").unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "deps::tests::exercise_installer_in_child",
                "--nocapture",
            ])
            .env("BABY_SITTER_DEPS_TEST_ROOT", &dir.0)
            .env("PATH", &bin)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[cfg(unix)]
    #[test]
    fn exercise_installer_in_child() {
        let Some(root) = std::env::var_os("BABY_SITTER_DEPS_TEST_ROOT").map(PathBuf::from) else {
            return;
        };
        let state = root.join("state");
        let lockfile = root.join("pnpm-lock.yaml");
        let run = || prepare(&root, &state, Instant::now() + Duration::from_secs(10));
        let calls = || fs::read_to_string(root.join("calls")).unwrap();

        fs::write(&lockfile, "version one").unwrap();
        assert_eq!(run().unwrap(), Prepared::Installed(Installer::Pnpm));
        assert_eq!(calls(), "pnpm install --frozen-lockfile --prefer-offline\n");
        assert_eq!(run().unwrap(), Prepared::Installed(Installer::Pnpm));
        assert_eq!(calls().lines().count(), 1);

        fs::write(&lockfile, "version two").unwrap();
        assert_eq!(run().unwrap(), Prepared::Installed(Installer::Pnpm));
        assert_eq!(calls().lines().count(), 2);
        fs::remove_dir_all(root.join("node_modules")).unwrap();
        assert_eq!(run().unwrap(), Prepared::Installed(Installer::Pnpm));
        assert_eq!(calls().lines().count(), 3);

        fs::write(&lockfile, "version one").unwrap();
        fs::write(root.join("fail"), "").unwrap();
        fs::write(
            root.join("failure.stderr"),
            format!("{}\nlast installer error\n", "é".repeat(700)),
        )
        .unwrap();
        let Prepared::Failed(reason) = run().unwrap() else {
            panic!("expected installer failure")
        };
        assert!(reason.ends_with("last installer error"));
        assert!(reason.chars().count() <= 500);
        assert!(!state.join("deps.lock-hash").exists());
        fs::remove_file(root.join("fail")).unwrap();
        fs::write(&lockfile, "version two").unwrap();
        assert_eq!(run().unwrap(), Prepared::Installed(Installer::Pnpm));
        assert_eq!(calls().lines().count(), 5);

        fs::write(root.join("package-lock.json"), "version two").unwrap();
        fs::remove_file(&lockfile).unwrap();
        assert_eq!(run().unwrap(), Prepared::Installed(Installer::Npm));
        assert!(calls().ends_with("npm ci --prefer-offline --no-audit --no-fund\n"));

        fs::write(root.join("package-lock.json"), "version three").unwrap();
        fs::write(root.join("slow"), "").unwrap();
        let error =
            prepare(&root, &state, Instant::now() + Duration::from_millis(200)).unwrap_err();
        assert!(error.downcast_ref::<proc::TimedOut>().is_some());
        assert!(!state.join("deps.lock-hash").exists());

        fs::remove_file(root.join("slow")).unwrap();
        fs::remove_file(root.join("bin/npm")).unwrap();
        let Prepared::Failed(reason) = run().unwrap() else {
            panic!("expected a missing installer failure");
        };
        assert!(reason.contains(&std::io::Error::from_raw_os_error(2).to_string()));
        assert!(!state.join("deps.lock-hash").exists());
    }
}
