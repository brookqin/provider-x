use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{LoadedFile, ProviderConfigStoreError, SecureFileError, atomic_file};
use crate::control_plane::{AppPaths, default_providers, empty_cache};

const RECEIPT: &str = "provider-upgrade.json";

#[derive(Debug, Error)]
pub(crate) enum UpgradeError {
    #[error(transparent)]
    File(#[from] SecureFileError),
    #[error(transparent)]
    Provider(#[from] ProviderConfigStoreError),
    #[error("could not prepare provider configuration backup")]
    Backup,
    #[error("invalid provider upgrade receipt")]
    Receipt,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    directory: String,
    providers_sha256: String,
    cache_sha256: Option<String>,
    complete: bool,
}

#[derive(Clone)]
pub(crate) struct UpgradeNotice {
    pub(crate) backup: PathBuf,
    receipt: PathBuf,
    sha256: String,
}

impl UpgradeNotice {
    pub(crate) fn acknowledge(&self) -> Result<(), UpgradeError> {
        atomic_file::ensure_private_directory(self.receipt.parent().ok_or(UpgradeError::Receipt)?)?;
        atomic_file::remove_external(&self.receipt, &self.sha256)?;
        Ok(())
    }
}

fn load_optional(path: &Path) -> Result<Option<LoadedFile>, SecureFileError> {
    match atomic_file::load(path) {
        Ok(file) => Ok(Some(file)),
        Err(SecureFileError::MissingFile(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Called under the process lock, before loading or publishing any runtime configuration.
pub(crate) fn prepare(paths: &AppPaths) -> Result<Option<UpgradeNotice>, UpgradeError> {
    atomic_file::ensure_private_directory(&paths.root)?;
    let receipt_path = paths.root.join(RECEIPT);
    let loaded = if let Some(loaded) = load_optional(&receipt_path)? {
        loaded
    } else {
        let Some(providers) = load_optional(&paths.providers)? else {
            return Ok(None);
        };
        let yaml = std::str::from_utf8(&providers.bytes)
            .map_err(|_| ProviderConfigStoreError::InvalidDocument("invalid UTF-8".to_owned()))?;
        match provider_x_core::ProvidersDocument::from_yaml(yaml) {
            Err(provider_x_core::CoreError::UnsupportedSchemaVersion { actual, .. })
                if actual > 0 => {}
            Ok(_) => return Ok(None),
            Err(error) => {
                return Err(ProviderConfigStoreError::InvalidDocument(error.to_string()).into());
            }
        }
        begin(paths, &providers)?
    };
    finish(paths, &loaded).map(Some)
}

fn begin(paths: &AppPaths, providers: &LoadedFile) -> Result<LoadedFile, UpgradeError> {
    // Validate both sources before any mutation; unsafe storage must never be "repaired".
    atomic_file::ensure_private_directory(paths.model_cache.parent().ok_or(UpgradeError::Backup)?)?;
    let cache = load_optional(&paths.model_cache)?;
    let directory = tempfile::Builder::new()
        .prefix("upgrade-backup-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(&paths.root)
        .map_err(|_| UpgradeError::Backup)?
        .keep();
    atomic_file::ensure_private_directory(&directory)?;
    atomic_file::write_without_backup(&directory.join("providers.yaml"), None, &providers.bytes)?;
    if let Some(cache) = &cache {
        atomic_file::write_without_backup(&directory.join("models.yaml"), None, &cache.bytes)?;
    }
    let receipt = Receipt {
        directory: directory
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or(UpgradeError::Backup)?
            .to_owned(),
        providers_sha256: providers.sha256.clone(),
        cache_sha256: cache.map(|file| file.sha256),
        complete: false,
    };
    // Durable backups and receipt precede both replacements, so interruption is recoverable.
    Ok(atomic_file::write_without_backup(
        &paths.root.join(RECEIPT),
        None,
        &serde_json::to_vec(&receipt).map_err(|_| UpgradeError::Receipt)?,
    )?)
}

fn replacement_hash(
    path: &Path,
    original: Option<&str>,
    replacement: &[u8],
) -> Result<Option<String>, UpgradeError> {
    let current = load_optional(path)?;
    if current.as_ref().map(|file| file.sha256.as_str()) == original
        || current
            .as_ref()
            .is_some_and(|file| file.bytes == replacement)
    {
        Ok(current.map(|file| file.sha256))
    } else {
        Err(SecureFileError::ConcurrentModification {
            path: path.to_path_buf(),
        }
        .into())
    }
}

fn finish(paths: &AppPaths, loaded: &LoadedFile) -> Result<UpgradeNotice, UpgradeError> {
    let mut receipt: Receipt =
        serde_json::from_slice(&loaded.bytes).map_err(|_| UpgradeError::Receipt)?;
    let suffix = receipt
        .directory
        .strip_prefix("upgrade-backup-")
        .ok_or(UpgradeError::Receipt)?;
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err(UpgradeError::Receipt);
    }
    let backup = paths.root.join(&receipt.directory);
    atomic_file::ensure_private_directory(&backup)?;
    if atomic_file::load(&backup.join("providers.yaml"))?.sha256 != receipt.providers_sha256 {
        return Err(UpgradeError::Receipt);
    }
    if let Some(expected) = &receipt.cache_sha256
        && atomic_file::load(&backup.join("models.yaml"))?.sha256 != *expected
    {
        return Err(UpgradeError::Receipt);
    }
    let receipt_path = paths.root.join(RECEIPT);
    let mut sha256 = loaded.sha256.clone();
    if !receipt.complete {
        let providers =
            yaml_serde::to_string(&default_providers()).map_err(|_| UpgradeError::Backup)?;
        let cache = yaml_serde::to_string(&empty_cache()).map_err(|_| UpgradeError::Backup)?;
        let provider_hash = replacement_hash(
            &paths.providers,
            Some(&receipt.providers_sha256),
            providers.as_bytes(),
        )?;
        let cache_hash = replacement_hash(
            &paths.model_cache,
            receipt.cache_sha256.as_deref(),
            cache.as_bytes(),
        )?;
        atomic_file::write_without_backup(
            &paths.model_cache,
            cache_hash.as_deref(),
            cache.as_bytes(),
        )?;
        atomic_file::write_without_backup(
            &paths.providers,
            provider_hash.as_deref(),
            providers.as_bytes(),
        )?;
        receipt.complete = true;
        sha256 = atomic_file::write_without_backup(
            &receipt_path,
            Some(&loaded.sha256),
            &serde_json::to_vec(&receipt).map_err(|_| UpgradeError::Receipt)?,
        )?
        .sha256;
    }
    Ok(UpgradeNotice {
        backup,
        receipt: receipt_path,
        sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control_plane::ControlPlane;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    const OLD: &[u8] = b"schema_version: 1\nsecret: synthetic-key\n";
    const CACHE: &[u8] = b"old model cache\n";

    fn fixture() -> (tempfile::TempDir, AppPaths) {
        let home = tempfile::tempdir().unwrap();
        let paths = AppPaths::for_home(home.path());
        atomic_file::write(&paths.providers, None, OLD).unwrap();
        atomic_file::write(&paths.model_cache, None, CACHE).unwrap();
        atomic_file::write(&paths.install_receipt, None, b"codex receipt unchanged").unwrap();
        (home, paths)
    }

    #[test]
    fn backs_up_exact_bytes_and_starts_empty_then_only_notifies_once() {
        let (_home, paths) = fixture();
        let notice = prepare(&paths).unwrap().unwrap();
        assert_eq!(fs::read(notice.backup.join("providers.yaml")).unwrap(), OLD);
        assert_eq!(fs::read(notice.backup.join("models.yaml")).unwrap(), CACHE);
        assert_eq!(
            fs::read(&paths.install_receipt).unwrap(),
            b"codex receipt unchanged"
        );
        assert_eq!(
            fs::metadata(&notice.backup).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(notice.backup.join("providers.yaml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(
            ControlPlane::load(&paths)
                .unwrap()
                .providers()
                .providers
                .is_empty()
        );
        assert_eq!(prepare(&paths).unwrap().unwrap().backup, notice.backup);
        notice.acknowledge().unwrap();
        assert!(prepare(&paths).unwrap().is_none());
    }

    #[test]
    fn resumes_after_interruption_between_replacements() {
        let (_home, paths) = fixture();
        let original = atomic_file::load(&paths.providers).unwrap();
        begin(&paths, &original).unwrap();
        let cache = atomic_file::load(&paths.model_cache).unwrap();
        atomic_file::write_without_backup(
            &paths.model_cache,
            Some(&cache.sha256),
            yaml_serde::to_string(&empty_cache()).unwrap().as_bytes(),
        )
        .unwrap();
        let notice = prepare(&paths).unwrap().unwrap();
        assert!(ControlPlane::load(&paths).is_ok());
        assert_eq!(fs::read(notice.backup.join("models.yaml")).unwrap(), CACHE);
    }

    #[test]
    fn refuses_concurrent_edits_after_backup() {
        let (_home, paths) = fixture();
        let original = atomic_file::load(&paths.providers).unwrap();
        begin(&paths, &original).unwrap();
        fs::write(&paths.providers, b"external edit").unwrap();
        assert!(matches!(
            prepare(&paths),
            Err(UpgradeError::File(
                SecureFileError::ConcurrentModification { .. }
            ))
        ));
        assert_eq!(fs::read(&paths.providers).unwrap(), b"external edit");
        assert_eq!(fs::read(&paths.model_cache).unwrap(), CACHE);
    }

    #[test]
    fn unsafe_cache_prevents_reset_and_does_not_follow_symlinks() {
        let (_home, paths) = fixture();
        fs::remove_file(&paths.model_cache).unwrap();
        symlink(&paths.install_receipt, &paths.model_cache).unwrap();
        assert!(matches!(
            prepare(&paths),
            Err(UpgradeError::File(SecureFileError::SymbolicLink(_)))
        ));
        assert_eq!(fs::read(&paths.providers).unwrap(), OLD);
        assert!(!paths.root.join(RECEIPT).exists());
    }

    #[test]
    fn does_not_treat_invalid_current_configuration_as_an_upgrade() {
        let (_home, paths) = fixture();
        fs::write(&paths.providers, b"schema_version: 3\nproviders: invalid\n").unwrap();
        assert!(prepare(&paths).is_err());
        assert!(!paths.root.join(RECEIPT).exists());
        assert_eq!(fs::read(&paths.model_cache).unwrap(), CACHE);
    }

    #[test]
    fn missing_cache_is_allowed_and_completed_upgrade_preserves_later_edits() {
        let (_home, paths) = fixture();
        fs::remove_file(&paths.model_cache).unwrap();
        let notice = prepare(&paths).unwrap().unwrap();
        assert!(!notice.backup.join("models.yaml").exists());
        let mut document = default_providers();
        document.listener.port = 43120;
        let current = atomic_file::load(&paths.providers).unwrap();
        atomic_file::write(
            &paths.providers,
            Some(&current.sha256),
            yaml_serde::to_string(&document).unwrap().as_bytes(),
        )
        .unwrap();
        prepare(&paths).unwrap();
        assert_eq!(
            ControlPlane::load(&paths)
                .unwrap()
                .providers()
                .listener
                .port,
            43120
        );
    }
}
