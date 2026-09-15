# Config Sync Contract

> `src-tauri/src/commands/config_sync/` + `src-tauri/src/network/webdav.rs`

---

## 1. Scope / Trigger

Code-spec depth is mandatory here: the feature adds new commands, a new
cross-layer request/response contract, writes to every configuration table,
and integrates external storage (WebDAV) plus stored credentials.

Reach for this spec when you change a config table, add a preference that
should follow the user across machines, touch the snapshot format, or add a
WebDAV verb.

---

## 2. Signatures

### Commands (`#[tauri::command]`, desktop only; `_core` functions are shared)

| Command | Input | Output |
|---------|-------|--------|
| `config_sync_export_file` | `dest_path: String` | `ConfigExportSummary` |
| `config_sync_peek_file` | `src_path: String` | `ConfigImportPreview` |
| `config_sync_import_file` | `src_path: String` | `ConfigImportResult` |
| `config_sync_get_settings` | — | `ConfigSyncSettingsView` |
| `config_sync_update_settings` | `settings: ConfigSyncSettingsInput` | `ConfigSyncSettingsView` |
| `config_sync_get_state` | — | `ConfigSyncState` |
| `config_sync_test_connection` | `settings: ConfigSyncSettingsInput` | `()` |
| `config_sync_upload_now` | — | `UploadOutcome` |
| `config_sync_peek_remote` | — | `Option<ConfigManifest>` |
| `config_sync_download_apply` | — | `DownloadOutcome` |

`config_sync_test_connection` takes the *unsaved* form so credentials can be
verified before they are persisted.

### Remote layout

```
{remoteDir}/v{PROTOCOL_VERSION}/{profile}/config.json
{remoteDir}/v{PROTOCOL_VERSION}/{profile}/manifest.json
```

Write order is **config first, manifest second**. WebDAV has no multi-file
transaction; an interrupted upload must leave the OLD manifest pointing at
consistent bytes rather than a half-written snapshot.

### Snapshot domains (`domains.rs`)

`modelProviders`, `agentSettings`, `customAgents`, `quickMessages`,
`taskTemplates`, `preferences`.

---

## 3. Contracts

### `ConfigSyncSettingsInput` → `ConfigSyncSettingsView`

| Input field | Type | Note |
|-------------|------|------|
| `enabled` | `bool` | |
| `serverUrl` | `String` | |
| `username` | `String` | |
| `password` | `Option<String>` | **`null`/empty = keep the stored password** |
| `remoteDir` | `Option<String>` | path segments validated |
| `profile` | `Option<String>` | devices sharing a profile share one snapshot |
| `autoSync` | `bool` | |
| `intervalMinutes` | `u32` | |

The view never returns the password — only `hasPassword: bool`. There is no
`passwordTouched`-style flag: empty means keep, and that is the only mechanism.

### `preferences` allowlist

`portable_keys::PORTABLE_PREFERENCE_KEYS` is an **allowlist**, never an
exclusion list. A new `app_metadata` key does not travel until it is added
here, which is what keeps credentials and device-local state out by default.

---

## 4. Validation & Error Matrix

| Condition | Error | i18n key |
|-----------|-------|----------|
| snapshot schema newer than this build | `invalid_input` | `configSync.error.newerSchema` |
| `sha256(config.json)` ≠ manifest checksum | `invalid_input` | `configSync.error.checksum` |
| payload is not a codeg snapshot / manifest | `invalid_input` | `configSync.error.invalidSnapshot` |
| no snapshot uploaded yet | `invalid_input` | `configSync.error.noRemoteSnapshot` |
| HTTP 401 | `network` | `configSync.error.unauthorized` |
| HTTP 403 | `network` | `configSync.error.forbidden` |
| 404/409 on write, directory unrecoverable | `network` | `configSync.error.remotePath` |
| HTTP 507 / 413 | `network` | `configSync.error.quota` |
| transport failure, oversized response | `network` | `configSync.error.network` |
| other 5xx | `network` (param `status`) | `configSync.error.server` |
| unparseable server URL | `invalid_input` | `configSync.error.remotePath` |
| `remoteDir` / `profile` contains `/`, `..`, control chars | `invalid_input` | — |

Everything reaching the network maps through `WebdavError → AppCommandError`
in `network/webdav.rs`; only URL construction and snapshot validation raise
`invalid_input`.

`PROPFIND` returning **404 on a directory that does not exist yet is not an
error** for *test connection*: the directory is created on first upload.

---

## 5. Good / Base / Bad Cases

- **Good**: user edits settings, leaves password blank, saves → stored password
  survives; `enabled=false` stops the timer but keeps credentials.
- **Base**: timer fires, snapshot hash equals `lastUploadedSha256` → no upload, no
  network call. Manual "sync now" uploads *regardless* of the hash.
- **Bad**: clearing config tables before applying a snapshot. Rows are upserted
  by natural key; truncating renumbers autoincrement IDs and breaks
  `agent_setting.model_provider_id`. A row deleted on machine A therefore does
  not disappear on machine B — a documented trade-off, stated in the UI.

---

## 6. Tests Required

| Area | Assertion point |
|------|-----------------|
| allowlist | intersection of `PORTABLE_PREFERENCE_KEYS` and sync-credential keys is empty |
| snapshot | collect → serialize → parse → apply is idempotent; counts match |
| checksum | flipping one byte of `config.json` makes `validate_manifest` fail |
| path join | base URL with and without a trailing slash produce the same path (no `//`) |
| settings merge | empty password keeps the old one; non-empty replaces it |
| auto sync | unchanged hash performs zero requests; manual upload still uploads |
| frontend | empty password field sends `null`, not `""` |

---

## 7. Wrong vs Correct

### Wrong — create the remote directory in one call

```rust
client.mkcol("/codeg/v1/default").await?; // assumes intermediate levels exist
```

WebDAV `MKCOL` only creates the leaf: RFC 4918 requires a 409 when an
intermediate collection is missing. Live run against Jianguoyun shows the
per-level calls each returning 201, i.e. nothing above the leaf pre-exists.

### Correct — create every level, tolerating "already exists"

```rust
for prefix in ["/codeg", "/codeg/v1", "/codeg/v1/default"] {
    client.mkcol(prefix).await?; // 201 created, 405 already there — both fine
}
```

### Wrong — let a timer download

```rust
if remote_is_newer { apply_snapshot(remote).await?; } // silent overwrite
```

### Correct — uploads are automatic, downloads never are

```rust
// peek_remote_core() feeds a confirmation dialog;
// download_and_apply_core() only runs after the user confirms.
```
