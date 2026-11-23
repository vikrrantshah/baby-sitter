use crate::model::Config;
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
    let config: Config =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
    if config
        .repos
        .values()
        .any(|settings| settings.interval.is_zero())
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
