use std::{
    fmt,
    fs::{self, File, Metadata, Permissions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

const PRIVATE_FILE_MODE: u32 = 0o600;
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;

#[derive(Clone, Copy)]
enum DestinationExpectation<'a> {
    Missing,
    Sha256(&'a str),
}

#[derive(Clone, Copy)]
enum DirectoryPolicy {
    Private,
    TrustedExisting,
}

#[derive(Clone, PartialEq, Eq)]
pub struct LoadedFile {
    pub bytes: Vec<u8>,
    pub sha256: String,
}

impl fmt::Debug for LoadedFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadedFile")
            .field(
                "bytes",
                &format_args!("[REDACTED; {} bytes]", self.bytes.len()),
            )
            .field("sha256", &self.sha256)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum SecureFileError {
    #[error("path has no parent directory: {0}")]
    MissingParent(PathBuf),

    #[error("refusing symbolic link: {0}")]
    SymbolicLink(PathBuf),

    #[error("refusing non-regular file: {0}")]
    NotRegularFile(PathBuf),

    #[error("refusing file with {links} hard links: {path}")]
    UnexpectedHardLinks { path: PathBuf, links: u64 },

    #[error("refusing file with permissions {mode:o}; expected 600: {path}")]
    InsecurePermissions { path: PathBuf, mode: u32 },

    #[error(
        "refusing external cache with permissions {mode:o}; owner read is required and group/other write or execute is forbidden: {path}"
    )]
    InsecureExternalPermissions { path: PathBuf, mode: u32 },

    #[error(
        "refusing external cache owned by uid {owner}; expected directory owner uid {expected}: {path}"
    )]
    UnexpectedOwner {
        path: PathBuf,
        owner: u32,
        expected: u32,
    },

    #[error("concurrent modification detected for {path}")]
    ConcurrentModification { path: PathBuf },

    #[error("expected an existing file at {0}")]
    MissingFile(PathBuf),

    #[error("system clock is before the Unix epoch")]
    InvalidSystemTime,

    #[error("I/O error for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to persist temporary file to {path}: {source}")]
    Persist {
        path: PathBuf,
        #[source]
        source: tempfile::PersistError,
    },
}

pub(crate) fn load(path: &Path) -> Result<LoadedFile, SecureFileError> {
    let metadata = secure_metadata(path)?;
    validate_regular_file(path, &metadata)?;

    read_loaded_file(path)
}

/// Loads a Codex-owned derived cache from a trusted directory.
///
/// Codex currently writes `models_cache.json` as 0644, so it cannot use provider-x's private
/// 0600 file policy. The cache contains model metadata rather than credentials. It must still be a
/// single-link regular file owned by the directory owner, readable by the owner, and not writable
/// or executable by group/other users.
pub(crate) fn load_external_cache(path: &Path) -> Result<LoadedFile, SecureFileError> {
    let metadata = secure_metadata(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| SecureFileError::MissingParent(path.to_path_buf()))?;
    ensure_trusted_existing_directory(parent)?;
    let parent_metadata = secure_metadata(parent)?;
    validate_external_cache_file(path, &metadata, parent_metadata.uid())?;

    read_loaded_file(path)
}

fn read_loaded_file(path: &Path) -> Result<LoadedFile, SecureFileError> {
    let mut file = File::open(path).map_err(|source| io_error(path, source))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|source| io_error(path, source))?;
    Ok(LoadedFile {
        sha256: sha256(&bytes),
        bytes,
    })
}

pub(crate) fn write(
    path: &Path,
    expected_sha256: Option<&str>,
    bytes: &[u8],
) -> Result<LoadedFile, SecureFileError> {
    write_with_policy(path, expected_sha256, bytes, DirectoryPolicy::Private, true)
}

pub(crate) fn write_without_backup(
    path: &Path,
    expected_sha256: Option<&str>,
    bytes: &[u8],
) -> Result<LoadedFile, SecureFileError> {
    write_with_policy(
        path,
        expected_sha256,
        bytes,
        DirectoryPolicy::Private,
        false,
    )
}

pub(crate) fn write_external(
    path: &Path,
    expected_sha256: Option<&str>,
    bytes: &[u8],
) -> Result<LoadedFile, SecureFileError> {
    write_with_policy(
        path,
        expected_sha256,
        bytes,
        DirectoryPolicy::TrustedExisting,
        false,
    )
}

pub(crate) fn remove_external(path: &Path, expected_sha256: &str) -> Result<(), SecureFileError> {
    let parent = path
        .parent()
        .ok_or_else(|| SecureFileError::MissingParent(path.to_path_buf()))?;
    ensure_trusted_existing_directory(parent)?;
    let current = load(path)?;
    if current.sha256 != expected_sha256 {
        return Err(SecureFileError::ConcurrentModification {
            path: path.to_path_buf(),
        });
    }
    fs::remove_file(path).map_err(|source| io_error(path, source))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(parent, source))
}

pub(crate) fn remove_external_cache(
    path: &Path,
    expected_sha256: &str,
) -> Result<(), SecureFileError> {
    let parent = path
        .parent()
        .ok_or_else(|| SecureFileError::MissingParent(path.to_path_buf()))?;
    ensure_trusted_existing_directory(parent)?;
    let current = load_external_cache(path)?;
    if current.sha256 != expected_sha256 {
        return Err(SecureFileError::ConcurrentModification {
            path: path.to_path_buf(),
        });
    }
    fs::remove_file(path).map_err(|source| io_error(path, source))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(parent, source))
}

fn write_with_policy(
    path: &Path,
    expected_sha256: Option<&str>,
    bytes: &[u8],
    directory_policy: DirectoryPolicy,
    backup: bool,
) -> Result<LoadedFile, SecureFileError> {
    let parent = path
        .parent()
        .ok_or_else(|| SecureFileError::MissingParent(path.to_path_buf()))?;
    match directory_policy {
        DirectoryPolicy::Private => ensure_private_directory(parent)?,
        DirectoryPolicy::TrustedExisting => ensure_trusted_existing_directory(parent)?,
    }

    let current = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_regular_file(path, &metadata)?;
            Some(load(path)?)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(source) => return Err(io_error(path, source)),
    };

    match (&current, expected_sha256) {
        (Some(current), Some(expected)) if current.sha256 == expected => {}
        (None, None) => {}
        _ => {
            return Err(SecureFileError::ConcurrentModification {
                path: path.to_path_buf(),
            });
        }
    }

    if backup && let Some(current) = &current {
        write_backup(path, current)?;
    }

    let destination_expectation = current
        .as_ref()
        .map_or(DestinationExpectation::Missing, |current| {
            DestinationExpectation::Sha256(&current.sha256)
        });
    persist_private(path, bytes, destination_expectation)?;
    load(path)
}

fn ensure_trusted_existing_directory(path: &Path) -> Result<(), SecureFileError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| io_error(path, source))?;
    if metadata.file_type().is_symlink() {
        return Err(SecureFileError::SymbolicLink(path.to_path_buf()));
    }
    if !metadata.file_type().is_dir() {
        return Err(SecureFileError::NotRegularFile(path.to_path_buf()));
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o022 != 0 {
        return Err(SecureFileError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

fn persist_private(
    path: &Path,
    bytes: &[u8],
    expectation: DestinationExpectation<'_>,
) -> Result<(), SecureFileError> {
    let parent = path
        .parent()
        .ok_or_else(|| SecureFileError::MissingParent(path.to_path_buf()))?;
    let mut temporary = NamedTempFile::new_in(parent).map_err(|source| io_error(parent, source))?;
    temporary
        .as_file()
        .set_permissions(Permissions::from_mode(PRIVATE_FILE_MODE))
        .map_err(|source| io_error(temporary.path(), source))?;
    temporary
        .write_all(bytes)
        .map_err(|source| io_error(temporary.path(), source))?;
    temporary
        .flush()
        .map_err(|source| io_error(temporary.path(), source))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|source| io_error(temporary.path(), source))?;

    // Recheck immediately before rename. The hash check coordinates all provider-x writers and
    // narrows the remaining race window with non-cooperating processes.
    match expectation {
        DestinationExpectation::Missing => match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) => {
                return Err(SecureFileError::ConcurrentModification {
                    path: path.to_path_buf(),
                });
            }
            Err(source) => return Err(io_error(path, source)),
        },
        DestinationExpectation::Sha256(expected) => {
            let current = load(path)?;
            if current.sha256 != expected {
                return Err(SecureFileError::ConcurrentModification {
                    path: path.to_path_buf(),
                });
            }
        }
    }

    temporary
        .persist(path)
        .map_err(|source| SecureFileError::Persist {
            path: path.to_path_buf(),
            source,
        })?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| io_error(parent, source))?;
    Ok(())
}

fn write_backup(path: &Path, current: &LoadedFile) -> Result<(), SecureFileError> {
    let parent = path
        .parent()
        .ok_or_else(|| SecureFileError::MissingParent(path.to_path_buf()))?;
    let backup_directory = parent.join("backups");
    ensure_private_directory(&backup_directory)?;

    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| SecureFileError::InvalidSystemTime)?
        .as_nanos();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("provider-x");
    let backup_path = backup_directory.join(format!(
        "{file_name}.{timestamp}.sha256-{}.bak",
        &current.sha256[..16]
    ));
    persist_private(
        &backup_path,
        &current.bytes,
        DestinationExpectation::Missing,
    )
}

pub(crate) fn ensure_private_directory(path: &Path) -> Result<(), SecureFileError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(SecureFileError::SymbolicLink(path.to_path_buf()));
            }
            if !metadata.file_type().is_dir() {
                return Err(SecureFileError::NotRegularFile(path.to_path_buf()));
            }
            let mode = metadata.mode() & 0o777;
            if mode != PRIVATE_DIRECTORY_MODE {
                return Err(SecureFileError::InsecurePermissions {
                    path: path.to_path_buf(),
                    mode,
                });
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path).map_err(|source| io_error(path, source))?;
            fs::set_permissions(path, Permissions::from_mode(PRIVATE_DIRECTORY_MODE))
                .map_err(|source| io_error(path, source))
        }
        Err(source) => Err(io_error(path, source)),
    }
}

fn secure_metadata(path: &Path) -> Result<Metadata, SecureFileError> {
    fs::symlink_metadata(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            SecureFileError::MissingFile(path.to_path_buf())
        } else {
            io_error(path, source)
        }
    })
}

pub(crate) fn validate_regular_file(
    path: &Path,
    metadata: &Metadata,
) -> Result<(), SecureFileError> {
    if metadata.file_type().is_symlink() {
        return Err(SecureFileError::SymbolicLink(path.to_path_buf()));
    }
    if !metadata.file_type().is_file() {
        return Err(SecureFileError::NotRegularFile(path.to_path_buf()));
    }
    if metadata.nlink() != 1 {
        return Err(SecureFileError::UnexpectedHardLinks {
            path: path.to_path_buf(),
            links: metadata.nlink(),
        });
    }
    let mode = metadata.mode() & 0o777;
    if mode != PRIVATE_FILE_MODE {
        return Err(SecureFileError::InsecurePermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

fn validate_external_cache_file(
    path: &Path,
    metadata: &Metadata,
    expected_owner: u32,
) -> Result<(), SecureFileError> {
    if metadata.file_type().is_symlink() {
        return Err(SecureFileError::SymbolicLink(path.to_path_buf()));
    }
    if !metadata.file_type().is_file() {
        return Err(SecureFileError::NotRegularFile(path.to_path_buf()));
    }
    if metadata.nlink() != 1 {
        return Err(SecureFileError::UnexpectedHardLinks {
            path: path.to_path_buf(),
            links: metadata.nlink(),
        });
    }
    if metadata.uid() != expected_owner {
        return Err(SecureFileError::UnexpectedOwner {
            path: path.to_path_buf(),
            owner: metadata.uid(),
            expected: expected_owner,
        });
    }
    let mode = metadata.mode() & 0o777;
    if mode & 0o400 == 0 || mode & 0o033 != 0 {
        return Err(SecureFileError::InsecureExternalPermissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn io_error(path: &Path, source: std::io::Error) -> SecureFileError {
    SecureFileError::Io {
        path: path.to_path_buf(),
        source,
    }
}
