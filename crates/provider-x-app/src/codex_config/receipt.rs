use std::{fmt, path::PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::storage::{LoadedFile, SecureFileError};

use crate::storage::atomic_file;

pub const INSTALL_RECEIPT_SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ReceiptPhase {
    Prepared { planned_sha256: String },
    Active { applied_sha256: String },
    Restored,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalCodexConfig {
    pub existed: bool,
    pub sha256: Option<String>,
    pub contents: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginalCodexModelCache {
    pub existed: bool,
    pub sha256: Option<String>,
    pub contents: String,
}

impl fmt::Debug for OriginalCodexModelCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OriginalCodexModelCache")
            .field("existed", &self.existed)
            .field("sha256", &self.sha256)
            .field(
                "contents",
                &format_args!("[REDACTED; {} bytes]", self.contents.len()),
            )
            .finish()
    }
}

impl fmt::Debug for OriginalCodexConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OriginalCodexConfig")
            .field("existed", &self.existed)
            .field("sha256", &self.sha256)
            .field(
                "contents",
                &format_args!("[REDACTED; {} bytes]", self.contents.len()),
            )
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedCodexValues {
    pub openai_base_url: String,
}

impl fmt::Debug for AppliedCodexValues {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AppliedCodexValues")
            .field("openai_base_url", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallReceipt {
    pub schema_version: u32,
    pub config_path: String,
    pub updated_at: String,
    pub phase: ReceiptPhase,
    pub original: OriginalCodexConfig,
    pub original_model_cache: Option<OriginalCodexModelCache>,
    pub applied_values: AppliedCodexValues,
}

impl InstallReceipt {
    /// Checks the receipt envelope before it can participate in a config transaction.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported schema or inconsistent original-file metadata.
    pub fn validate(&self) -> Result<(), ReceiptStoreError> {
        if self.schema_version != INSTALL_RECEIPT_SCHEMA_VERSION {
            return Err(ReceiptStoreError::InvalidDocument(
                "unsupported schema version".to_owned(),
            ));
        }
        if self.config_path.trim().is_empty() {
            return Err(ReceiptStoreError::InvalidDocument(
                "config path is empty".to_owned(),
            ));
        }
        if self.original.existed != self.original.sha256.is_some() {
            return Err(ReceiptStoreError::InvalidDocument(
                "original existence and hash disagree".to_owned(),
            ));
        }
        if !self.original.existed && !self.original.contents.is_empty() {
            return Err(ReceiptStoreError::InvalidDocument(
                "missing original config has contents".to_owned(),
            ));
        }
        if let Some(expected) = &self.original.sha256
            && hex::encode(Sha256::digest(self.original.contents.as_bytes())) != *expected
        {
            return Err(ReceiptStoreError::InvalidDocument(
                "original config hash does not match contents".to_owned(),
            ));
        }
        if let Some(cache) = &self.original_model_cache {
            if cache.existed != cache.sha256.is_some() {
                return Err(ReceiptStoreError::InvalidDocument(
                    "original model cache existence and hash disagree".to_owned(),
                ));
            }
            if !cache.existed && !cache.contents.is_empty() {
                return Err(ReceiptStoreError::InvalidDocument(
                    "missing original model cache has contents".to_owned(),
                ));
            }
            if let Some(expected) = &cache.sha256
                && hex::encode(Sha256::digest(cache.contents.as_bytes())) != *expected
            {
                return Err(ReceiptStoreError::InvalidDocument(
                    "original model cache hash does not match contents".to_owned(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ReceiptStoreError {
    #[error(transparent)]
    File(#[from] SecureFileError),

    #[error("invalid install receipt: {0}")]
    InvalidDocument(String),

    #[error("failed to serialize install receipt: {0}")]
    Serialization(String),
}

#[derive(Clone, Debug)]
pub struct LoadedInstallReceipt {
    pub receipt: InstallReceipt,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct InstallReceiptStore {
    path: PathBuf,
}

impl InstallReceiptStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Loads and validates the securely stored receipt.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe storage, malformed JSON, or an invalid receipt envelope.
    pub fn load(&self) -> Result<LoadedInstallReceipt, ReceiptStoreError> {
        let loaded = atomic_file::load(&self.path)?;
        let receipt: InstallReceipt = serde_json::from_slice(&loaded.bytes)
            .map_err(|error| ReceiptStoreError::InvalidDocument(error.to_string()))?;
        receipt.validate()?;
        Ok(LoadedInstallReceipt {
            receipt,
            sha256: loaded.sha256,
        })
    }

    /// Atomically saves a validated receipt with optional hash-based concurrency protection.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid data, serialization failure, or an unsafe/concurrent write.
    pub fn save(
        &self,
        receipt: &InstallReceipt,
        expected_sha256: Option<&str>,
    ) -> Result<LoadedFile, ReceiptStoreError> {
        receipt.validate()?;
        let mut bytes = serde_json::to_vec_pretty(receipt)
            .map_err(|error| ReceiptStoreError::Serialization(error.to_string()))?;
        bytes.push(b'\n');
        atomic_file::write(&self.path, expected_sha256, &bytes).map_err(Into::into)
    }
}
