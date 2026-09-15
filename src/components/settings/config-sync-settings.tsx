"use client"

import { useCallback, useEffect, useRef, useState } from "react"
import {
  Check,
  CloudUpload,
  FileDown,
  FileUp,
  Loader2,
  RefreshCw,
  ShieldAlert,
  X,
} from "lucide-react"
import { useTranslations } from "next-intl"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select"
import { Switch } from "@/components/ui/switch"
import {
  AlertDialog,
  AlertDialogAction,
  AlertDialogCancel,
  AlertDialogContent,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogTitle,
} from "@/components/ui/alert-dialog"
import {
  toLocalizedErrorMessage,
  type AppErrorTranslator,
} from "@/lib/app-error"
import { isDesktop } from "@/lib/platform"
import { getActiveRemoteConnectionId } from "@/lib/transport"
import {
  downloadAndApplyConfig,
  exportConfigToFile,
  getConfigSyncSettings,
  getConfigSyncState,
  importConfigFromFile,
  listenConfigSyncStatus,
  peekRemoteConfig,
  pickConfigFileToImport,
  summarizeCounts,
  testConfigSyncConnection,
  updateConfigSyncSettings,
  uploadConfigNow,
  type ConfigImportPreview,
  type ConfigManifest,
  type ConfigSyncSettingsInput,
  type DomainCounts,
} from "@/lib/config-sync"

/** Fixed choices instead of a free number field: the interval only has to be
 *  "how stale may the remote copy be", and an open input invites 0 or 99999. */
const INTERVAL_OPTIONS = [5, 15, 30, 60] as const

const SCOPE_INCLUDED = [
  "providers",
  "agentSettings",
  "customAgents",
  "quickMessages",
  "taskTemplates",
  "preferences",
] as const

const SCOPE_EXCLUDED = [
  "conversations",
  "uploads",
  "workspaces",
  "mcp",
  "credentials",
] as const

/// Address templates only. A preset never changes how the client talks to the
/// server, so adding one is a translation change, not a protocol change.
const WEBDAV_PRESETS = [
  { id: "jianguoyun", url: "https://dav.jianguoyun.com/dav/" },
  {
    id: "nextcloud",
    url: "https://example.com/remote.php/dav/files/username/",
  },
  { id: "synology", url: "http://192.168.1.10:5005/" },
  { id: "custom", url: null },
] as const

type PresetId = (typeof WEBDAV_PRESETS)[number]["id"]

/// Recognises a saved URL so reopening settings keeps the preset highlighted.
function presetFromUrl(url: string): PresetId {
  const value = url.trim().toLowerCase()
  if (value.includes("dav.jianguoyun.com")) return "jianguoyun"
  if (value.includes("/remote.php/dav")) return "nextcloud"
  if (/:(5005|5006)(\/|$)/.test(value)) return "synology"
  return "custom"
}

type PendingImport = { path: string; preview: ConfigImportPreview }

function formatTimestamp(value: string | null): string | null {
  if (!value) return null
  const parsed = new Date(value)
  return Number.isNaN(parsed.getTime()) ? value : parsed.toLocaleString()
}

export function ConfigSyncSettings() {
  const t = useTranslations("ConfigSyncSettings")
  // Root translator so backend errors carrying `configSync.error.*` keys
  // localize; falls back to the English message when the key is unknown.
  const tRoot = useTranslations()
  const localize = useCallback(
    (err: unknown) =>
      toLocalizedErrorMessage(err, tRoot as unknown as AppErrorTranslator),
    [tRoot]
  )

  // Native dialogs plus Tauri-only commands: a remote-desktop window points at
  // another machine's server, where neither applies.
  const desktop = isDesktop() && getActiveRemoteConnectionId() === null

  const [loaded, setLoaded] = useState(false)
  const [enabled, setEnabled] = useState(false)
  const [serverUrl, setServerUrl] = useState("")
  const [preset, setPreset] = useState<PresetId>("custom")
  const [username, setUsername] = useState("")
  // Always starts empty. Submitting an empty field means "keep the stored
  // password"; rendering a mask here would risk saving the mask itself.
  const [password, setPassword] = useState("")
  const [hasPassword, setHasPassword] = useState(false)
  const [remoteDir, setRemoteDir] = useState("codeg")
  const [profile, setProfile] = useState("default")
  const [autoSync, setAutoSync] = useState(true)
  const [intervalMinutes, setIntervalMinutes] = useState(5)

  const [lastSyncAt, setLastSyncAt] = useState<string | null>(null)
  const [lastError, setLastError] = useState<string | null>(null)

  const [busy, setBusy] = useState<
    null | "save" | "test" | "upload" | "download" | "export" | "import"
  >(null)
  const [pendingImport, setPendingImport] = useState<PendingImport | null>(null)
  const [remoteManifest, setRemoteManifest] = useState<ConfigManifest | null>(
    null
  )
  const [restoreOpen, setRestoreOpen] = useState(false)

  // Guards against a late `setState` when the settings page unmounts during a
  // slow WebDAV round-trip.
  const mounted = useRef(true)
  useEffect(() => {
    mounted.current = true
    return () => {
      mounted.current = false
    }
  }, [])

  useEffect(() => {
    if (!desktop) return
    let cancelled = false
    void (async () => {
      try {
        const [settings, state] = await Promise.all([
          getConfigSyncSettings(),
          getConfigSyncState(),
        ])
        if (cancelled) return
        setEnabled(settings.enabled)
        setServerUrl(settings.serverUrl)
        setPreset(presetFromUrl(settings.serverUrl))
        setUsername(settings.username)
        setHasPassword(settings.hasPassword)
        setRemoteDir(settings.remoteDir)
        setProfile(settings.profile)
        setAutoSync(settings.autoSync)
        setIntervalMinutes(settings.intervalMinutes)
        setLastSyncAt(state.lastSyncAt)
        setLastError(state.lastError)
      } catch (err) {
        console.error("[config-sync] failed to load settings", err)
      } finally {
        if (!cancelled) setLoaded(true)
      }
    })()
    return () => {
      cancelled = true
    }
  }, [desktop])

  // The background uploader reports here; without this the panel would show a
  // stale "last synced" until the page is reopened.
  useEffect(() => {
    if (!desktop) return
    let unlisten: (() => void) | null = null
    let disposed = false
    void listenConfigSyncStatus((event) => {
      setLastSyncAt(event.lastSyncAt)
      setLastError(event.lastError)
    }).then((fn) => {
      if (disposed) fn()
      else unlisten = fn
    })
    return () => {
      disposed = true
      unlisten?.()
    }
  }, [desktop])

  const currentInput = useCallback(
    (): ConfigSyncSettingsInput => ({
      enabled,
      serverUrl: serverUrl.trim(),
      username: username.trim(),
      password: password.length > 0 ? password : null,
      remoteDir: remoteDir.trim(),
      profile: profile.trim(),
      autoSync,
      intervalMinutes,
    }),
    [
      enabled,
      serverUrl,
      username,
      password,
      remoteDir,
      profile,
      autoSync,
      intervalMinutes,
    ]
  )

  const handleSave = useCallback(async () => {
    setBusy("save")
    try {
      const saved = await updateConfigSyncSettings(currentInput())
      if (!mounted.current) return
      setHasPassword(saved.hasPassword)
      setRemoteDir(saved.remoteDir)
      setProfile(saved.profile)
      setIntervalMinutes(saved.intervalMinutes)
      // Clear the field once it is stored, so a second save does not re-send
      // a value the user cannot see.
      setPassword("")
      toast.success(t("saved"))
    } catch (err) {
      toast.error(localize(err))
    } finally {
      if (mounted.current) setBusy(null)
    }
  }, [currentInput, localize, t])

  const handleTest = useCallback(async () => {
    setBusy("test")
    try {
      await testConfigSyncConnection(currentInput())
      if (mounted.current) toast.success(t("testSucceeded"))
    } catch (err) {
      toast.error(localize(err))
    } finally {
      if (mounted.current) setBusy(null)
    }
  }, [currentInput, localize, t])

  const handleUpload = useCallback(async () => {
    setBusy("upload")
    try {
      const outcome = await uploadConfigNow()
      if (!mounted.current) return
      setLastSyncAt(outcome.syncedAt)
      setLastError(null)
      toast.success(t("uploaded"))
    } catch (err) {
      const message = localize(err)
      if (mounted.current) setLastError(message)
      toast.error(message)
    } finally {
      if (mounted.current) setBusy(null)
    }
  }, [localize, t])

  /** Look before overwriting: the confirmation names the machine and time the
   *  remote snapshot came from. */
  const handleOpenRestore = useCallback(async () => {
    setBusy("download")
    try {
      const manifest = await peekRemoteConfig()
      if (!mounted.current) return
      if (!manifest) {
        toast.error(t("noRemoteSnapshot"))
        return
      }
      setRemoteManifest(manifest)
      setRestoreOpen(true)
    } catch (err) {
      toast.error(localize(err))
    } finally {
      if (mounted.current) setBusy(null)
    }
  }, [localize, t])

  const handleConfirmRestore = useCallback(async () => {
    setRestoreOpen(false)
    setBusy("download")
    try {
      const outcome = await downloadAndApplyConfig()
      if (!mounted.current) return
      toast.success(t("restored", { count: outcome.applied.total }))
    } catch (err) {
      toast.error(localize(err))
    } finally {
      if (mounted.current) setBusy(null)
    }
  }, [localize, t])

  const handleExport = useCallback(async () => {
    setBusy("export")
    try {
      const summary = await exportConfigToFile()
      // `null` = the user dismissed the save dialog, which is not an error.
      if (summary && mounted.current) toast.success(t("exported"))
    } catch (err) {
      toast.error(localize(err))
    } finally {
      if (mounted.current) setBusy(null)
    }
  }, [localize, t])

  const handlePickImport = useCallback(async () => {
    setBusy("import")
    try {
      const picked = await pickConfigFileToImport()
      if (picked && mounted.current) setPendingImport(picked)
    } catch (err) {
      toast.error(localize(err))
    } finally {
      if (mounted.current) setBusy(null)
    }
  }, [localize])

  const handleConfirmImport = useCallback(async () => {
    if (!pendingImport) return
    const path = pendingImport.path
    setPendingImport(null)
    setBusy("import")
    try {
      const result = await importConfigFromFile(path)
      if (mounted.current) {
        toast.success(t("imported", { count: result.applied.total }))
      }
    } catch (err) {
      toast.error(localize(err))
    } finally {
      if (mounted.current) setBusy(null)
    }
  }, [pendingImport, localize, t])

  if (!desktop) return null

  const syncedLabel = formatTimestamp(lastSyncAt)
  // Only the hosted services need a setup note; "custom" has nothing to say.
  const presetHint = preset === "custom" ? null : t(`presetHint.${preset}`)
  const remoteBusy = busy === "upload" || busy === "download"
  const credentialsIncomplete =
    serverUrl.trim().length === 0 || username.trim().length === 0

  return (
    <section className="rounded-xl border bg-card p-4 space-y-4">
      <div className="flex items-center gap-2">
        <CloudUpload className="h-4 w-4 text-muted-foreground" />
        <h2 className="text-sm font-semibold">{t("title")}</h2>
      </div>

      <p className="text-xs text-muted-foreground leading-5">
        {t("description")}
      </p>

      {/* The two columns exist so nobody reads "sync" as "backs up everything". */}
      <div className="grid gap-3 rounded-lg border bg-muted/30 p-3 sm:grid-cols-2">
        <div className="space-y-1.5">
          <Label className="text-2xs font-medium text-muted-foreground">
            {t("scopeIncludedTitle")}
          </Label>
          <ul className="space-y-1">
            {SCOPE_INCLUDED.map((key) => (
              <li key={key} className="flex items-start gap-1.5 text-2xs">
                <Check className="mt-0.5 h-3 w-3 shrink-0 text-emerald-600 dark:text-emerald-500" />
                <span>{t(`scopeIncluded.${key}`)}</span>
              </li>
            ))}
          </ul>
        </div>
        <div className="space-y-1.5">
          <Label className="text-2xs font-medium text-muted-foreground">
            {t("scopeExcludedTitle")}
          </Label>
          <ul className="space-y-1">
            {SCOPE_EXCLUDED.map((key) => (
              <li
                key={key}
                className="flex items-start gap-1.5 text-2xs text-muted-foreground"
              >
                <X className="mt-0.5 h-3 w-3 shrink-0" />
                <span>{t(`scopeExcluded.${key}`)}</span>
              </li>
            ))}
          </ul>
        </div>
      </div>

      {/* Local file transfer works with no server at all, so it comes first. */}
      <div className="space-y-2">
        <Label className="text-xs font-medium text-muted-foreground">
          {t("fileTitle")}
        </Label>
        <div className="flex flex-wrap gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={handleExport}
            disabled={busy !== null}
          >
            {busy === "export" ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              <FileDown className="h-4 w-4" />
            )}
            {t("exportButton")}
          </Button>
          <Button
            variant="outline"
            size="sm"
            onClick={handlePickImport}
            disabled={busy !== null}
          >
            {busy === "import" ? (
              <Loader2 className="h-4 w-4 animate-spin" />
            ) : (
              <FileUp className="h-4 w-4" />
            )}
            {t("importButton")}
          </Button>
        </div>
        <p className="text-2xs text-muted-foreground">{t("fileHint")}</p>
      </div>

      <div className="border-t pt-4 space-y-4">
        <div className="flex items-center justify-between gap-4">
          <div className="space-y-1">
            <Label className="text-xs font-medium">{t("webdavTitle")}</Label>
            <p className="text-2xs text-muted-foreground">{t("webdavHint")}</p>
          </div>
          <Switch
            checked={enabled}
            onCheckedChange={setEnabled}
            disabled={!loaded || busy !== null}
          />
        </div>

        {enabled ? (
          <div className="space-y-3">
            {/* Pure address templates — no provider-specific logic anywhere. */}
            <div className="space-y-2">
              <Label className="text-xs font-medium text-muted-foreground">
                {t("presetLabel")}
              </Label>
              <div className="flex flex-wrap gap-2">
                {WEBDAV_PRESETS.map((item) => (
                  <Button
                    key={item.id}
                    type="button"
                    size="sm"
                    variant={preset === item.id ? "secondary" : "outline"}
                    onClick={() => {
                      setPreset(item.id)
                      if (item.url) setServerUrl(item.url)
                    }}
                    disabled={busy !== null}
                  >
                    {t(`preset.${item.id}`)}
                  </Button>
                ))}
              </div>
              {presetHint ? (
                <p className="text-2xs text-muted-foreground">{presetHint}</p>
              ) : null}
            </div>

            <div className="space-y-2">
              <Label
                htmlFor="config-sync-url"
                className="text-xs font-medium text-muted-foreground"
              >
                {t("serverUrl")}
              </Label>
              <Input
                id="config-sync-url"
                value={serverUrl}
                onChange={(e) => {
                  setServerUrl(e.target.value)
                  setPreset(presetFromUrl(e.target.value))
                }}
                placeholder="https://dav.example.com/dav/"
                autoComplete="off"
                spellCheck={false}
              />
            </div>

            <div className="grid gap-3 sm:grid-cols-2">
              <div className="space-y-2">
                <Label
                  htmlFor="config-sync-user"
                  className="text-xs font-medium text-muted-foreground"
                >
                  {t("username")}
                </Label>
                <Input
                  id="config-sync-user"
                  value={username}
                  onChange={(e) => setUsername(e.target.value)}
                  autoComplete="off"
                  spellCheck={false}
                />
              </div>
              <div className="space-y-2">
                <Label
                  htmlFor="config-sync-password"
                  className="text-xs font-medium text-muted-foreground"
                >
                  {t("password")}
                </Label>
                <Input
                  id="config-sync-password"
                  type="password"
                  value={password}
                  onChange={(e) => setPassword(e.target.value)}
                  placeholder={
                    hasPassword ? t("passwordKeep") : t("passwordPlaceholder")
                  }
                  autoComplete="new-password"
                />
              </div>
            </div>

            <div className="grid gap-3 sm:grid-cols-2">
              <div className="space-y-2">
                <Label
                  htmlFor="config-sync-dir"
                  className="text-xs font-medium text-muted-foreground"
                >
                  {t("remoteDir")}
                </Label>
                <Input
                  id="config-sync-dir"
                  value={remoteDir}
                  onChange={(e) => setRemoteDir(e.target.value)}
                  spellCheck={false}
                />
              </div>
              <div className="space-y-2">
                <Label
                  htmlFor="config-sync-profile"
                  className="text-xs font-medium text-muted-foreground"
                >
                  {t("profile")}
                </Label>
                <Input
                  id="config-sync-profile"
                  value={profile}
                  onChange={(e) => setProfile(e.target.value)}
                  spellCheck={false}
                />
              </div>
            </div>
            <p className="text-2xs text-muted-foreground">{t("profileHint")}</p>

            <div className="flex items-center justify-between gap-4">
              <div className="space-y-1">
                <Label className="text-xs font-medium">
                  {t("autoSyncLabel")}
                </Label>
                <p className="text-2xs text-muted-foreground">
                  {t("autoSyncHint")}
                </p>
              </div>
              <Switch
                checked={autoSync}
                onCheckedChange={setAutoSync}
                disabled={busy !== null}
              />
            </div>

            <div className="space-y-2">
              <Label className="text-xs font-medium text-muted-foreground">
                {t("intervalLabel")}
              </Label>
              <Select
                value={String(intervalMinutes)}
                onValueChange={(value) => setIntervalMinutes(Number(value))}
                disabled={!autoSync || busy !== null}
              >
                <SelectTrigger className="w-full sm:w-56">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent align="start">
                  {INTERVAL_OPTIONS.map((minutes) => (
                    <SelectItem key={minutes} value={String(minutes)}>
                      {t("intervalOption", { minutes })}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            </div>

            <div className="flex flex-wrap gap-2">
              <Button
                size="sm"
                onClick={handleSave}
                disabled={busy !== null || credentialsIncomplete}
              >
                {busy === "save" ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : null}
                {t("saveButton")}
              </Button>
              <Button
                variant="outline"
                size="sm"
                onClick={handleTest}
                disabled={busy !== null || credentialsIncomplete}
              >
                {busy === "test" ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : null}
                {t("testButton")}
              </Button>
              <Button
                variant="outline"
                size="sm"
                onClick={handleUpload}
                disabled={busy !== null || credentialsIncomplete}
              >
                {busy === "upload" ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  <CloudUpload className="h-4 w-4" />
                )}
                {t("uploadButton")}
              </Button>
              <Button
                variant="outline"
                size="sm"
                onClick={handleOpenRestore}
                disabled={busy !== null || credentialsIncomplete}
              >
                {busy === "download" ? (
                  <Loader2 className="h-4 w-4 animate-spin" />
                ) : (
                  <RefreshCw className="h-4 w-4" />
                )}
                {t("restoreButton")}
              </Button>
            </div>

            <div className="text-2xs text-muted-foreground space-y-1">
              <p>
                {syncedLabel
                  ? t("lastSyncAt", { time: syncedLabel })
                  : t("neverSynced")}
              </p>
              {lastError ? (
                <p className="text-destructive">
                  {t("lastError", { message: lastError })}
                </p>
              ) : null}
              {remoteBusy ? <p>{t("working")}</p> : null}
            </div>

            <div className="flex items-start gap-2 rounded-lg border border-amber-500/40 bg-amber-500/5 p-3">
              <ShieldAlert className="h-4 w-4 shrink-0 text-amber-600" />
              <p className="text-2xs text-muted-foreground leading-5">
                {t("plaintextWarning")}
              </p>
            </div>
          </div>
        ) : null}
      </div>

      <AlertDialog
        open={pendingImport !== null}
        onOpenChange={(open) => {
          if (!open) setPendingImport(null)
        }}
      >
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("importConfirmTitle")}</AlertDialogTitle>
            <AlertDialogDescription asChild>
              <div className="space-y-2">
                <p>
                  {pendingImport?.preview.blockedReason
                    ? t("importBlocked")
                    : t("importConfirmBody")}
                </p>
                {pendingImport ? (
                  <CountsSummary
                    counts={pendingImport.preview.manifest.counts}
                  />
                ) : null}
              </div>
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>{t("cancel")}</AlertDialogCancel>
            <AlertDialogAction
              onClick={(e) => {
                e.preventDefault()
                void handleConfirmImport()
              }}
              disabled={!pendingImport?.preview.importable}
            >
              {t("importConfirmAction")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>

      <AlertDialog open={restoreOpen} onOpenChange={setRestoreOpen}>
        <AlertDialogContent>
          <AlertDialogHeader>
            <AlertDialogTitle>{t("restoreConfirmTitle")}</AlertDialogTitle>
            <AlertDialogDescription asChild>
              <div className="space-y-2">
                <p>
                  {t("restoreConfirmBody", {
                    device: remoteManifest?.sourceDevice ?? "",
                    time:
                      formatTimestamp(remoteManifest?.createdAt ?? null) ?? "",
                  })}
                </p>
                {remoteManifest ? (
                  <CountsSummary counts={remoteManifest.counts} />
                ) : null}
              </div>
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogCancel>{t("cancel")}</AlertDialogCancel>
            <AlertDialogAction
              onClick={(e) => {
                e.preventDefault()
                void handleConfirmRestore()
              }}
            >
              {t("restoreConfirmAction")}
            </AlertDialogAction>
          </AlertDialogFooter>
        </AlertDialogContent>
      </AlertDialog>
    </section>
  )
}

/** "3 providers · 2 agents · 12 preferences" — what is actually about to be
 *  written, so a confirmation is more than a shrug. */
function CountsSummary({ counts }: { counts: DomainCounts }) {
  const t = useTranslations("ConfigSyncSettings")
  const entries = summarizeCounts(counts)
  if (entries.length === 0) {
    return (
      <p className="text-2xs text-muted-foreground">{t("emptySnapshot")}</p>
    )
  }
  return (
    <ul className="text-2xs text-muted-foreground space-y-0.5">
      {entries.map((entry) => (
        <li key={entry.id}>
          {t(`domain.${entry.id}`, { count: entry.count })}
        </li>
      ))}
    </ul>
  )
}
