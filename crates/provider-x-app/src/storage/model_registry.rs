use std::{path::PathBuf, sync::Arc};

use provider_x_catalog::ModelRegistryCache;
use thiserror::Error;

use super::atomic_file::{self, LoadedFile, SecureFileError};

#[derive(Debug, Error)]
pub enum ModelRegistryStoreError {
    #[error(transparent)]
    File(#[from] SecureFileError),

    #[error("invalid model registry cache JSON: {0}")]
    InvalidDocument(String),

    #[error("failed to serialize model registry cache JSON: {0}")]
    Serialization(String),
}

#[derive(Clone, Debug)]
pub struct LoadedModelRegistry {
    pub document: ModelRegistryCache,
    pub sha256: String,
}

#[derive(Clone, Debug)]
pub struct ModelRegistryStore {
    path: Arc<PathBuf>,
}

impl ModelRegistryStore {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Arc::new(path.into()),
        }
    }

    /// Loads and validates a securely stored registry cache.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe file attributes, malformed JSON, or an invalid cache envelope.
    pub fn load(&self) -> Result<LoadedModelRegistry, ModelRegistryStoreError> {
        let loaded = atomic_file::load(&self.path)?;
        let document: ModelRegistryCache = serde_json::from_slice(&loaded.bytes)
            .map_err(|error| ModelRegistryStoreError::InvalidDocument(error.to_string()))?;
        document
            .validate()
            .map_err(|error| ModelRegistryStoreError::InvalidDocument(error.to_string()))?;
        Ok(LoadedModelRegistry {
            document,
            sha256: loaded.sha256,
        })
    }

    /// Saves a validated registry cache using the standard private, hash-checked atomic writer.
    ///
    /// # Errors
    ///
    /// Returns an error for validation, serialization, concurrent changes, or unsafe files.
    pub fn save(
        &self,
        document: &ModelRegistryCache,
        expected_sha256: Option<&str>,
    ) -> Result<LoadedFile, ModelRegistryStoreError> {
        document
            .validate()
            .map_err(|error| ModelRegistryStoreError::InvalidDocument(error.to_string()))?;
        let mut bytes = serde_json::to_vec_pretty(document)
            .map_err(|error| ModelRegistryStoreError::Serialization(error.to_string()))?;
        bytes.push(b'\n');
        atomic_file::write(&self.path, expected_sha256, &bytes).map_err(Into::into)
    }
}
