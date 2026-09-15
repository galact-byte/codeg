//! Snapshot assembly, validation, and application.
//!
//! A snapshot is two files that always travel together:
//!
//! * `config.json` — `{ schemaVersion, domains: { <domain id>: <payload> } }`,
//!   produced by iterating [`CONFIG_DOMAINS`].
//! * `manifest.json` — provenance (who wrote it, when, with which app
//!   version) plus the size and sha256 of `config.json`.
//!
//! The manifest is what makes a half-finished upload detectable: WebDAV has no
//! multi-file transaction, so a reader that finds a `config.json` whose bytes
//! do not match the manifest checksum refuses it instead of applying a
//! truncated configuration.
//!
//! Application is one transaction over all domains. A domain that fails to
//! decode aborts the whole apply — half-applied settings ("providers moved
//! over but the agents still point at the old ones") are worse than no apply.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use sea_orm::{DatabaseConnection, TransactionTrait};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::domains::{count_entries, CONFIG_DOMAINS};
use crate::app_error::{
    AppCommandError, CONFIG_SYNC_I18N_KEY_CHECKSUM, CONFIG_SYNC_I18N_KEY_INVALID_SNAPSHOT,
    CONFIG_SYNC_I18N_KEY_NEWER_SCHEMA,
};

/// Bump only for a change older binaries cannot read. Adding a domain does not
/// qualify: unknown domains are ignored on read and missing domains decode as
/// empty, so both directions already degrade gracefully.
pub const SCHEMA_VERSION: u32 = 1;

/// v1 uploads plaintext. The security boundary is the user's own
/// self-authenticated WebDAV endpoint; the field exists so a future encrypted
/// format is a value change rather than a format break, and so today's reader
/// refuses a file it cannot decrypt instead of misparsing it.
pub const ENCRYPTION_NONE: &str = "none";

pub const CONFIG_FILE_NAME: &str = "config.json";
pub const MANIFEST_FILE_NAME: &str = "manifest.json";

/// How many pre-apply rollback snapshots to keep on disk. They are tens of KB
/// each; ten is roughly "the last few times I pulled config down".
const ROLLBACK_KEEP: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSnapshot {
    pub schema_version: u32,
    /// Domain id → payload. A map, not a struct with one field per domain, so
    /// [`CONFIG_DOMAINS`] stays the only place a domain is declared.
    #[serde(default)]
    pub domains: BTreeMap<String, Value>,
}

impl ConfigSnapshot {
    /// Entry count per domain, for the manifest and for the import preview.
    pub fn counts(&self) -> BTreeMap<String, usize> {
        self.domains
            .iter()
            .map(|(id, value)| (id.clone(), count_entries(value)))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigFileMeta {
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigManifest {
    pub schema_version: u32,
    pub encryption: String,
    /// RFC 3339, UTC.
    pub created_at: String,
    pub app_version: String,
    /// Best-effort hostname, so a user staring at "last synced from" can tell
    /// which machine wrote the snapshot.
    pub source_device: String,
    pub config: ConfigFileMeta,
    #[serde(default)]
    pub counts: BTreeMap<String, usize>,
}

/// What an apply actually changed, per domain.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplyReport {
    pub domains: BTreeMap<String, usize>,
    pub total: usize,
}

/// Read every domain out of the local database.
pub async fn collect_snapshot_core(
    conn: &DatabaseConnection,
) -> Result<ConfigSnapshot, AppCommandError> {
    let mut domains = BTreeMap::new();
    for domain in CONFIG_DOMAINS {
        let value = (domain.collect)(conn).await?;
        domains.insert(domain.id.to_string(), value);
    }
    Ok(ConfigSnapshot {
        schema_version: SCHEMA_VERSION,
        domains,
    })
}

/// Upsert every domain the snapshot carries, in [`CONFIG_DOMAINS`] order, in a
/// single transaction. Domains this binary does not know are ignored — a
/// snapshot from a newer build stays partially usable.
pub async fn apply_snapshot_core(
    conn: &DatabaseConnection,
    snapshot: &ConfigSnapshot,
) -> Result<ApplyReport, AppCommandError> {
    reject_newer_schema(snapshot.schema_version)?;

    for id in snapshot.domains.keys() {
        if !CONFIG_DOMAINS.iter().any(|d| d.id == id) {
            tracing::warn!("[CONFIG-SYNC] ignoring unknown snapshot domain '{id}'");
        }
    }

    let tx = conn.begin().await.map_err(|e| {
        AppCommandError::database_error("Begin config apply").with_detail(e.to_string())
    })?;

    let mut report = ApplyReport::default();
    for domain in CONFIG_DOMAINS {
        let Some(value) = snapshot.domains.get(domain.id) else {
            continue;
        };
        match (domain.apply)(&tx, value).await {
            Ok(applied) => {
                report.total += applied;
                report.domains.insert(domain.id.to_string(), applied);
            }
            Err(err) => {
                // Roll back explicitly so the failure surfaces as the domain's
                // error rather than as a dropped-transaction warning.
                let _ = tx.rollback().await;
                return Err(err);
            }
        }
    }

    tx.commit().await.map_err(|e| {
        AppCommandError::database_error("Commit config apply").with_detail(e.to_string())
    })?;
    Ok(report)
}

/// Pretty-printed on purpose: the file is the user's own configuration sitting
/// on their own storage, and a readable diff is worth the extra bytes. The
/// checksum is taken over exactly these bytes.
pub fn serialize_snapshot(snapshot: &ConfigSnapshot) -> Result<Vec<u8>, AppCommandError> {
    serde_json::to_vec_pretty(snapshot).map_err(|e| {
        AppCommandError::task_execution_failed("Serialize config snapshot")
            .with_detail(e.to_string())
    })
}

pub fn parse_snapshot(bytes: &[u8]) -> Result<ConfigSnapshot, AppCommandError> {
    let snapshot: ConfigSnapshot = serde_json::from_slice(bytes).map_err(|e| {
        AppCommandError::invalid_input("Not a codeg config snapshot")
            .with_detail(e.to_string())
            .with_i18n(CONFIG_SYNC_I18N_KEY_INVALID_SNAPSHOT, BTreeMap::new())
    })?;
    reject_newer_schema(snapshot.schema_version)?;
    Ok(snapshot)
}

pub fn parse_manifest(bytes: &[u8]) -> Result<ConfigManifest, AppCommandError> {
    serde_json::from_slice(bytes).map_err(|e| {
        AppCommandError::invalid_input("Not a codeg config manifest")
            .with_detail(e.to_string())
            .with_i18n(CONFIG_SYNC_I18N_KEY_INVALID_SNAPSHOT, BTreeMap::new())
    })
}

pub fn build_manifest(
    config_bytes: &[u8],
    app_version: &str,
    counts: BTreeMap<String, usize>,
) -> ConfigManifest {
    ConfigManifest {
        schema_version: SCHEMA_VERSION,
        encryption: ENCRYPTION_NONE.to_string(),
        created_at: Utc::now().to_rfc3339(),
        app_version: app_version.to_string(),
        source_device: source_device(),
        config: ConfigFileMeta {
            size: config_bytes.len() as u64,
            sha256: sha256_hex(config_bytes),
        },
        counts,
    }
}

/// Everything that must hold before a downloaded `config.json` is parsed: the
/// format is one we can read, and the bytes are the ones the writer finished
/// writing.
pub fn validate_manifest(
    manifest: &ConfigManifest,
    config_bytes: &[u8],
) -> Result<(), AppCommandError> {
    reject_newer_schema(manifest.schema_version)?;

    if manifest.encryption != ENCRYPTION_NONE {
        return Err(AppCommandError::invalid_input(format!(
            "Unsupported snapshot encryption '{}'",
            manifest.encryption
        ))
        .with_i18n(CONFIG_SYNC_I18N_KEY_INVALID_SNAPSHOT, BTreeMap::new()));
    }

    if manifest.config.size != config_bytes.len() as u64 {
        return Err(checksum_error());
    }
    if !manifest
        .config
        .sha256
        .eq_ignore_ascii_case(&sha256_hex(config_bytes))
    {
        return Err(checksum_error());
    }
    Ok(())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn checksum_error() -> AppCommandError {
    AppCommandError::invalid_input("Config snapshot failed its checksum")
        .with_i18n(CONFIG_SYNC_I18N_KEY_CHECKSUM, BTreeMap::new())
}

fn reject_newer_schema(schema_version: u32) -> Result<(), AppCommandError> {
    if schema_version > SCHEMA_VERSION {
        let mut params = BTreeMap::new();
        params.insert("snapshotVersion".to_string(), schema_version.to_string());
        params.insert("appVersion".to_string(), SCHEMA_VERSION.to_string());
        return Err(AppCommandError::invalid_input(
            "Config snapshot was written by a newer version of codeg",
        )
        .with_i18n(CONFIG_SYNC_I18N_KEY_NEWER_SCHEMA, params));
    }
    Ok(())
}

/// Best-effort machine name. Deliberately env-only: a hostname lookup would
/// mean a new dependency for a label that is purely informational.
fn source_device() -> String {
    for key in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim().to_string();
            if !value.is_empty() {
                return value;
            }
        }
    }
    "unknown".to_string()
}

/// Where pre-apply rollback snapshots live.
pub fn rollback_dir() -> PathBuf {
    crate::paths::codeg_home_dir().join("config-snapshots")
}

/// Write "what this machine looked like before the apply" next to the app's
/// own data, then prune to [`ROLLBACK_KEEP`]. Synchronous: the payload is tens
/// of KB, so the blocking write is shorter than the cost of a thread hop.
///
/// Failure is reported to the caller but must never be treated as fatal by it
/// — losing the safety net is worse than not having one, but not as bad as
/// refusing to apply a configuration the user explicitly asked for.
pub fn write_rollback_snapshot(
    dir: &Path,
    snapshot: &ConfigSnapshot,
) -> Result<PathBuf, AppCommandError> {
    std::fs::create_dir_all(dir).map_err(AppCommandError::io)?;
    let bytes = serialize_snapshot(snapshot)?;
    // Sortable, second-resolution + a millisecond suffix so two applies in the
    // same second do not collide.
    let name = format!("config-{}.json", Utc::now().format("%Y%m%dT%H%M%S%3f"));
    let path = dir.join(name);
    std::fs::write(&path, &bytes).map_err(AppCommandError::io)?;
    prune_rollback_snapshots(dir, ROLLBACK_KEEP);
    Ok(path)
}

/// Newest first.
pub fn list_rollback_snapshots(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("config-") && n.ends_with(".json"))
        })
        .collect();
    // Names are timestamp-ordered, so lexicographic order IS chronological
    // order — no filesystem mtime involved, which keeps this stable across
    // copies and restores.
    files.sort();
    files.reverse();
    files
}

fn prune_rollback_snapshots(dir: &Path, keep: usize) {
    for path in list_rollback_snapshots(dir).into_iter().skip(keep) {
        if let Err(err) = std::fs::remove_file(&path) {
            tracing::warn!("[CONFIG-SYNC] failed to prune rollback snapshot: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::entities::{agent_setting, custom_agent, model_provider, quick_message};
    use crate::db::service::app_metadata_service;
    use crate::db::test_helpers::fresh_in_memory_db;
    use sea_orm::{ActiveModelTrait, ActiveValue::NotSet, EntityTrait, Set};

    async fn seed_source(conn: &DatabaseConnection) {
        let now = Utc::now();
        let provider = model_provider::ActiveModel {
            id: NotSet,
            name: Set("DeepSeek".to_string()),
            api_url: Set("https://api.deepseek.com".to_string()),
            api_key: Set("sk-secret".to_string()),
            agent_types_json: Set("[\"deepseek\"]".to_string()),
            agent_type: Set("deepseek".to_string()),
            model: Set(Some("deepseek-chat".to_string())),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(conn)
        .await
        .expect("insert provider");

        agent_setting::ActiveModel {
            id: NotSet,
            agent_type: Set("deepseek".to_string()),
            registry_id: Set("deepseek".to_string()),
            enabled: Set(true),
            sort_order: Set(3),
            installed_version: Set(Some("1.2.3-local".to_string())),
            env_json: Set(Some("{\"FOO\":\"bar\"}".to_string())),
            model_provider_id: Set(Some(provider.id)),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(conn)
        .await
        .expect("insert agent setting");

        quick_message::ActiveModel {
            id: NotSet,
            title: Set("Review".to_string()),
            content: Set("Please review this diff".to_string()),
            sort_order: Set(1),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(conn)
        .await
        .expect("insert quick message");

        app_metadata_service::upsert_value(conn, "appearance_mode", "dark")
            .await
            .expect("portable pref");
        app_metadata_service::upsert_value(conn, "web_service_token", "device-local-token")
            .await
            .expect("device-local pref");
    }

    #[tokio::test]
    async fn snapshot_round_trips_into_a_second_database() {
        let source = fresh_in_memory_db().await;
        seed_source(&source.conn).await;

        let snapshot = collect_snapshot_core(&source.conn).await.expect("collect");
        let bytes = serialize_snapshot(&snapshot).expect("serialize");
        let parsed = parse_snapshot(&bytes).expect("parse");

        let target = fresh_in_memory_db().await;
        let report = apply_snapshot_core(&target.conn, &parsed)
            .await
            .expect("apply");
        assert!(report.total >= 4, "unexpected apply report: {report:?}");

        let providers = model_provider::Entity::find()
            .all(&target.conn)
            .await
            .expect("providers");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].api_key, "sk-secret");

        let settings = agent_setting::Entity::find()
            .all(&target.conn)
            .await
            .expect("settings");
        assert_eq!(settings.len(), 1);
        // The provider reference was remapped to the TARGET machine's row id.
        assert_eq!(settings[0].model_provider_id, Some(providers[0].id));
        // Device-local: the source machine's installed CLI version must not
        // travel, or the target would skip its own install/upgrade prompt.
        assert_eq!(settings[0].installed_version, None);

        let messages = quick_message::Entity::find()
            .all(&target.conn)
            .await
            .expect("quick messages");
        assert_eq!(messages.len(), 1);

        assert_eq!(
            app_metadata_service::get_value(&target.conn, "appearance_mode")
                .await
                .expect("pref"),
            Some("dark".to_string())
        );
    }

    #[tokio::test]
    async fn device_local_preferences_never_enter_a_snapshot() {
        let source = fresh_in_memory_db().await;
        seed_source(&source.conn).await;

        let snapshot = collect_snapshot_core(&source.conn).await.expect("collect");
        let prefs = snapshot.domains.get("preferences").expect("preferences");
        assert!(prefs.get("appearance_mode").is_some());
        assert!(
            prefs.get("web_service_token").is_none(),
            "device-local key leaked into the snapshot: {prefs}"
        );
    }

    /// A hand-edited or hostile snapshot must not be able to write a key the
    /// allowlist excludes — the collect-side filter is not the only guard.
    #[tokio::test]
    async fn applying_a_doctored_preference_key_is_ignored() {
        let target = fresh_in_memory_db().await;
        let snapshot = ConfigSnapshot {
            schema_version: SCHEMA_VERSION,
            domains: BTreeMap::from([(
                "preferences".to_string(),
                serde_json::json!({
                    "appearance_mode": "light",
                    "web_service_token": "stolen",
                    "config_sync_settings": "{}"
                }),
            )]),
        };

        apply_snapshot_core(&target.conn, &snapshot)
            .await
            .expect("apply");

        assert_eq!(
            app_metadata_service::get_value(&target.conn, "appearance_mode")
                .await
                .expect("pref"),
            Some("light".to_string())
        );
        for key in ["web_service_token", "config_sync_settings"] {
            assert_eq!(
                app_metadata_service::get_value(&target.conn, key)
                    .await
                    .expect("pref"),
                None,
                "{key} must not be writable from a snapshot"
            );
        }
    }

    /// Re-applying the same snapshot must converge, not duplicate: the whole
    /// point of natural-key upserts.
    #[tokio::test]
    async fn applying_twice_updates_instead_of_duplicating() {
        let source = fresh_in_memory_db().await;
        seed_source(&source.conn).await;
        let snapshot = collect_snapshot_core(&source.conn).await.expect("collect");

        let target = fresh_in_memory_db().await;
        apply_snapshot_core(&target.conn, &snapshot)
            .await
            .expect("first apply");
        apply_snapshot_core(&target.conn, &snapshot)
            .await
            .expect("second apply");

        assert_eq!(
            model_provider::Entity::find()
                .all(&target.conn)
                .await
                .expect("providers")
                .len(),
            1
        );
        assert_eq!(
            quick_message::Entity::find()
                .all(&target.conn)
                .await
                .expect("messages")
                .len(),
            1
        );
    }

    /// Rows the snapshot does not mention are the user's local work; an apply
    /// is an upsert, never a mirror.
    #[tokio::test]
    async fn local_only_rows_survive_an_apply() {
        let target = fresh_in_memory_db().await;
        let now = Utc::now();
        quick_message::ActiveModel {
            id: NotSet,
            title: Set("Local only".to_string()),
            content: Set("keep me".to_string()),
            sort_order: Set(9),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&target.conn)
        .await
        .expect("seed local");

        let source = fresh_in_memory_db().await;
        seed_source(&source.conn).await;
        let snapshot = collect_snapshot_core(&source.conn).await.expect("collect");
        apply_snapshot_core(&target.conn, &snapshot)
            .await
            .expect("apply");

        let titles: Vec<String> = quick_message::Entity::find()
            .all(&target.conn)
            .await
            .expect("messages")
            .into_iter()
            .map(|m| m.title)
            .collect();
        assert!(titles.contains(&"Local only".to_string()));
        assert!(titles.contains(&"Review".to_string()));
    }

    /// `skills_dir` is an absolute path on the machine that owns it.
    #[tokio::test]
    async fn custom_agent_skills_dir_stays_local() {
        let target = fresh_in_memory_db().await;
        let now = Utc::now();
        custom_agent::ActiveModel {
            id: NotSet,
            registry_id: Set("acme".to_string()),
            name: Set("Acme".to_string()),
            description: Set(String::new()),
            version: Set("1.0.0".to_string()),
            distribution_kind: Set("npm".to_string()),
            spec_json: Set("{}".to_string()),
            icon_url: Set(None),
            skills_shared_store: Set(false),
            skills_dir: Set(Some("D:/local/skills".to_string())),
            source: Set("user".to_string()),
            version_probe: Set(None),
            supports_mcp: Set(false),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(&target.conn)
        .await
        .expect("seed agent");

        let snapshot = ConfigSnapshot {
            schema_version: SCHEMA_VERSION,
            domains: BTreeMap::from([(
                "customAgents".to_string(),
                serde_json::json!([{
                    "registryId": "acme",
                    "name": "Acme Renamed",
                    "version": "2.0.0",
                    "distributionKind": "npm",
                    "specJson": "{}",
                    "skillsDir": "/somewhere/else"
                }]),
            )]),
        };
        apply_snapshot_core(&target.conn, &snapshot)
            .await
            .expect("apply");

        let row = custom_agent::Entity::find()
            .all(&target.conn)
            .await
            .expect("agents")
            .remove(0);
        assert_eq!(row.name, "Acme Renamed");
        assert_eq!(row.skills_dir, Some("D:/local/skills".to_string()));
    }

    #[test]
    fn manifest_validates_matching_bytes_and_rejects_tampering() {
        let snapshot = ConfigSnapshot {
            schema_version: SCHEMA_VERSION,
            domains: BTreeMap::from([("preferences".to_string(), serde_json::json!({}))]),
        };
        let bytes = serialize_snapshot(&snapshot).expect("serialize");
        let manifest = build_manifest(&bytes, "1.0.0", snapshot.counts());

        validate_manifest(&manifest, &bytes).expect("matching bytes validate");

        let mut tampered = bytes.clone();
        tampered.extend_from_slice(b"\n");
        let err = validate_manifest(&manifest, &tampered).expect_err("must reject");
        assert_eq!(
            err.i18n_key.as_deref(),
            Some(CONFIG_SYNC_I18N_KEY_CHECKSUM)
        );
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_guessed_at() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": SCHEMA_VERSION + 1,
            "domains": {}
        }))
        .expect("bytes");
        let err = parse_snapshot(&bytes).expect_err("must refuse");
        assert_eq!(
            err.i18n_key.as_deref(),
            Some(CONFIG_SYNC_I18N_KEY_NEWER_SCHEMA)
        );
    }

    #[test]
    fn rollback_snapshots_are_pruned_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let snapshot = ConfigSnapshot {
            schema_version: SCHEMA_VERSION,
            domains: BTreeMap::new(),
        };
        for index in 0..(ROLLBACK_KEEP + 3) {
            // Distinct, monotonically increasing names without waiting on the
            // wall clock.
            let path = dir
                .path()
                .join(format!("config-20240101T00000{index:03}.json"));
            std::fs::create_dir_all(dir.path()).expect("dir");
            std::fs::write(&path, serialize_snapshot(&snapshot).expect("bytes")).expect("write");
        }
        prune_rollback_snapshots(dir.path(), ROLLBACK_KEEP);
        let remaining = list_rollback_snapshots(dir.path());
        assert_eq!(remaining.len(), ROLLBACK_KEEP);
        // Newest first, and the pruned ones are the oldest.
        assert!(remaining[0] > remaining[remaining.len() - 1]);
    }
}
