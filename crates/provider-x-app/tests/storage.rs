use std::{fs, os::unix::fs::PermissionsExt};

use provider_x_app::storage::{
    ModelRegistryStore, ProviderConfigStore, ProviderConfigStoreError, SecureFileError,
    SingleInstanceError, SingleInstanceGuard,
};
use provider_x_catalog::{MODEL_REGISTRY_SCHEMA_VERSION, MODEL_REGISTRY_URL, ModelRegistryCache};
use provider_x_core::ProvidersDocument;
use serde_json::json;

const PROVIDERS_YAML: &str = r"
schema_version: 1
listener:
  host: 127.0.0.1
  port: 43119
  request_body_limit_bytes: 33554432
  max_connections: 128
timeouts:
  request_body_ms: 30000
  connect_ms: 10000
  response_headers_ms: 30000
  stream_idle_ms: 300000
  websocket_idle_ms: 300000
  shutdown_grace_ms: 30000
codex:
  manage_user_config: true
providers: []
";

#[test]
fn store_writes_private_file_and_detects_concurrent_change() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("provider-x/providers.yaml");
    let store = ProviderConfigStore::new(&path);
    let document = ProvidersDocument::from_yaml(PROVIDERS_YAML).unwrap();

    let first = store.save(&document, None).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(store.load().unwrap().document, document);

    let second = store.save(&document, Some(&first.sha256)).unwrap();
    assert_eq!(second.sha256, first.sha256);
    let backup_directory = directory.path().join("provider-x/backups");
    assert!(backup_directory.is_dir());
    assert_eq!(
        fs::metadata(&backup_directory)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let backup_path = fs::read_dir(&backup_directory)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_eq!(
        fs::metadata(backup_path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let debug = format!("{second:?}");
    assert!(debug.contains("[REDACTED"));
    assert!(!debug.contains("schema_version"));

    let error = store.save(&document, Some("stale-hash")).unwrap_err();
    assert!(error.to_string().contains("concurrent modification"));
}

#[test]
fn store_rejects_symbolic_links() {
    use std::os::unix::fs::symlink;

    let directory = tempfile::tempdir().unwrap();
    let target = directory.path().join("target.yaml");
    fs::write(&target, PROVIDERS_YAML).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    let link = directory.path().join("providers.yaml");
    symlink(&target, &link).unwrap();

    let error = ProviderConfigStore::new(&link).load().unwrap_err();
    assert!(matches!(
        error,
        ProviderConfigStoreError::File(SecureFileError::SymbolicLink(_))
    ));
}

#[test]
fn store_rejects_hard_links_and_insecure_permissions() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("providers.yaml");
    fs::write(&path, PROVIDERS_YAML).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&path, directory.path().join("alias.yaml")).unwrap();

    let error = ProviderConfigStore::new(&path).load().unwrap_err();
    assert!(error.to_string().contains("hard links"));

    fs::remove_file(directory.path().join("alias.yaml")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let error = ProviderConfigStore::new(&path).load().unwrap_err();
    assert!(error.to_string().contains("permissions 644"));
}

#[test]
fn store_never_repairs_an_existing_insecure_parent_directory() {
    let directory = tempfile::tempdir().unwrap();
    let parent = directory.path().join("provider-x");
    fs::create_dir(&parent).unwrap();
    fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();
    let path = parent.join("providers.yaml");
    let document = ProvidersDocument::from_yaml(PROVIDERS_YAML).unwrap();

    let error = ProviderConfigStore::new(path)
        .save(&document, None)
        .unwrap_err();
    assert!(error.to_string().contains("permissions 755"));
    assert_eq!(
        fs::metadata(parent).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[test]
fn model_registry_store_round_trips_private_validated_cache() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join("provider-x/cache/model-registry.json");
    let store = ModelRegistryStore::new(&path);
    let cache = ModelRegistryCache {
        schema_version: MODEL_REGISTRY_SCHEMA_VERSION,
        source_url: MODEL_REGISTRY_URL.to_owned(),
        fetched_at: "2026-08-12T00:00:00Z".to_owned(),
        etag: Some("registry-v1".to_owned()),
        payload: json!({"provider-a":{"models":{"coder":{"id":"coder"}}}}),
    };

    let saved = store.save(&cache, None).unwrap();
    let loaded = store.load().unwrap();

    assert_eq!(loaded.document, cache);
    assert_eq!(loaded.sha256, saved.sha256);
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(store.save(&cache, Some("stale-hash")).is_err());
}

#[test]
fn single_instance_lock_releases_on_drop_without_stale_lockout() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("provider-x/provider-x.lock");

    let first = SingleInstanceGuard::acquire(&path).unwrap();
    let second = SingleInstanceGuard::acquire(&path).unwrap_err();
    assert!(matches!(second, SingleInstanceError::AlreadyRunning));
    drop(first);

    SingleInstanceGuard::acquire(&path).unwrap();
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn normal_startup_can_wait_for_a_gracefully_exiting_instance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("provider-x/provider-x.lock");
    let first = SingleInstanceGuard::acquire(&path).unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        drop(first);
    });

    let started = std::time::Instant::now();
    let second =
        SingleInstanceGuard::acquire_with_timeout(&path, std::time::Duration::from_secs(1))
            .unwrap();
    assert!(started.elapsed() >= std::time::Duration::from_millis(100));
    drop(second);
    release.join().unwrap();
}
