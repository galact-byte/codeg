//! Sync settings, remote layout, and the upload/download choreography.
//!
//! ## Remote layout
//!
//! `{remoteDir}/v{PROTOCOL_VERSION}/{profile}/{config.json,manifest.json}`
//!
//! The `v1` level means a future incompatible protocol can land beside this
//! one instead of on top of it, and `{profile}` lets one share hold several
//! independent configurations (work vs personal) without extra accounts.
//!
//! ## Upload order is load-bearing
//!
//! `config.json` first, `manifest.json` second. WebDAV has no multi-file
//! transaction, so an interrupted upload leaves the OLD manifest pointing at
//! the NEW config — and the downloader's checksum check rejects that pair
//! instead of applying a half-written configuration. Writing the manifest
//! first would invert this into "looks valid, is truncated".

use std::collections::BTreeMap;
use std::sync::OnceLock;

use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::snapshot::{
    apply_snapshot_core, build_manifest, collect_snapshot_core, parse_manifest, parse_snapshot,
    serialize_snapshot, sha256_hex, validate_manifest, ApplyReport, ConfigManifest,
    CONFIG_FILE_NAME, MANIFEST_FILE_NAME,
};
use crate::app_error::{AppCommandError, CONFIG_SYNC_I18N_KEY_NO_REMOTE};
use crate::db::service::app_metadata_service;
use crate::network::webdav::{sanitize_path_segment, WebdavClient};

/// Credentials of this feature. NOT in `portable_keys`: if it travelled, one
/// machine's credentials would overwrite the other's and the two would sync
/// into each other in a loop.
pub const CONFIG_SYNC_SETTINGS_KEY: &str = "config_sync_settings";
/// Last-uploaded hash and last result. Device-local by nature.
pub const CONFIG_SYNC_STATE_KEY: &str = "config_sync_state";

/// Remote layout version, independent of the snapshot's `schemaVersion`.
pub const PROTOCOL_VERSION: u32 = 1;

pub const DEFAULT_REMOTE_DIR: &str = "codeg";
pub const DEFAULT_PROFILE: &str = "default";
pub const DEFAULT_INTERVAL_MINUTES: u32 = 5;
/// A day. Not a real limit, just a guard against a value that would overflow
/// the backoff multiplier or park the timer past the heat death of the laptop.
const MAX_INTERVAL_MINUTES: u32 = 1440;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigSyncSettings {
    pub enabled: bool,
    pub server_url: String,
    pub username: String,
    /// Stored as given. See the module docs of `mod.rs` for why the snapshot
    /// itself stays unencrypted; this value never leaves the local database.
    pub password: String,
    pub remote_dir: String,
    pub profile: String,
    pub auto_sync: bool,
    pub interval_minutes: u32,
}

impl Default for ConfigSyncSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            server_url: String::new(),
            username: String::new(),
            password: String::new(),
            remote_dir: DEFAULT_REMOTE_DIR.to_string(),
            profile: DEFAULT_PROFILE.to_string(),
            auto_sync: true,
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
        }
    }
}

/// What the frontend sees. The password is replaced by "is one stored", so a
/// compromised renderer cannot read it back and the settings form has nothing
/// to accidentally re-submit.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSyncSettingsView {
    pub enabled: bool,
    pub server_url: String,
    pub username: String,
    pub has_password: bool,
    pub remote_dir: String,
    pub profile: String,
    pub auto_sync: bool,
    pub interval_minutes: u32,
}

impl From<&ConfigSyncSettings> for ConfigSyncSettingsView {
    fn from(settings: &ConfigSyncSettings) -> Self {
        Self {
            enabled: settings.enabled,
            server_url: settings.server_url.clone(),
            username: settings.username.clone(),
            has_password: !settings.password.is_empty(),
            remote_dir: settings.remote_dir.clone(),
            profile: settings.profile.clone(),
            auto_sync: settings.auto_sync,
            interval_minutes: settings.interval_minutes,
        }
    }
}

/// Save payload. `password: None` (or empty) means "keep what is stored" —
/// the ONLY password mechanism, deliberately.
///
/// The alternative, rendering a masked placeholder into the password field,
/// has a known failure mode: the form submits the mask verbatim and the mask
/// becomes the password, so the next sync fails to authenticate. There is also
/// no separate `passwordTouched` flag, because a flag plus a value is two
/// sources of truth that disagree exactly when the user clears the field.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSyncSettingsInput {
    pub enabled: bool,
    pub server_url: String,
    pub username: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub remote_dir: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
    pub auto_sync: bool,
    pub interval_minutes: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigSyncState {
    /// Hash of the last snapshot successfully uploaded. Persisted so a restart
    /// does not re-upload an unchanged configuration just to rebuild an
    /// in-memory baseline.
    pub last_uploaded_sha256: Option<String>,
    pub last_sync_at: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadOutcome {
    /// `false` means the snapshot was byte-identical to the last upload and
    /// nothing was sent.
    pub uploaded: bool,
    pub sha256: String,
    pub counts: BTreeMap<String, usize>,
    pub synced_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadOutcome {
    pub manifest: ConfigManifest,
    pub applied: ApplyReport,
    pub rollback_path: Option<String>,
}

/// Every remote read/write funnels through here. An upload is two PUTs; two
/// concurrent uploads would interleave into a manifest from one snapshot and a
/// config from another.
fn remote_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

// ─── settings persistence ─────────────────────────────────────────────

/// Never fails: a row this build cannot parse degrades to defaults (sync off)
/// rather than breaking the settings page.
pub async fn load_settings(conn: &DatabaseConnection) -> ConfigSyncSettings {
    let raw = match app_metadata_service::get_value(conn, CONFIG_SYNC_SETTINGS_KEY).await {
        Ok(Some(raw)) => raw,
        Ok(None) => return ConfigSyncSettings::default(),
        Err(err) => {
            tracing::warn!("[CONFIG-SYNC] failed to read sync settings: {err}");
            return ConfigSyncSettings::default();
        }
    };
    match serde_json::from_str::<ConfigSyncSettings>(&raw) {
        Ok(settings) => settings,
        Err(err) => {
            tracing::warn!("[CONFIG-SYNC] failed to parse sync settings: {err}");
            ConfigSyncSettings::default()
        }
    }
}

pub async fn save_settings_core(
    conn: &DatabaseConnection,
    input: ConfigSyncSettingsInput,
) -> Result<ConfigSyncSettingsView, AppCommandError> {
    let existing = load_settings(conn).await;
    let merged = merge_settings(&existing, input)?;

    let serialized = serde_json::to_string(&merged).map_err(|e| {
        AppCommandError::invalid_input("Failed to serialize config sync settings")
            .with_detail(e.to_string())
    })?;
    app_metadata_service::upsert_value(conn, CONFIG_SYNC_SETTINGS_KEY, &serialized)
        .await
        .map_err(AppCommandError::db)?;

    Ok(ConfigSyncSettingsView::from(&merged))
}

/// Pure so the password-retention and path-validation rules are testable
/// without a database.
pub fn merge_settings(
    existing: &ConfigSyncSettings,
    input: ConfigSyncSettingsInput,
) -> Result<ConfigSyncSettings, AppCommandError> {
    let password = match input.password {
        Some(value) if !value.is_empty() => value,
        // Both `None` and `Some("")` keep the stored password. An empty field
        // means "I did not retype it", which is what an empty password field
        // means to every user who has ever seen one.
        _ => existing.password.clone(),
    };

    let remote_dir = normalize_segment(input.remote_dir, &existing.remote_dir, DEFAULT_REMOTE_DIR)?;
    let profile = normalize_segment(input.profile, &existing.profile, DEFAULT_PROFILE)?;

    Ok(ConfigSyncSettings {
        enabled: input.enabled,
        server_url: input.server_url.trim().to_string(),
        username: input.username.trim().to_string(),
        password,
        remote_dir,
        profile,
        auto_sync: input.auto_sync,
        interval_minutes: input
            .interval_minutes
            .clamp(1, MAX_INTERVAL_MINUTES),
    })
}

fn normalize_segment(
    incoming: Option<String>,
    existing: &str,
    fallback: &str,
) -> Result<String, AppCommandError> {
    let candidate = incoming.unwrap_or_else(|| existing.to_string());
    let candidate = if candidate.trim().is_empty() {
        fallback.to_string()
    } else {
        candidate
    };
    sanitize_path_segment(&candidate).ok_or_else(|| {
        AppCommandError::invalid_input(format!("Invalid remote path segment: {candidate}"))
            .with_i18n(
                crate::app_error::CONFIG_SYNC_I18N_KEY_REMOTE_PATH,
                BTreeMap::new(),
            )
    })
}

pub async fn load_state(conn: &DatabaseConnection) -> ConfigSyncState {
    match app_metadata_service::get_value(conn, CONFIG_SYNC_STATE_KEY).await {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_default(),
        _ => ConfigSyncState::default(),
    }
}

pub async fn save_state(conn: &DatabaseConnection, state: &ConfigSyncState) {
    let Ok(serialized) = serde_json::to_string(state) else {
        return;
    };
    if let Err(err) =
        app_metadata_service::upsert_value(conn, CONFIG_SYNC_STATE_KEY, &serialized).await
    {
        // Losing the hash baseline costs one redundant upload, not data.
        tracing::warn!("[CONFIG-SYNC] failed to persist sync state: {err}");
    }
}

// ─── remote paths ─────────────────────────────────────────────────────

/// `{remoteDir}/v1/{profile}` — the directory the two files live in.
pub fn remote_dir_path(settings: &ConfigSyncSettings) -> Result<String, AppCommandError> {
    let dir = normalize_segment(Some(settings.remote_dir.clone()), DEFAULT_REMOTE_DIR, DEFAULT_REMOTE_DIR)?;
    let profile = normalize_segment(Some(settings.profile.clone()), DEFAULT_PROFILE, DEFAULT_PROFILE)?;
    Ok(format!("{dir}/v{PROTOCOL_VERSION}/{profile}"))
}

fn client_for(settings: &ConfigSyncSettings) -> Result<WebdavClient, AppCommandError> {
    WebdavClient::new(&settings.server_url, &settings.username, &settings.password)
        .map_err(AppCommandError::from)
}

// ─── operations ───────────────────────────────────────────────────────

/// Credentials + reachability, without writing anything.
pub async fn test_connection_core(settings: &ConfigSyncSettings) -> Result<(), AppCommandError> {
    let client = client_for(settings)?;
    let dir = remote_dir_path(settings)?;
    let _guard = remote_lock().lock().await;
    client.probe(&dir).await.map_err(AppCommandError::from)
}

/// Collect → hash → skip-if-unchanged → upload.
///
/// `force` bypasses only the hash comparison (the manual "sync now" button);
/// it never bypasses validation.
pub async fn upload_snapshot_core(
    conn: &DatabaseConnection,
    app_version: &str,
    force: bool,
) -> Result<UploadOutcome, AppCommandError> {
    let settings = load_settings(conn).await;
    let snapshot = collect_snapshot_core(conn).await?;
    let bytes = serialize_snapshot(&snapshot)?;
    let hash = sha256_hex(&bytes);
    let counts = snapshot.counts();

    let mut state = load_state(conn).await;
    if !force && state.last_uploaded_sha256.as_deref() == Some(hash.as_str()) {
        // The common case on a timer: nothing changed, so nothing is sent and
        // no request is made at all.
        return Ok(UploadOutcome {
            uploaded: false,
            sha256: hash,
            counts,
            synced_at: state.last_sync_at.clone().unwrap_or_default(),
        });
    }

    let client = client_for(&settings)?;
    let dir = remote_dir_path(&settings)?;
    let manifest = build_manifest(&bytes, app_version, counts.clone());
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|e| {
        AppCommandError::task_execution_failed("Serialize manifest").with_detail(e.to_string())
    })?;

    let result = async {
        let _guard = remote_lock().lock().await;
        client.ensure_dir(&dir).await?;
        client.put(&format!("{dir}/{CONFIG_FILE_NAME}"), bytes).await?;
        client
            .put(&format!("{dir}/{MANIFEST_FILE_NAME}"), manifest_bytes)
            .await?;
        Ok::<(), crate::network::webdav::WebdavError>(())
    }
    .await;

    let synced_at = chrono::Utc::now().to_rfc3339();
    match result {
        Ok(()) => {
            state.last_uploaded_sha256 = Some(hash.clone());
            state.last_sync_at = Some(synced_at.clone());
            state.last_error = None;
            save_state(conn, &state).await;
            Ok(UploadOutcome {
                uploaded: true,
                sha256: hash,
                counts,
                synced_at,
            })
        }
        Err(err) => {
            let app_error = AppCommandError::from(err);
            state.last_error = Some(app_error.message.clone());
            save_state(conn, &state).await;
            Err(app_error)
        }
    }
}

/// Fetch the remote pair, verify it, and apply it locally.
///
/// Always explicit: nothing here runs on a timer. Automatic download would
/// mean a machine silently overwriting local configuration with whatever
/// another machine last pushed.
pub async fn download_and_apply_core(
    conn: &DatabaseConnection,
) -> Result<DownloadOutcome, AppCommandError> {
    let settings = load_settings(conn).await;
    let client = client_for(&settings)?;
    let dir = remote_dir_path(&settings)?;

    let (manifest_bytes, config_bytes) = {
        let _guard = remote_lock().lock().await;
        let manifest_bytes = client
            .get(&format!("{dir}/{MANIFEST_FILE_NAME}"))
            .await
            .map_err(AppCommandError::from)?;
        let config_bytes = client
            .get(&format!("{dir}/{CONFIG_FILE_NAME}"))
            .await
            .map_err(AppCommandError::from)?;
        (manifest_bytes, config_bytes)
    };

    let (Some(manifest_bytes), Some(config_bytes)) = (manifest_bytes, config_bytes) else {
        return Err(
            AppCommandError::invalid_input("No config snapshot on the remote yet")
                .with_i18n(CONFIG_SYNC_I18N_KEY_NO_REMOTE, BTreeMap::new()),
        );
    };

    let manifest = parse_manifest(&manifest_bytes)?;
    // Checksum first: an interrupted upload must never reach the database.
    validate_manifest(&manifest, &config_bytes)?;
    let snapshot = parse_snapshot(&config_bytes)?;

    // Hold the suppression guard across the apply. Applying rewrites local
    // configuration, and an auto-sync tick landing mid-apply would push a
    // half-merged state straight back to the remote.
    let _suppression = super::auto_sync::suppress_auto_sync();
    let rollback_path = super::local_io::save_rollback(conn).await;
    let applied = apply_snapshot_core(conn, &snapshot).await?;

    Ok(DownloadOutcome {
        manifest,
        applied,
        rollback_path,
    })
}

/// Read the remote manifest without applying anything — powers "the remote has
/// a snapshot from DESKTOP-42, 3 providers, 2 hours ago".
pub async fn peek_remote_core(
    conn: &DatabaseConnection,
) -> Result<Option<ConfigManifest>, AppCommandError> {
    let settings = load_settings(conn).await;
    let client = client_for(&settings)?;
    let dir = remote_dir_path(&settings)?;

    let bytes = {
        let _guard = remote_lock().lock().await;
        client
            .get(&format!("{dir}/{MANIFEST_FILE_NAME}"))
            .await
            .map_err(AppCommandError::from)?
    };
    match bytes {
        Some(bytes) => Ok(Some(parse_manifest(&bytes)?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_helpers::fresh_in_memory_db;

    fn input() -> ConfigSyncSettingsInput {
        ConfigSyncSettingsInput {
            enabled: true,
            server_url: " https://dav.example.com/dav ".to_string(),
            username: " alice ".to_string(),
            password: Some("app-password".to_string()),
            remote_dir: Some("codeg".to_string()),
            profile: Some("work".to_string()),
            auto_sync: true,
            interval_minutes: 5,
        }
    }

    #[test]
    fn an_empty_password_field_keeps_the_stored_one() {
        let existing = ConfigSyncSettings {
            password: "stored".to_string(),
            ..Default::default()
        };

        for submitted in [None, Some(String::new())] {
            let merged = merge_settings(
                &existing,
                ConfigSyncSettingsInput {
                    password: submitted.clone(),
                    ..input()
                },
            )
            .expect("merge");
            assert_eq!(
                merged.password, "stored",
                "submitted {submitted:?} must not clear the password"
            );
        }

        let merged = merge_settings(&existing, input()).expect("merge");
        assert_eq!(merged.password, "app-password");
    }

    #[test]
    fn urls_and_usernames_are_trimmed_and_intervals_clamped() {
        let merged = merge_settings(
            &ConfigSyncSettings::default(),
            ConfigSyncSettingsInput {
                interval_minutes: 0,
                ..input()
            },
        )
        .expect("merge");
        assert_eq!(merged.server_url, "https://dav.example.com/dav");
        assert_eq!(merged.username, "alice");
        assert_eq!(merged.interval_minutes, 1);

        let merged = merge_settings(
            &ConfigSyncSettings::default(),
            ConfigSyncSettingsInput {
                interval_minutes: u32::MAX,
                ..input()
            },
        )
        .expect("merge");
        assert_eq!(merged.interval_minutes, MAX_INTERVAL_MINUTES);
    }

    #[test]
    fn traversal_in_a_path_segment_is_refused() {
        for bad in ["..", "a/b", "a\\b"] {
            let err = merge_settings(
                &ConfigSyncSettings::default(),
                ConfigSyncSettingsInput {
                    remote_dir: Some(bad.to_string()),
                    ..input()
                },
            )
            .expect_err("must reject");
            assert_eq!(
                err.i18n_key.as_deref(),
                Some(crate::app_error::CONFIG_SYNC_I18N_KEY_REMOTE_PATH)
            );
        }
    }

    #[test]
    fn blank_segments_fall_back_to_defaults() {
        let merged = merge_settings(
            &ConfigSyncSettings::default(),
            ConfigSyncSettingsInput {
                remote_dir: Some("  ".to_string()),
                profile: None,
                ..input()
            },
        )
        .expect("merge");
        assert_eq!(merged.remote_dir, DEFAULT_REMOTE_DIR);
        assert_eq!(merged.profile, DEFAULT_PROFILE);
    }

    #[test]
    fn remote_path_carries_the_protocol_version_and_profile() {
        let settings = ConfigSyncSettings {
            remote_dir: "backups".to_string(),
            profile: "work".to_string(),
            ..Default::default()
        };
        assert_eq!(remote_dir_path(&settings).expect("path"), "backups/v1/work");
    }

    #[tokio::test]
    async fn the_view_never_carries_the_password() {
        let db = fresh_in_memory_db().await;
        let view = save_settings_core(&db.conn, input()).await.expect("save");
        assert!(view.has_password);

        let serialized = serde_json::to_string(&view).expect("serialize");
        assert!(
            !serialized.contains("app-password"),
            "password leaked to the frontend: {serialized}"
        );

        // And it round-trips through the database untouched.
        let stored = load_settings(&db.conn).await;
        assert_eq!(stored.password, "app-password");
        assert_eq!(stored.profile, "work");
    }

    #[tokio::test]
    async fn sync_state_survives_a_reload() {
        let db = fresh_in_memory_db().await;
        let state = ConfigSyncState {
            last_uploaded_sha256: Some("abc".to_string()),
            last_sync_at: Some("2026-01-01T00:00:00Z".to_string()),
            last_error: None,
        };
        save_state(&db.conn, &state).await;
        let loaded = load_state(&db.conn).await;
        assert_eq!(loaded.last_uploaded_sha256.as_deref(), Some("abc"));
    }

    /// An unchanged configuration must not touch the network — this is what
    /// makes a 5-minute timer acceptable.
    #[tokio::test]
    async fn an_unchanged_snapshot_skips_the_upload_entirely() {
        let db = fresh_in_memory_db().await;
        let snapshot = collect_snapshot_core(&db.conn).await.expect("collect");
        let hash = sha256_hex(&serialize_snapshot(&snapshot).expect("bytes"));
        save_state(
            &db.conn,
            &ConfigSyncState {
                last_uploaded_sha256: Some(hash.clone()),
                last_sync_at: Some("2026-01-01T00:00:00Z".to_string()),
                last_error: None,
            },
        )
        .await;

        // No server is configured, so reaching the transport at all would
        // surface as an error rather than a skip.
        let outcome = upload_snapshot_core(&db.conn, "1.0.0", false)
            .await
            .expect("skip without network");
        assert!(!outcome.uploaded);
        assert_eq!(outcome.sha256, hash);
    }
}
