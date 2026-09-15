//! Configuration sync: a small, config-only snapshot of this machine's
//! settings that can be exported to a file or pushed to the user's own WebDAV
//! share, and pulled back on another machine.
//!
//! Deliberately NOT the backup engine (`commands::backup`): that one packs
//! conversations, uploads, and transcripts into an encrypted archive measured
//! in gigabytes, which is the wrong unit for something that runs on a timer.
//! This snapshot is tens of KB of configuration and nothing else.
//!
//! ## Shape of the feature
//!
//! * [`domains`] — the single table of what a snapshot contains.
//! * [`portable_keys`] — which `app_metadata` preferences may travel.
//! * [`snapshot`] — collect / validate / apply, plus local rollback copies.
//! * [`local_io`] — single-file export and import.
//! * [`webdav_sync`] — settings, remote layout, upload/download.
//! * [`auto_sync`] — the periodic hash-compare uploader and its suppression.
//!
//! ## Two rules that shape everything else
//!
//! **Uploads are automatic; downloads never are.** A timer that pulled would
//! be indistinguishable from another machine silently overwriting local
//! settings, so every download is an explicit user action with a confirmation
//! that shows what is about to change.
//!
//! **The snapshot is plaintext.** The security boundary is the user's own
//! authenticated WebDAV endpoint. API keys therefore travel in the clear and
//! the sync's own credentials are excluded from the snapshot entirely — a
//! machine can never overwrite another machine's credentials, which is what
//! would turn two clients into a sync loop.
//!
//! Layering mirrors the backup engine: `*_core` functions take plain
//! references (`&DatabaseConnection`, `&EventEmitter`) so desktop commands,
//! the (future) Axum handlers, and the background scheduler share one
//! implementation.

pub mod auto_sync;
pub mod domains;
pub mod local_io;
pub mod portable_keys;
pub mod snapshot;
pub mod webdav_sync;

// ─── Desktop Tauri commands ──────────────────────────────────────────────
//
// Thin wrappers only. The frontend picks paths with the native file dialog
// and passes them in, exactly as the backup commands do.

#[cfg(feature = "tauri-runtime")]
mod tauri_commands {
    use std::path::Path;

    use tauri::State;

    use crate::app_error::AppCommandError;
    use crate::db::AppDatabase;

    use super::local_io::{
        export_to_file_core, import_from_file_core, peek_import_core, ConfigExportSummary,
        ConfigImportPreview, ConfigImportResult,
    };
    use super::snapshot::ConfigManifest;
    use super::webdav_sync::{
        download_and_apply_core, load_settings, load_state, merge_settings, peek_remote_core,
        save_settings_core, test_connection_core, upload_snapshot_core, ConfigSyncSettingsInput,
        ConfigSyncSettingsView, ConfigSyncState, DownloadOutcome, UploadOutcome,
    };

    const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

    #[tauri::command]
    pub async fn config_sync_export_file(
        dest_path: String,
        db: State<'_, AppDatabase>,
    ) -> Result<ConfigExportSummary, AppCommandError> {
        export_to_file_core(&db.conn, APP_VERSION, Path::new(&dest_path)).await
    }

    /// Read-only: powers the "this file contains…" confirmation before any
    /// local data is touched.
    #[tauri::command]
    pub async fn config_sync_peek_file(
        src_path: String,
    ) -> Result<ConfigImportPreview, AppCommandError> {
        peek_import_core(Path::new(&src_path))
    }

    /// Intentionally NOT suppressed: a configuration the user imported by hand
    /// should propagate to their other machines like any other local change.
    #[tauri::command]
    pub async fn config_sync_import_file(
        src_path: String,
        db: State<'_, AppDatabase>,
    ) -> Result<ConfigImportResult, AppCommandError> {
        import_from_file_core(&db.conn, Path::new(&src_path)).await
    }

    #[tauri::command]
    pub async fn config_sync_get_settings(
        db: State<'_, AppDatabase>,
    ) -> Result<ConfigSyncSettingsView, AppCommandError> {
        Ok(ConfigSyncSettingsView::from(&load_settings(&db.conn).await))
    }

    #[tauri::command]
    pub async fn config_sync_update_settings(
        settings: ConfigSyncSettingsInput,
        db: State<'_, AppDatabase>,
    ) -> Result<ConfigSyncSettingsView, AppCommandError> {
        save_settings_core(&db.conn, settings).await
    }

    #[tauri::command]
    pub async fn config_sync_get_state(
        db: State<'_, AppDatabase>,
    ) -> Result<ConfigSyncState, AppCommandError> {
        Ok(load_state(&db.conn).await)
    }

    /// Tests what the form currently shows WITHOUT saving it, so a user can
    /// verify credentials before committing them. An empty password field
    /// falls back to the stored one, same as saving would.
    #[tauri::command]
    pub async fn config_sync_test_connection(
        settings: ConfigSyncSettingsInput,
        db: State<'_, AppDatabase>,
    ) -> Result<(), AppCommandError> {
        let existing = load_settings(&db.conn).await;
        let candidate = merge_settings(&existing, settings)?;
        test_connection_core(&candidate).await
    }

    /// The manual "sync now" button: uploads even when the hash says nothing
    /// changed, because the user pressing it usually means they suspect the
    /// remote is out of date.
    #[tauri::command]
    pub async fn config_sync_upload_now(
        db: State<'_, AppDatabase>,
    ) -> Result<UploadOutcome, AppCommandError> {
        upload_snapshot_core(&db.conn, APP_VERSION, true).await
    }

    #[tauri::command]
    pub async fn config_sync_peek_remote(
        db: State<'_, AppDatabase>,
    ) -> Result<Option<ConfigManifest>, AppCommandError> {
        peek_remote_core(&db.conn).await
    }

    #[tauri::command]
    pub async fn config_sync_download_apply(
        db: State<'_, AppDatabase>,
    ) -> Result<DownloadOutcome, AppCommandError> {
        download_and_apply_core(&db.conn).await
    }
}

#[cfg(feature = "tauri-runtime")]
pub use tauri_commands::*;
