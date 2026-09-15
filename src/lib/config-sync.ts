/**
 * Frontend surface of configuration sync.
 *
 * Mirrors `src-tauri/src/commands/config_sync/`: a config-only snapshot
 * (providers, agent settings, custom agents, quick messages, task templates,
 * whitelisted preferences) that can be written to a single file or pushed to
 * the user's own WebDAV share.
 *
 * Kept out of `api.ts` on purpose — that module is already thousands of lines
 * and this feature has its own vocabulary.
 *
 * Desktop-only in this version: file paths come from native dialogs and the
 * commands are registered on the Tauri runtime, so the settings UI gates on
 * `isDesktop()` rather than degrading here.
 */

import { getTransport } from "./transport"

/** Stable domain ids, matching `domains::CONFIG_DOMAINS` in Rust. */
export const CONFIG_DOMAIN_IDS = [
  "modelProviders",
  "agentSettings",
  "customAgents",
  "quickMessages",
  "taskTemplates",
  "preferences",
] as const

export type ConfigDomainId = (typeof CONFIG_DOMAIN_IDS)[number]

/** Row counts per domain. A domain missing from an older snapshot is absent,
 *  not zero — the UI must treat `undefined` as "not present in this file". */
export type DomainCounts = Partial<Record<ConfigDomainId, number>> &
  Record<string, number>

export interface ConfigFileMeta {
  size: number
  sha256: string
}

export interface ConfigManifest {
  schemaVersion: number
  encryption: string
  createdAt: string
  appVersion: string
  /** Hostname of the machine that produced the snapshot, so "overwrite with
   *  the remote copy" can say whose copy it is. */
  sourceDevice: string
  config: ConfigFileMeta
  counts: DomainCounts
}

export interface ApplyReport {
  domains: DomainCounts
  total: number
}

export interface ConfigExportSummary {
  path: string
  manifest: ConfigManifest
}

export interface ConfigImportPreview {
  manifest: ConfigManifest
  /** False when the file is from a newer schema or fails its checksum; the
   *  import button stays disabled and `blockedReason` explains why. */
  importable: boolean
  blockedReason: string | null
}

export interface ConfigImportResult {
  manifest: ConfigManifest
  applied: ApplyReport
  rollbackPath: string | null
}

export interface ConfigSyncSettingsView {
  enabled: boolean
  serverUrl: string
  username: string
  /** The password itself never crosses the bridge. */
  hasPassword: boolean
  remoteDir: string
  profile: string
  autoSync: boolean
  intervalMinutes: number
}

export interface ConfigSyncSettingsInput {
  enabled: boolean
  serverUrl: string
  username: string
  /** `null` or `""` keeps the stored password. Never send a placeholder —
   *  the backend would store the placeholder verbatim. */
  password: string | null
  remoteDir: string
  profile: string
  autoSync: boolean
  intervalMinutes: number
}

export interface ConfigSyncState {
  lastUploadedSha256: string | null
  lastSyncAt: string | null
  lastError: string | null
}

export interface ConfigSyncStatusEvent {
  lastSyncAt: string | null
  lastError: string | null
}

export interface UploadOutcome {
  /** False means the snapshot was identical to the last upload and nothing
   *  was sent — a success, not a failure. */
  uploaded: boolean
  sha256: string
  counts: DomainCounts
  syncedAt: string
}

export interface DownloadOutcome {
  manifest: ConfigManifest
  applied: ApplyReport
  rollbackPath: string | null
}

/** Emitted only by the background uploader; manual actions return their
 *  result directly, so the UI never has to guess what a status refers to. */
export const CONFIG_SYNC_STATUS_EVENT = "config-sync://status"

export const CONFIG_EXPORT_EXTENSION = "codegcfg.json"

/** `codeg-config-2026-05-04-11-32-07.codegcfg.json` — sortable, and obvious
 *  in a downloads folder six months later. */
export function defaultExportFileName(now: Date = new Date()): string {
  const stamp = now.toISOString().slice(0, 19).replace(/[:T]/g, "-")
  return `codeg-config-${stamp}.${CONFIG_EXPORT_EXTENSION}`
}

/** `null` when the user dismissed the save dialog. */
export async function exportConfigToFile(): Promise<ConfigExportSummary | null> {
  const { save } = await import("@tauri-apps/plugin-dialog")
  const destPath = await save({
    defaultPath: defaultExportFileName(),
    filters: [{ name: "Codeg config", extensions: ["json"] }],
  })
  if (!destPath) return null
  return getTransport().call<ConfigExportSummary>("config_sync_export_file", {
    destPath,
  })
}

/** Opens a file picker and inspects the choice WITHOUT applying it. `null`
 *  when the dialog was dismissed. */
export async function pickConfigFileToImport(): Promise<{
  path: string
  preview: ConfigImportPreview
} | null> {
  const { open } = await import("@tauri-apps/plugin-dialog")
  const picked = await open({
    multiple: false,
    directory: false,
    filters: [{ name: "Codeg config", extensions: ["json"] }],
  })
  const srcPath = typeof picked === "string" ? picked : null
  if (!srcPath) return null
  const preview = await getTransport().call<ConfigImportPreview>(
    "config_sync_peek_file",
    { srcPath }
  )
  return { path: srcPath, preview }
}

export async function importConfigFromFile(
  srcPath: string
): Promise<ConfigImportResult> {
  return getTransport().call<ConfigImportResult>("config_sync_import_file", {
    srcPath,
  })
}

export async function getConfigSyncSettings(): Promise<ConfigSyncSettingsView> {
  return getTransport().call<ConfigSyncSettingsView>(
    "config_sync_get_settings",
    {}
  )
}

export async function updateConfigSyncSettings(
  settings: ConfigSyncSettingsInput
): Promise<ConfigSyncSettingsView> {
  return getTransport().call<ConfigSyncSettingsView>(
    "config_sync_update_settings",
    { settings }
  )
}

export async function getConfigSyncState(): Promise<ConfigSyncState> {
  return getTransport().call<ConfigSyncState>("config_sync_get_state", {})
}

/** Verifies the form's credentials without saving them. */
export async function testConfigSyncConnection(
  settings: ConfigSyncSettingsInput
): Promise<void> {
  await getTransport().call<null>("config_sync_test_connection", { settings })
}

/** Manual "sync now": uploads even when the hash is unchanged. */
export async function uploadConfigNow(): Promise<UploadOutcome> {
  return getTransport().call<UploadOutcome>("config_sync_upload_now", {})
}

/** `null` when the remote has no snapshot yet. */
export async function peekRemoteConfig(): Promise<ConfigManifest | null> {
  return getTransport().call<ConfigManifest | null>(
    "config_sync_peek_remote",
    {}
  )
}

/** Explicit, never automatic: overwrites local configuration with the remote
 *  snapshot after the user confirms. */
export async function downloadAndApplyConfig(): Promise<DownloadOutcome> {
  return getTransport().call<DownloadOutcome>("config_sync_download_apply", {})
}

export async function listenConfigSyncStatus(
  handler: (event: ConfigSyncStatusEvent) => void
): Promise<() => void> {
  return getTransport().subscribe<ConfigSyncStatusEvent>(
    CONFIG_SYNC_STATUS_EVENT,
    handler
  )
}

/** Domains with at least one row, in the fixed display order. Used by both
 *  the import preview and the post-apply summary so they read alike. */
export function summarizeCounts(counts: DomainCounts): {
  id: ConfigDomainId
  count: number
}[] {
  return CONFIG_DOMAIN_IDS.map((id) => ({ id, count: counts[id] ?? 0 })).filter(
    (entry) => entry.count > 0
  )
}
