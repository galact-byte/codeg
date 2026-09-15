import { act, fireEvent, render, screen, waitFor } from "@testing-library/react"
import { NextIntlClientProvider } from "next-intl"
import { beforeEach, describe, expect, it, vi } from "vitest"

// Flipped per-test: the panel is desktop-only and must disappear entirely on
// web / remote-desktop rather than render disabled controls.
const env = vi.hoisted(() => ({
  desktop: true,
  remoteId: null as string | null,
}))

vi.mock("@/lib/platform", () => ({
  isDesktop: () => env.desktop,
  isLocalDesktop: () => env.desktop,
  openUrl: vi.fn(),
}))

vi.mock("@/lib/transport", () => ({
  getTransport: () => ({ call: vi.fn(), subscribe: vi.fn() }),
  isDesktop: () => env.desktop,
  isRemoteDesktopMode: () => env.remoteId !== null,
  getActiveRemoteConnectionId: () => env.remoteId,
}))

// Captured so a test can push a background-sync status frame.
let statusHandler: ((e: unknown) => void) | null = null

vi.mock("@/lib/config-sync", async () => {
  const actual =
    await vi.importActual<typeof import("@/lib/config-sync")>(
      "@/lib/config-sync"
    )
  return {
    ...actual,
    getConfigSyncSettings: vi.fn(),
    getConfigSyncState: vi.fn(),
    updateConfigSyncSettings: vi.fn(),
    testConfigSyncConnection: vi.fn(),
    uploadConfigNow: vi.fn(),
    peekRemoteConfig: vi.fn(),
    downloadAndApplyConfig: vi.fn(),
    exportConfigToFile: vi.fn(),
    pickConfigFileToImport: vi.fn(),
    importConfigFromFile: vi.fn(),
    listenConfigSyncStatus: vi.fn(async (handler: (e: unknown) => void) => {
      statusHandler = handler
      return () => {}
    }),
  }
})

const toastError = vi.fn()
const toastSuccess = vi.fn()
vi.mock("sonner", () => ({
  toast: {
    success: (m: string) => toastSuccess(m),
    error: (m: string) => toastError(m),
    message: vi.fn(),
  },
}))

import { ConfigSyncSettings } from "./config-sync-settings"
import enMessages from "@/i18n/messages/en.json"
import {
  downloadAndApplyConfig,
  getConfigSyncSettings,
  getConfigSyncState,
  importConfigFromFile,
  peekRemoteConfig,
  pickConfigFileToImport,
  updateConfigSyncSettings,
} from "@/lib/config-sync"

const t = enMessages.ConfigSyncSettings

const SAVED = {
  enabled: true,
  serverUrl: "https://dav.example.com/dav/",
  username: "alice",
  hasPassword: true,
  remoteDir: "codeg",
  profile: "default",
  autoSync: true,
  intervalMinutes: 5,
}

function manifest(overrides: Record<string, unknown> = {}) {
  return {
    schemaVersion: 1,
    encryption: "none",
    createdAt: "2026-06-06T10:00:00Z",
    appVersion: "0.30.0",
    sourceDevice: "work-laptop",
    config: { size: 2048, sha256: "abc" },
    counts: { modelProviders: 3, preferences: 7 },
    ...overrides,
  }
}

function renderPanel() {
  return render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <ConfigSyncSettings />
    </NextIntlClientProvider>
  )
}

/** Render and wait until the saved settings have populated the form. */
async function renderLoaded() {
  const view = renderPanel()
  await screen.findByRole("button", { name: t.saveButton })
  return view
}

beforeEach(() => {
  vi.clearAllMocks()
  statusHandler = null
  env.desktop = true
  env.remoteId = null
  vi.mocked(getConfigSyncSettings).mockResolvedValue({ ...SAVED })
  vi.mocked(getConfigSyncState).mockResolvedValue({
    lastUploadedSha256: null,
    lastSyncAt: null,
    lastError: null,
  })
  vi.mocked(updateConfigSyncSettings).mockResolvedValue({ ...SAVED })
})

describe("ConfigSyncSettings — availability", () => {
  it("renders nothing on web", () => {
    env.desktop = false
    const { container } = renderPanel()
    expect(container).toBeEmptyDOMElement()
    expect(getConfigSyncSettings).not.toHaveBeenCalled()
  })

  it("renders nothing for a remote-desktop window", () => {
    env.remoteId = "remote-1"
    const { container } = renderPanel()
    expect(container).toBeEmptyDOMElement()
  })

  it("shows the file actions even before WebDAV is set up", async () => {
    vi.mocked(getConfigSyncSettings).mockResolvedValue({
      ...SAVED,
      enabled: false,
    })
    renderPanel()
    await screen.findByRole("button", { name: t.exportButton })
    expect(
      screen.queryByRole("button", { name: t.uploadButton })
    ).not.toBeInTheDocument()
  })
})

describe("ConfigSyncSettings — credentials", () => {
  it("sends a null password when the field is untouched, keeping the stored one", async () => {
    await renderLoaded()
    fireEvent.click(screen.getByRole("button", { name: t.saveButton }))
    await waitFor(() => expect(updateConfigSyncSettings).toHaveBeenCalled())
    expect(vi.mocked(updateConfigSyncSettings).mock.calls[0][0]).toMatchObject({
      username: "alice",
      password: null,
    })
  })

  it("sends a typed password and then clears the field", async () => {
    const { container } = await renderLoaded()
    const password = container.querySelector(
      "#config-sync-password"
    ) as HTMLInputElement
    fireEvent.change(password, { target: { value: "s3cret" } })
    fireEvent.click(screen.getByRole("button", { name: t.saveButton }))
    await waitFor(() => expect(updateConfigSyncSettings).toHaveBeenCalled())
    expect(vi.mocked(updateConfigSyncSettings).mock.calls[0][0]).toMatchObject({
      password: "s3cret",
    })
    // Cleared so a second save does not resend a value the user cannot see.
    await waitFor(() => expect(password.value).toBe(""))
  })

  it("disables the remote actions until a server and user are filled in", async () => {
    vi.mocked(getConfigSyncSettings).mockResolvedValue({
      ...SAVED,
      serverUrl: "",
      username: "",
      hasPassword: false,
    })
    await renderLoaded()
    expect(screen.getByRole("button", { name: t.saveButton })).toBeDisabled()
    expect(screen.getByRole("button", { name: t.uploadButton })).toBeDisabled()
  })
})

describe("ConfigSyncSettings — restore from remote", () => {
  it("never downloads without a confirmation naming the source snapshot", async () => {
    vi.mocked(peekRemoteConfig).mockResolvedValue(manifest())
    await renderLoaded()
    fireEvent.click(screen.getByRole("button", { name: t.restoreButton }))
    await screen.findByText(t.restoreConfirmTitle)
    expect(downloadAndApplyConfig).not.toHaveBeenCalled()
    expect(screen.getByText(/work-laptop/)).toBeInTheDocument()

    vi.mocked(downloadAndApplyConfig).mockResolvedValue({
      manifest: manifest(),
      applied: { domains: { modelProviders: 3 }, total: 3 },
      rollbackPath: null,
    })
    fireEvent.click(
      screen.getByRole("button", { name: t.restoreConfirmAction })
    )
    await waitFor(() => expect(downloadAndApplyConfig).toHaveBeenCalled())
  })

  it("reports an empty remote instead of opening the dialog", async () => {
    vi.mocked(peekRemoteConfig).mockResolvedValue(null)
    await renderLoaded()
    fireEvent.click(screen.getByRole("button", { name: t.restoreButton }))
    await waitFor(() =>
      expect(toastError).toHaveBeenCalledWith(t.noRemoteSnapshot)
    )
    expect(screen.queryByText(t.restoreConfirmTitle)).not.toBeInTheDocument()
  })
})

describe("ConfigSyncSettings — file import", () => {
  it("previews the file and applies it only after confirmation", async () => {
    vi.mocked(pickConfigFileToImport).mockResolvedValue({
      path: "/tmp/config.json",
      preview: { manifest: manifest(), importable: true, blockedReason: null },
    })
    vi.mocked(importConfigFromFile).mockResolvedValue({
      manifest: manifest(),
      applied: { domains: { modelProviders: 3 }, total: 3 },
      rollbackPath: null,
    })
    await renderLoaded()
    fireEvent.click(screen.getByRole("button", { name: t.importButton }))
    await screen.findByText(t.importConfirmTitle)
    expect(importConfigFromFile).not.toHaveBeenCalled()

    fireEvent.click(screen.getByRole("button", { name: t.importConfirmAction }))
    await waitFor(() =>
      expect(importConfigFromFile).toHaveBeenCalledWith("/tmp/config.json")
    )
  })

  it("blocks importing a snapshot the backend rejected", async () => {
    vi.mocked(pickConfigFileToImport).mockResolvedValue({
      path: "/tmp/future.json",
      preview: {
        manifest: manifest({ schemaVersion: 99 }),
        importable: false,
        blockedReason: "newer schema",
      },
    })
    await renderLoaded()
    fireEvent.click(screen.getByRole("button", { name: t.importButton }))
    await screen.findByText(t.importBlocked)
    expect(
      screen.getByRole("button", { name: t.importConfirmAction })
    ).toBeDisabled()
  })

  it("stays quiet when the file dialog is dismissed", async () => {
    vi.mocked(pickConfigFileToImport).mockResolvedValue(null)
    await renderLoaded()
    fireEvent.click(screen.getByRole("button", { name: t.importButton }))
    await waitFor(() => expect(pickConfigFileToImport).toHaveBeenCalled())
    expect(screen.queryByText(t.importConfirmTitle)).not.toBeInTheDocument()
    expect(toastError).not.toHaveBeenCalled()
  })
})

describe("ConfigSyncSettings — background status", () => {
  it("reflects an upload performed by the periodic loop", async () => {
    await renderLoaded()
    expect(screen.getByText(t.neverSynced)).toBeInTheDocument()
    act(() => {
      statusHandler?.({ lastSyncAt: "2026-06-06T12:00:00Z", lastError: null })
    })
    await waitFor(() =>
      expect(screen.queryByText(t.neverSynced)).not.toBeInTheDocument()
    )
  })

  it("surfaces the last background failure", async () => {
    await renderLoaded()
    act(() => {
      statusHandler?.({ lastSyncAt: null, lastError: "connection refused" })
    })
    await screen.findByText(/connection refused/)
  })
})
