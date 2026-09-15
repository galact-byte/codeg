//! Export the local configuration to a single file, and import one back.
//!
//! The file is one self-contained JSON object — manifest and snapshot
//! together — because a user who picks "export" expects one file they can put
//! in a note-taking app or send to themselves, not a pair they must keep
//! together. The WebDAV path keeps them separate for a different reason (see
//! `snapshot.rs`: a two-file upload is how a half-finished transfer becomes
//! detectable), and the two formats deliberately share the same manifest type.
//!
//! Import also accepts a bare `config.json` — the exact file the sync writes
//! to WebDAV — so a user who fetches one out of their cloud drive's web UI can
//! feed it straight back in.
//!
//! The checksum is NOT enforced on import. It exists to catch a truncated
//! upload, which cannot happen to a local file the OS handed us whole; holding
//! a hand-edited export to a byte-exact hash would only punish the user for
//! reformatting their own file. Schema version, structure, and the preference
//! allowlist are still enforced.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};

use super::snapshot::{
    apply_snapshot_core, build_manifest, collect_snapshot_core, rollback_dir, serialize_snapshot,
    write_rollback_snapshot, ApplyReport, ConfigManifest, ConfigSnapshot,
};
use crate::app_error::{AppCommandError, CONFIG_SYNC_I18N_KEY_INVALID_SNAPSHOT};

/// Marker + version of the single-file export envelope.
pub const EXPORT_FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigExportFile {
    /// Presence of this field is what distinguishes an export envelope from a
    /// bare `config.json`.
    pub codeg_config_export: u32,
    pub manifest: ConfigManifest,
    pub config: ConfigSnapshot,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigExportSummary {
    pub path: String,
    pub counts: BTreeMap<String, usize>,
}

/// What the confirmation dialog shows before anything is written.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigImportPreview {
    pub manifest: ConfigManifest,
    pub counts: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigImportResult {
    pub applied: ApplyReport,
    /// Where the pre-import state was saved. `None` means the safety net could
    /// not be written — surfaced, but never a reason to refuse an import the
    /// user explicitly asked for.
    pub rollback_path: Option<String>,
}

pub async fn build_export_core(
    conn: &DatabaseConnection,
    app_version: &str,
) -> Result<ConfigExportFile, AppCommandError> {
    let snapshot = collect_snapshot_core(conn).await?;
    let bytes = serialize_snapshot(&snapshot)?;
    let manifest = build_manifest(&bytes, app_version, snapshot.counts());
    Ok(ConfigExportFile {
        codeg_config_export: EXPORT_FORMAT_VERSION,
        manifest,
        config: snapshot,
    })
}

pub async fn export_to_file_core(
    conn: &DatabaseConnection,
    app_version: &str,
    dest: &Path,
) -> Result<ConfigExportSummary, AppCommandError> {
    let export = build_export_core(conn, app_version).await?;
    let bytes = serde_json::to_vec_pretty(&export).map_err(|e| {
        AppCommandError::task_execution_failed("Serialize config export").with_detail(e.to_string())
    })?;

    if let Some(parent) = dest.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(AppCommandError::io)?;
        }
    }
    std::fs::write(dest, &bytes).map_err(AppCommandError::io)?;

    Ok(ConfigExportSummary {
        path: dest.to_string_lossy().to_string(),
        counts: export.config.counts(),
    })
}

/// Accepts either envelope shape. A bare `config.json` gets a synthesized
/// manifest so the preview dialog has something to show.
pub fn parse_export_bytes(bytes: &[u8]) -> Result<ConfigExportFile, AppCommandError> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
        AppCommandError::invalid_input("Not a codeg config file")
            .with_detail(e.to_string())
            .with_i18n(CONFIG_SYNC_I18N_KEY_INVALID_SNAPSHOT, BTreeMap::new())
    })?;

    if value.get("codegConfigExport").is_some() {
        let export: ConfigExportFile = serde_json::from_value(value).map_err(|e| {
            AppCommandError::invalid_input("Malformed codeg config export")
                .with_detail(e.to_string())
                .with_i18n(CONFIG_SYNC_I18N_KEY_INVALID_SNAPSHOT, BTreeMap::new())
        })?;
        // Reject a newer schema the same way the WebDAV path does, before any
        // of it reaches the database.
        let snapshot_bytes = serialize_snapshot(&export.config)?;
        super::snapshot::parse_snapshot(&snapshot_bytes)?;
        return Ok(export);
    }

    let snapshot = super::snapshot::parse_snapshot(bytes)?;
    let snapshot_bytes = serialize_snapshot(&snapshot)?;
    let manifest = build_manifest(&snapshot_bytes, "unknown", snapshot.counts());
    Ok(ConfigExportFile {
        codeg_config_export: EXPORT_FORMAT_VERSION,
        manifest,
        config: snapshot,
    })
}

pub fn read_export_file(path: &Path) -> Result<ConfigExportFile, AppCommandError> {
    let bytes = std::fs::read(path).map_err(AppCommandError::io)?;
    parse_export_bytes(&bytes)
}

/// Read and validate without touching the database — what the UI calls to
/// populate "this file contains N providers, M agents…".
pub fn peek_import_core(path: &Path) -> Result<ConfigImportPreview, AppCommandError> {
    let export = read_export_file(path)?;
    Ok(ConfigImportPreview {
        counts: export.config.counts(),
        manifest: export.manifest,
    })
}

pub async fn import_from_file_core(
    conn: &DatabaseConnection,
    path: &Path,
) -> Result<ConfigImportResult, AppCommandError> {
    // Parse before writing the rollback snapshot: a malformed file should cost
    // the user nothing at all.
    let export = read_export_file(path)?;
    let rollback_path = save_rollback(conn).await;
    let applied = apply_snapshot_core(conn, &export.config).await?;
    Ok(ConfigImportResult {
        applied,
        rollback_path,
    })
}

/// Capture "what this machine looked like before" so a surprising import is
/// undoable. Best effort by design — see [`ConfigImportResult::rollback_path`].
pub async fn save_rollback(conn: &DatabaseConnection) -> Option<String> {
    let snapshot = match collect_snapshot_core(conn).await {
        Ok(snapshot) => snapshot,
        Err(err) => {
            tracing::warn!("[CONFIG-SYNC] rollback snapshot not collected: {err}");
            return None;
        }
    };
    let dir: PathBuf = rollback_dir();
    match write_rollback_snapshot(&dir, &snapshot) {
        Ok(path) => Some(path.to_string_lossy().to_string()),
        Err(err) => {
            tracing::warn!("[CONFIG-SYNC] rollback snapshot not written: {err}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::snapshot::SCHEMA_VERSION;
    use super::*;
    use crate::db::entities::quick_message;
    use crate::db::test_helpers::fresh_in_memory_db;
    use sea_orm::{ActiveModelTrait, ActiveValue::NotSet, EntityTrait, Set};

    async fn seed_message(conn: &DatabaseConnection, title: &str) {
        let now = chrono::Utc::now();
        quick_message::ActiveModel {
            id: NotSet,
            title: Set(title.to_string()),
            content: Set("body".to_string()),
            sort_order: Set(0),
            created_at: Set(now),
            updated_at: Set(now),
        }
        .insert(conn)
        .await
        .expect("seed");
    }

    #[tokio::test]
    async fn exported_file_imports_into_another_machine() {
        let source = fresh_in_memory_db().await;
        seed_message(&source.conn, "Exported").await;

        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("nested").join("codeg-config.json");
        let summary = export_to_file_core(&source.conn, "9.9.9", &dest)
            .await
            .expect("export");
        assert!(dest.exists());
        assert_eq!(summary.counts.get("quickMessages"), Some(&1));

        let preview = peek_import_core(&dest).expect("peek");
        assert_eq!(preview.manifest.app_version, "9.9.9");
        assert_eq!(preview.counts.get("quickMessages"), Some(&1));

        let target = fresh_in_memory_db().await;
        let result = import_from_file_core(&target.conn, &dest)
            .await
            .expect("import");
        assert!(result.applied.total >= 1);
        assert_eq!(
            quick_message::Entity::find()
                .all(&target.conn)
                .await
                .expect("messages")
                .len(),
            1
        );
    }

    /// The file the WebDAV sync uploads must be importable as-is — a user who
    /// pulls `config.json` out of their cloud drive's web UI should not have
    /// to reshape it.
    #[tokio::test]
    async fn a_bare_remote_config_json_is_accepted() {
        let source = fresh_in_memory_db().await;
        seed_message(&source.conn, "Bare").await;
        let snapshot = collect_snapshot_core(&source.conn).await.expect("collect");
        let bytes = serialize_snapshot(&snapshot).expect("bytes");

        let export = parse_export_bytes(&bytes).expect("parse bare");
        assert_eq!(export.config.schema_version, SCHEMA_VERSION);
        assert_eq!(export.manifest.app_version, "unknown");
        assert_eq!(export.config.counts().get("quickMessages"), Some(&1));
    }

    /// A reformatted export (different indentation, reordered keys) must still
    /// import: the checksum guards transfers, not the user's text editor.
    #[tokio::test]
    async fn a_reformatted_export_still_imports() {
        let source = fresh_in_memory_db().await;
        seed_message(&source.conn, "Reformatted").await;
        let export = build_export_core(&source.conn, "1.0.0").await.expect("build");

        let compact = serde_json::to_vec(&export).expect("compact bytes");
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("compact.json");
        std::fs::write(&path, compact).expect("write");

        let target = fresh_in_memory_db().await;
        import_from_file_core(&target.conn, &path)
            .await
            .expect("import compact");
        assert_eq!(
            quick_message::Entity::find()
                .all(&target.conn)
                .await
                .expect("messages")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn junk_files_are_rejected_before_anything_is_touched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, b"just some notes").expect("write");

        let target = fresh_in_memory_db().await;
        let err = import_from_file_core(&target.conn, &path)
            .await
            .expect_err("must reject");
        assert_eq!(
            err.i18n_key.as_deref(),
            Some(CONFIG_SYNC_I18N_KEY_INVALID_SNAPSHOT)
        );
    }
}
