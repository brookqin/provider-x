use std::path::{Path, PathBuf};

use provider_x_core::ModelCacheDocument;
use thiserror::Error;

use super::atomic_file::{self, LoadedFile, SecureFileError};

#[derive(Debug, Error)]
pub enum ModelCacheStoreError {
    #[error(transparent)]
    File(#[from] SecureFileError),

    #[error("invalid model cache YAML: {0}")]
    InvalidDocument(String),

    #[error("failed to serialize model cache YAML: {0}")]
    Serialization(String),
}

#[derive(Clone, Debug)]
pub struct LoadedModelCache {
    pub document: ModelCacheDocument,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct ModelCacheStore {
    path: PathBuf,
}

impl ModelCacheStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Loads the model cache from a secure regular file.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe file attributes, I/O failures, or invalid cache YAML.
    pub fn load(&self) -> Result<LoadedModelCache, ModelCacheStoreError> {
        let loaded = atomic_file::load(&self.path)?;
        let yaml = String::from_utf8(loaded.bytes)
            .map_err(|error| ModelCacheStoreError::InvalidDocument(error.to_string()))?;
        let document = ModelCacheDocument::from_yaml(&yaml)
            .map_err(|error| ModelCacheStoreError::InvalidDocument(error.to_string()))?;
        Ok(LoadedModelCache {
            document,
            sha256: loaded.sha256,
        })
    }

    /// Saves model cache YAML if the file still matches the caller's previously loaded hash.
    ///
    /// # Errors
    ///
    /// Returns an error for serialization, concurrent changes, unsafe file attributes, or atomic
    /// write failures.
    pub fn save(
        &self,
        document: &ModelCacheDocument,
        expected_sha256: Option<&str>,
    ) -> Result<LoadedFile, ModelCacheStoreError> {
        document
            .validate()
            .map_err(|error| ModelCacheStoreError::InvalidDocument(error.to_string()))?;
        let yaml = yaml_serde::to_string(document)
            .map_err(|error| ModelCacheStoreError::Serialization(error.to_string()))?;
        atomic_file::write(&self.path, expected_sha256, yaml.as_bytes()).map_err(Into::into)
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}
