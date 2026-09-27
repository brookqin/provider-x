use crate::storage::atomic_file;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ThemePreference {
    #[default]
    Dark,
    Light,
}

pub(crate) fn load(path: &Path) -> anyhow::Result<ThemePreference> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(ThemePreference::default())
        }
        Err(error) => Err(error.into()),
        Ok(_) => Ok(serde_json::from_slice(&atomic_file::load(path)?.bytes)?),
    }
}

pub(crate) fn save(path: &Path, theme: ThemePreference) -> anyhow::Result<()> {
    let previous = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
        Ok(_) => Some(atomic_file::load(path)?.sha256),
    };
    atomic_file::write(path, previous.as_deref(), &serde_json::to_vec(&theme)?)?;
    Ok(())
}
