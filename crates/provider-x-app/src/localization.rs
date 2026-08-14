use std::{fs, path::PathBuf, process::Command};

use thiserror::Error;

use crate::storage::{SecureFileError, atomic_file};

pub(crate) const ENGLISH_LABEL: &str = "English";
pub(crate) const SIMPLIFIED_CHINESE_LABEL: &str = "简体中文";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum UiLocale {
    #[default]
    English,
    SimplifiedChinese,
}

impl UiLocale {
    pub(crate) const fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::SimplifiedChinese => "zh-CN",
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::English => ENGLISH_LABEL,
            Self::SimplifiedChinese => SIMPLIFIED_CHINESE_LABEL,
        }
    }

    pub(crate) fn from_label(label: &str) -> Option<Self> {
        match label {
            ENGLISH_LABEL => Some(Self::English),
            SIMPLIFIED_CHINESE_LABEL => Some(Self::SimplifiedChinese),
            _ => None,
        }
    }

    pub(crate) fn from_identifier(identifier: &str) -> Self {
        let normalized = identifier.trim().replace('_', "-").to_ascii_lowercase();
        if normalized == "zh" || normalized.starts_with("zh-") {
            Self::SimplifiedChinese
        } else {
            Self::English
        }
    }

    pub(crate) fn system_default() -> Self {
        if cfg!(target_os = "macos")
            && let Ok(output) = Command::new("/usr/bin/defaults")
                .args(["read", "-g", "AppleLocale"])
                .output()
            && output.status.success()
            && let Ok(value) = String::from_utf8(output.stdout)
        {
            return Self::from_identifier(&value);
        }
        for variable in ["LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Ok(value) = std::env::var(variable)
                && !value.trim().is_empty()
            {
                return Self::from_identifier(&value);
            }
        }
        Self::English
    }

    pub(crate) fn activate(self) {
        rust_i18n::set_locale(self.code());
    }
}

#[derive(Clone, Debug)]
pub(crate) struct UiLocaleStore {
    path: PathBuf,
}

impl UiLocaleStore {
    pub(crate) const fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub(crate) fn load(&self) -> Result<Option<UiLocale>, UiLocaleError> {
        match fs::symlink_metadata(&self.path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(UiLocaleError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        }
        let loaded = atomic_file::load(&self.path)?;
        let value = std::str::from_utf8(&loaded.bytes)?.trim();
        match value {
            "en" => Ok(Some(UiLocale::English)),
            "zh-CN" => Ok(Some(UiLocale::SimplifiedChinese)),
            _ => Err(UiLocaleError::Unsupported(value.to_owned())),
        }
    }

    pub(crate) fn save(&self, locale: UiLocale) -> Result<(), UiLocaleError> {
        let expected_sha256 = match fs::symlink_metadata(&self.path) {
            Ok(_) => Some(atomic_file::load(&self.path)?.sha256),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(source) => {
                return Err(UiLocaleError::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        atomic_file::write(
            &self.path,
            expected_sha256.as_deref(),
            format!("{}\n", locale.code()).as_bytes(),
        )?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub(crate) enum UiLocaleError {
    #[error(transparent)]
    SecureFile(#[from] SecureFileError),

    #[error("locale file at {path} could not be read: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("locale file is not UTF-8: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("unsupported UI locale: {0}")]
    Unsupported(String),
}

#[cfg(test)]
mod tests {
    use super::{UiLocale, UiLocaleStore};

    #[test]
    fn locale_identifier_normalizes_chinese_variants() {
        assert_eq!(
            UiLocale::from_identifier("zh_CN.UTF-8"),
            UiLocale::SimplifiedChinese
        );
        assert_eq!(
            UiLocale::from_identifier("zh-Hans-CN"),
            UiLocale::SimplifiedChinese
        );
        assert_eq!(UiLocale::from_identifier("en_US.UTF-8"), UiLocale::English);
    }

    #[test]
    fn locale_preference_round_trips_privately() {
        let directory = tempfile::tempdir().unwrap();
        let store = UiLocaleStore::new(directory.path().join("preferences/ui-locale"));
        assert_eq!(store.load().unwrap(), None);

        store.save(UiLocale::SimplifiedChinese).unwrap();
        assert_eq!(store.load().unwrap(), Some(UiLocale::SimplifiedChinese));

        store.save(UiLocale::English).unwrap();
        assert_eq!(store.load().unwrap(), Some(UiLocale::English));
    }

    #[test]
    fn bundled_resources_include_english_and_simplified_chinese() {
        assert_eq!(rust_i18n::t!("app.global.about", locale = "en"), "About");
        assert_eq!(rust_i18n::t!("app.global.about", locale = "zh-CN"), "关于");
    }
}
