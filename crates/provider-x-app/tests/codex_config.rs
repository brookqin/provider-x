use std::{fs, os::unix::fs::PermissionsExt};

use provider_x_app::codex_config::{
    CodexConfigEditor, CodexConfigError, CodexIntegration, InstallReceiptStore, ReceiptPhase,
};
use toml_edit::DocumentMut;

const TEST_CAPABILITY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn paths() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let home = tempfile::tempdir().unwrap();
    let codex = home.path().join(".codex");
    fs::create_dir(&codex).unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o755)).unwrap();
    let receipt = home
        .path()
        .join("Library/Application Support/dev.qiankun.provider-x/install-receipt.json");
    (home, codex.join("config.toml"), receipt)
}

fn write_config(path: &std::path::Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn desired() -> CodexIntegration {
    CodexIntegration {
        openai_base_url: format!("http://127.0.0.1:43119/{TEST_CAPABILITY}/v1"),
    }
}

#[test]
fn integration_manages_three_keys_and_restores_exact_bytes() {
    let (_home, config_path, receipt_path) = paths();
    let original = "# keep this comment\nmodel = \"gpt-5.6\"\nmodel_provider = \"custom\"\nmodel_catalog_json = \"/tmp/original.json\"\n\n[model_providers.custom]\nbase_url = \"https://example.invalid\"\n\n[features]\nmulti_agent = true\n";
    write_config(&config_path, original);
    let editor = CodexConfigEditor::new(&config_path, &receipt_path);

    editor.apply(&desired(), "enabled").unwrap();
    let installed = fs::read_to_string(&config_path)
        .unwrap()
        .parse::<DocumentMut>()
        .unwrap();
    assert_eq!(
        installed["openai_base_url"].as_str(),
        Some(
            "http://127.0.0.1:43119/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/v1"
        )
    );
    assert!(installed.get("model_catalog_json").is_none());
    assert_eq!(installed["model_provider"].as_str(), Some("openai"));
    assert!(installed["model_providers"]["custom"].is_table());
    assert!(installed["features"]["multi_agent"].as_bool().unwrap());
    assert_eq!(editor.active_integration().unwrap(), Some(desired()));
    let debug = format!("{:?}", editor.active_integration().unwrap().unwrap());
    assert!(!debug.contains(TEST_CAPABILITY));

    let restored = editor.restore("restored").unwrap();
    assert_eq!(restored.phase, ReceiptPhase::Restored);
    assert_eq!(editor.active_integration().unwrap(), None);
    assert_eq!(fs::read_to_string(&config_path).unwrap(), original);
    assert_eq!(
        fs::metadata(&receipt_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn absent_managed_keys_are_removed_again_on_restore() {
    let (_home, config_path, receipt_path) = paths();
    let original = "model = \"gpt-5.6\"\n";
    write_config(&config_path, original);
    let editor = CodexConfigEditor::new(&config_path, &receipt_path);

    editor.apply(&desired(), "enabled").unwrap();
    let installed = fs::read_to_string(&config_path)
        .unwrap()
        .parse::<DocumentMut>()
        .unwrap();
    assert_eq!(installed["model_provider"].as_str(), Some("openai"));

    editor.restore("restored").unwrap();
    assert_eq!(fs::read_to_string(&config_path).unwrap(), original);
}

#[test]
fn unrelated_external_change_survives_managed_key_restore() {
    let (_home, config_path, receipt_path) = paths();
    write_config(&config_path, "model = \"gpt-5.6\"\n");
    let editor = CodexConfigEditor::new(&config_path, &receipt_path);
    editor.apply(&desired(), "installed").unwrap();

    let mut drifted = fs::read_to_string(&config_path).unwrap();
    drifted.push_str("approval_policy = \"never\"\n");
    write_config(&config_path, &drifted);

    editor.restore("restored").unwrap();
    let restored = fs::read_to_string(&config_path).unwrap();
    assert!(restored.contains("model = \"gpt-5.6\""));
    assert!(restored.contains("approval_policy = \"never\""));
    assert!(!restored.contains("openai_base_url"));
}

#[test]
fn managed_key_drift_blocks_restore_without_overwrite() {
    let (_home, config_path, receipt_path) = paths();
    write_config(&config_path, "model = \"gpt-5.6\"\n");
    let editor = CodexConfigEditor::new(&config_path, &receipt_path);
    editor.apply(&desired(), "installed").unwrap();

    let drifted = fs::read_to_string(&config_path)
        .unwrap()
        .replace(
            "http://127.0.0.1:43119/0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef/v1",
            "http://127.0.0.1:9999/ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff/v1",
        );
    write_config(&config_path, &drifted);

    let error = editor.restore("blocked").unwrap_err();
    assert!(matches!(error, CodexConfigError::ConfigDrift));
    assert_eq!(fs::read_to_string(&config_path).unwrap(), drifted);
}

#[test]
fn restore_removes_a_config_that_did_not_exist_before_install() {
    let (_home, config_path, receipt_path) = paths();
    let editor = CodexConfigEditor::new(&config_path, &receipt_path);

    editor.apply(&desired(), "installed").unwrap();
    assert!(config_path.is_file());
    assert_eq!(
        fs::metadata(&config_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    editor.restore("restored").unwrap();
    assert!(!config_path.exists());
}

#[test]
fn prepared_receipt_recovers_when_config_write_already_landed() {
    let (_home, config_path, receipt_path) = paths();
    write_config(&config_path, "model = \"gpt-5.6\"\n");
    let editor = CodexConfigEditor::new(&config_path, &receipt_path);
    let active = editor.apply(&desired(), "installed").unwrap();
    let ReceiptPhase::Active { applied_sha256 } = active.phase else {
        panic!("expected active receipt");
    };
    let store = InstallReceiptStore::new(&receipt_path);
    let loaded = store.load().unwrap();
    let mut prepared = loaded.receipt;
    prepared.phase = ReceiptPhase::Prepared {
        planned_sha256: applied_sha256,
    };
    store.save(&prepared, Some(&loaded.sha256)).unwrap();

    editor.restore("restored").unwrap();
    assert_eq!(
        fs::read_to_string(&config_path).unwrap(),
        "model = \"gpt-5.6\"\n"
    );
}

#[test]
fn writable_codex_directory_is_rejected_and_prepared_receipt_can_retry() {
    let (_home, config_path, receipt_path) = paths();
    let codex_directory = config_path.parent().unwrap();
    fs::set_permissions(codex_directory, fs::Permissions::from_mode(0o777)).unwrap();
    let editor = CodexConfigEditor::new(&config_path, &receipt_path);

    let error = editor.apply(&desired(), "blocked").unwrap_err();
    assert!(error.to_string().contains("permissions 777"));
    assert!(!config_path.exists());
    assert!(matches!(
        InstallReceiptStore::new(&receipt_path)
            .load()
            .unwrap()
            .receipt
            .phase,
        ReceiptPhase::Prepared { .. }
    ));

    fs::set_permissions(codex_directory, fs::Permissions::from_mode(0o755)).unwrap();
    let active = editor.apply(&desired(), "retried").unwrap();
    assert!(matches!(active.phase, ReceiptPhase::Active { .. }));
    assert!(config_path.is_file());
}

#[test]
fn proxy_only_integration_backs_up_and_invalidates_the_codex_model_cache() {
    let (_home, config_path, receipt_path) = paths();
    let original = "model = \"gpt-5.6\"\nmodel_provider = \"custom\"\nmodel_catalog_json = \"/tmp/original.json\"\n";
    write_config(&config_path, original);
    let model_cache_path = config_path.parent().unwrap().join("models_cache.json");
    let original_cache = r#"{"client_version":"0.147.0","models":[]}"#;
    write_config(&model_cache_path, original_cache);
    fs::set_permissions(&model_cache_path, fs::Permissions::from_mode(0o644)).unwrap();
    let editor = CodexConfigEditor::new(&config_path, &receipt_path);

    let receipt = editor.apply(&desired(), "enabled").unwrap();
    let installed = fs::read_to_string(&config_path).unwrap();
    assert!(installed.contains("openai_base_url"));
    assert!(!installed.contains("model_catalog_json"));
    assert!(installed.contains("model_provider = \"openai\""));
    assert!(!model_cache_path.exists());
    let backup = receipt
        .original_model_cache
        .expect("cache backup in receipt");
    assert!(backup.existed);
    assert_eq!(backup.contents, original_cache);

    write_config(
        &model_cache_path,
        r#"{"client_version":"0.147.0","models":[{"slug":"provider-a/coder"}]}"#,
    );
    fs::set_permissions(&model_cache_path, fs::Permissions::from_mode(0o644)).unwrap();
    editor.restore("disabled").unwrap();
    assert_eq!(fs::read_to_string(&config_path).unwrap(), original);
    assert!(
        !model_cache_path.exists(),
        "disable must force the next official startup to refresh instead of restoring stale data"
    );
}

#[test]
fn active_reconcile_invalidates_regenerated_cache_without_rebasing_backup() {
    let (_home, config_path, receipt_path) = paths();
    let original = "model = \"gpt-5.6\"\n";
    write_config(&config_path, original);
    let model_cache_path = config_path.parent().unwrap().join("models_cache.json");
    let original_cache = r#"{"client_version":"0.147.0","models":[]}"#;
    write_config(&model_cache_path, original_cache);
    fs::set_permissions(&model_cache_path, fs::Permissions::from_mode(0o644)).unwrap();
    let editor = CodexConfigEditor::new(&config_path, &receipt_path);

    editor.apply(&desired(), "enabled").unwrap();
    let regenerated_cache =
        r#"{"client_version":"0.147.0","models":[{"slug":"provider-a/coder"}]}"#;
    write_config(&model_cache_path, regenerated_cache);
    fs::set_permissions(&model_cache_path, fs::Permissions::from_mode(0o644)).unwrap();

    let reconciled = editor.apply(&desired(), "provider-updated").unwrap();
    assert!(
        !model_cache_path.exists(),
        "active reconcile must force the next model/list call to fetch the new projection"
    );
    let original_backup = reconciled
        .original_model_cache
        .expect("original cache backup must survive active reconcile");
    assert!(original_backup.existed);
    assert_eq!(original_backup.contents, original_cache);

    editor.restore("disabled").unwrap();
    assert_eq!(fs::read_to_string(&config_path).unwrap(), original);
}

#[test]
fn group_writable_codex_model_cache_is_rejected_before_config_changes() {
    let (_home, config_path, receipt_path) = paths();
    let original = "model = \"gpt-5.6\"\n";
    write_config(&config_path, original);
    let model_cache_path = config_path.parent().unwrap().join("models_cache.json");
    write_config(&model_cache_path, r#"{"models":[]}"#);
    fs::set_permissions(&model_cache_path, fs::Permissions::from_mode(0o664)).unwrap();

    let editor = CodexConfigEditor::new(&config_path, &receipt_path);
    let error = editor.apply(&desired(), "blocked").unwrap_err();

    assert!(error.to_string().contains("permissions 664"));
    assert_eq!(fs::read_to_string(&config_path).unwrap(), original);
    assert!(!receipt_path.exists());
    assert!(model_cache_path.exists());
}

#[test]
fn legacy_install_receipt_is_rejected_without_migration() {
    let (_home, config_path, receipt_path) = paths();
    write_config(&config_path, "model = \"gpt-5.6\"\n");
    let editor = CodexConfigEditor::new(&config_path, &receipt_path);
    editor.apply(&desired(), "enabled").unwrap();

    let mut receipt: serde_json::Value =
        serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
    receipt["schema_version"] = serde_json::json!(1);
    write_config(
        &receipt_path,
        &serde_json::to_string_pretty(&receipt).unwrap(),
    );

    let error = editor.inspect().unwrap_err();
    assert!(error.to_string().contains("unsupported schema version"));
}
