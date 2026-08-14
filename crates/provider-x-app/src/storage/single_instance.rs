use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    time::{Duration, Instant},
};

use fs2::FileExt;
use thiserror::Error;

use super::atomic_file::{self, SecureFileError};

#[derive(Debug, Error)]
pub enum SingleInstanceError {
    #[error(transparent)]
    File(#[from] SecureFileError),

    #[error("another provider-x instance already owns the application lock")]
    AlreadyRunning,

    #[error("failed to open application lock {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to inspect application lock {path}: {source}")]
    Metadata {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug)]
pub struct SingleInstanceGuard {
    file: File,
}

impl SingleInstanceGuard {
    /// Acquires a process-lifetime advisory lock in the provider-x private state directory.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe storage, I/O failure, or an already-running instance.
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Self, SingleInstanceError> {
        let path = path.into();
        let parent = path
            .parent()
            .ok_or_else(|| SecureFileError::MissingParent(path.clone()))?;
        atomic_file::ensure_private_directory(parent)?;
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            atomic_file::validate_regular_file(&path, &metadata)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|source| SingleInstanceError::Open {
                path: path.clone(),
                source,
            })?;
        let metadata = file
            .metadata()
            .map_err(|source| SingleInstanceError::Metadata {
                path: path.clone(),
                source,
            })?;
        atomic_file::validate_regular_file(&path, &metadata)?;
        file.try_lock_exclusive()
            .map_err(|_| SingleInstanceError::AlreadyRunning)?;
        Ok(Self { file })
    }

    /// Waits for a previous instance to finish its graceful shutdown before acquiring the lock.
    ///
    /// # Errors
    ///
    /// Returns [`SingleInstanceError::AlreadyRunning`] when the timeout expires, or forwards
    /// storage and I/O errors without retrying them.
    pub fn acquire_with_timeout(
        path: impl Into<PathBuf>,
        timeout: Duration,
    ) -> Result<Self, SingleInstanceError> {
        let path = path.into();
        let deadline = Instant::now() + timeout;
        loop {
            match Self::acquire(&path) {
                Ok(guard) => return Ok(guard),
                Err(SingleInstanceError::AlreadyRunning) if Instant::now() < deadline => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    std::thread::sleep(remaining.min(Duration::from_millis(100)));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

impl Drop for SingleInstanceGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}
