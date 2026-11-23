use crate::model::{Config, Defaults, Prompts};
use anyhow::{Context, Result, bail};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

pub fn data_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/baby-sitter"))
}

pub fn load(dir: &Path) -> Result<Config> {
    let path = dir.join("config.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let raw: serde_json::Value =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    let legacy = raw.get("app").is_none();
    let mut config: Config =
        serde_json::from_value(raw).with_context(|| format!("parse {}", path.display()))?;
    if legacy {
        let defaults = Defaults::default();
        for settings in config.repos.values_mut() {
            if settings.timeout == Some(defaults.timeout) {
                settings.timeout = None;
            }
            if settings.draft_timeout == Some(defaults.draft_timeout) {
                settings.draft_timeout = None;
            }
            if settings.interval == Some(defaults.interval) {
                settings.interval = None;
            }
        }
    }
    if config.app.defaults.interval.is_zero()
        || config
            .repos
            .values()
            .any(|settings| settings.interval.is_some_and(|interval| interval.is_zero()))
    {
        bail!("{} contains a zero interval", path.display());
    }
    Ok(config)
}

pub fn save(dir: &Path, config: &Config) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join("config.json");
    let tmp = dir.join(format!(".config-{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    serde_json::to_writer_pretty(&mut file, config)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&tmp, &path).with_context(|| format!("save {}", path.display()))?;
    Ok(())
}

pub fn write_prompts(dir: &Path, prompts: &Prompts) -> Result<()> {
    let path = dir.join("prompts.json");
    if prompts.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error).with_context(|| format!("remove {}", path.display())),
        }
    }
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let tmp = dir.join(format!(".prompts-{}.tmp", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp)?;
    file.write_all(prompts.render_json()?.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&tmp, &path).with_context(|| format!("save {}", path.display()))?;
    Ok(())
}

pub fn lock_instance(dir: &Path) -> Result<Option<File>> {
    use std::os::fd::AsRawFd;
    fs::create_dir_all(dir)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("app.lock"))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock {
        Ok(None)
    } else {
        Err(error.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PrRef, RepoName, RepoSettings};

    #[test]
    fn prompt_file_tracks_overrides() {
        let dir = crate::test_support::unique_test_dir("app-prompts");
        let prompts = Prompts {
            review: crate::model::CustomPrompt {
                enabled: true,
                text: Some("review this".into()),
            },
            ..Prompts::default()
        };
        write_prompts(&dir, &prompts).unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.join("prompts.json")).unwrap()).unwrap();
        assert_eq!(value, serde_json::json!({"review": "review this"}));
        write_prompts(&dir, &Prompts::default()).unwrap();
        assert!(!dir.join("prompts.json").exists());
        write_prompts(&dir, &Prompts::default()).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn older_config_has_no_prompt_overrides() {
        let config: Config = serde_json::from_str(r#"{"app": {}, "repos": {}}"#).unwrap();
        assert_eq!(config.prompts, Prompts::default());
    }

    #[test]
    fn legacy_defaults_become_inherited_and_custom_values_are_preserved() {
        let dir = crate::test_support::unique_test_dir("app-legacy-settings");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("config.json"),
            r#"{
                "repos": {
                    "arbiogroup/arbio-platform": {
                        "timeout": "2h",
                        "draft_timeout": "2h",
                        "interval": "30s"
                    },
                    "owner/custom": {
                        "timeout": "1h",
                        "draft_timeout": "2h",
                        "interval": "45s"
                    }
                }
            }"#,
        )
        .unwrap();
        let config = load(&dir).unwrap();
        assert_eq!(
            config.repos[&"arbiogroup/arbio-platform".parse::<RepoName>().unwrap()],
            RepoSettings::default()
        );
        let custom = &config.repos[&"owner/custom".parse::<RepoName>().unwrap()];
        assert_eq!(custom.timeout, Some(std::time::Duration::from_secs(3600)));
        assert_eq!(custom.draft_timeout, None);
        assert_eq!(custom.interval, Some(std::time::Duration::from_secs(45)));
        save(&dir, &config).unwrap();
        assert_eq!(load(&dir).unwrap(), config);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_defaults_remain_overrides_after_app_settings_exist() {
        let dir = crate::test_support::unique_test_dir("app-settings-overrides");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("config.json"),
            r#"{"app": {}, "repos": {"owner/repo": {
                "timeout": "2h", "draft_timeout": "2h", "interval": "30s"
            }}}"#,
        )
        .unwrap();
        let config = load(&dir).unwrap();
        let settings = &config.repos[&"owner/repo".parse::<RepoName>().unwrap()];
        assert_eq!(settings.timeout, Some(std::time::Duration::from_secs(7200)));
        assert_eq!(
            settings.draft_timeout,
            Some(std::time::Duration::from_secs(7200))
        );
        assert_eq!(settings.interval, Some(std::time::Duration::from_secs(30)));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn zero_intervals_are_rejected_in_defaults_and_overrides() {
        let dir = crate::test_support::unique_test_dir("app-zero-interval");
        fs::create_dir_all(&dir).unwrap();
        for json in [
            r#"{"app": {"defaults": {"interval": "0s"}}}"#,
            r#"{"app": {}, "repos": {"owner/repo": {"interval": "0s"}}}"#,
        ] {
            fs::write(dir.join("config.json"), json).unwrap();
            assert!(
                load(&dir)
                    .unwrap_err()
                    .to_string()
                    .contains("zero interval")
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn configuration_persists_and_instance_lock_is_exclusive() {
        let dir = crate::test_support::unique_test_dir("app-store");
        let first = lock_instance(&dir).unwrap().unwrap();
        assert!(lock_instance(&dir).unwrap().is_none());
        let repo: RepoName = "owner/repo".parse().unwrap();
        let pr = PrRef {
            repo: repo.clone(),
            number: 9,
        };
        let mut config = Config::default();
        config.repos.insert(repo, RepoSettings::default());
        config.active.insert(pr, 1_700_000_000);
        save(&dir, &config).unwrap();
        assert_eq!(load(&dir).unwrap(), config);
        drop(first);
        let next = lock_instance(&dir).unwrap().unwrap();
        drop(next);
        fs::remove_dir_all(dir).unwrap();
    }
}
