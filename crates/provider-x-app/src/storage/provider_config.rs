use std::path::{Path, PathBuf};

use provider_x_core::ProvidersDocument;
use thiserror::Error;

use super::atomic_file::{self, LoadedFile, SecureFileError};

#[derive(Debug, Error)]
pub enum ProviderConfigStoreError {
    #[error(transparent)]
    File(#[from] SecureFileError),

    #[error("invalid Provider YAML: {0}")]
    InvalidDocument(String),

    #[error("failed to serialize Provider YAML: {0}")]
    Serialization(String),
}

#[derive(Clone, Debug)]
pub struct LoadedProviderConfig {
    pub document: ProvidersDocument,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct ProviderConfigStore {
    path: PathBuf,
}

impl ProviderConfigStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Loads and validates the Provider YAML from a secure regular file.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe file attributes, I/O failures, or invalid configuration.
    pub fn load(&self) -> Result<LoadedProviderConfig, ProviderConfigStoreError> {
        let loaded = atomic_file::load(&self.path)?;
        let yaml = String::from_utf8(loaded.bytes)
            .map_err(|error| ProviderConfigStoreError::InvalidDocument(error.to_string()))?;
        let document = ProvidersDocument::from_yaml(&yaml)
            .map_err(|error| ProviderConfigStoreError::InvalidDocument(error.to_string()))?;
        Ok(LoadedProviderConfig {
            document,
            sha256: loaded.sha256,
        })
    }

    /// Saves Provider YAML if the file still matches the caller's previously loaded hash.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid documents, concurrent changes, unsafe file attributes, or
    /// atomic write failures.
    pub fn save(
        &self,
        document: &ProvidersDocument,
        expected_sha256: Option<&str>,
    ) -> Result<LoadedFile, ProviderConfigStoreError> {
        document
            .validate()
            .map_err(|error| ProviderConfigStoreError::InvalidDocument(error.to_string()))?;
        let yaml = yaml_serde::to_string(document)
            .map_err(|error| ProviderConfigStoreError::Serialization(error.to_string()))?;
        atomic_file::write(&self.path, expected_sha256, yaml.as_bytes()).map_err(Into::into)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}
