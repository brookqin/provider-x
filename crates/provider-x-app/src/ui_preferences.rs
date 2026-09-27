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
    load_value(path)
}

pub(crate) fn load_dock_visible(path: &Path) -> anyhow::Result<bool> {
    load_value(path)
}

fn load_value<T: serde::de::DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(error.into()),
        Ok(_) => Ok(serde_json::from_slice(&atomic_file::load(path)?.bytes)?),
    }
}

pub(crate) fn save(path: &Path, theme: ThemePreference) -> anyhow::Result<()> {
    save_value(path, &theme)
}

pub(crate) fn save_dock_visible(path: &Path, visible: bool) -> anyhow::Result<()> {
    save_value(path, &visible)
}

fn save_value(path: &Path, value: &impl Serialize) -> anyhow::Result<()> {
    let previous = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
        Ok(_) => Some(atomic_file::load(path)?.sha256),
    };
    atomic_file::write(path, previous.as_deref(), &serde_json::to_vec(value)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn dock_visibility_defaults_to_hidden_and_survives_reload() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("preferences/ui-dock.json");
        assert!(!load_dock_visible(&path).unwrap());
        for visible in [true, false] {
            save_dock_visible(&path, visible).unwrap();
            assert_eq!(load_dock_visible(&path).unwrap(), visible);
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn dock_visibility_rejects_invalid_or_redirected_preferences() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("preferences/ui-dock.json");
        save_dock_visible(&path, true).unwrap();
        fs::write(&path, b"not a boolean").unwrap();
        assert!(load_dock_visible(&path).is_err());

        let target = home.path().join("preferences/target.json");
        fs::rename(&path, &target).unwrap();
        symlink(&target, &path).unwrap();
        assert!(load_dock_visible(&path).is_err());
        assert!(save_dock_visible(&path, false).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"not a boolean");
    }
}
