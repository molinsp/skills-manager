//! Per-repo write lock for synced repos.
//!
//! Modeled on `repo_lock`, but one lock file per synced repo and separate from
//! the Backup repo's lock, so refreshing a shared repo never blocks (or is
//! blocked by) a Backup push. The lock file lives in `synced_repos/.locks/`,
//! outside every repo directory, so a repo dir can be renamed or removed while
//! the lock is held (Windows refuses to remove a directory containing a locked
//! file).

use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::time::{Duration, Instant};

use super::central_repo;

/// Foreground operations wait this long for a background refresh to finish.
pub const FOREGROUND_WAIT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct SyncedRepoLock {
    file: File,
}

impl SyncedRepoLock {
    /// Acquire, failing immediately when held. For background rounds.
    pub fn acquire(slug: &str, operation: &str) -> Result<Self> {
        let file = open_lock_file(slug)?;
        file.try_lock_exclusive()
            .with_context(|| format!("synced repo {slug} is busy: {operation}"))?;
        Self::stamp(file, operation)
    }

    pub fn acquire_blocking(slug: &str, operation: &str, timeout: Duration) -> Result<Self> {
        let file = open_lock_file(slug)?;
        let start = Instant::now();
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => return Self::stamp(file, operation),
                Err(err) => {
                    if start.elapsed() >= timeout {
                        return Err(err)
                            .with_context(|| format!("synced repo {slug} is busy: {operation}"));
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        }
    }

    pub fn acquire_foreground(slug: &str, operation: &str) -> Result<Self> {
        Self::acquire_blocking(slug, operation, FOREGROUND_WAIT)
    }

    fn stamp(mut file: File, operation: &str) -> Result<Self> {
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        writeln!(
            file,
            "pid={}\noperation={}\nstart_time={}",
            std::process::id(),
            operation,
            chrono::Utc::now().to_rfc3339()
        )?;
        Ok(Self { file })
    }
}

fn open_lock_file(slug: &str) -> Result<File> {
    let dir = central_repo::synced_repos_dir().join(".locks");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{slug}.lock"));
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("failed to open synced repo lock {}", path.display()))
}

impl Drop for SyncedRepoLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locks_are_per_repo_and_independent_of_backup_lock() {
        let _guard = central_repo::test_base_dir_lock();
        let tmp = tempfile::tempdir().unwrap();
        central_repo::set_test_base_dir_override(Some(tmp.path().join("base")));

        let a = SyncedRepoLock::acquire("a", "test").unwrap();
        assert!(SyncedRepoLock::acquire("a", "again").is_err());
        let b = SyncedRepoLock::acquire("b", "test").unwrap();
        let backup = super::super::repo_lock::RepoLock::acquire("backup").unwrap();
        drop((a, b, backup));
        assert!(SyncedRepoLock::acquire("a", "after release").is_ok());

        central_repo::set_test_base_dir_override(None);
    }
}
