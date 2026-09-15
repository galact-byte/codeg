import { describe, expect, it } from "vitest"

import {
  CONFIG_DOMAIN_IDS,
  CONFIG_EXPORT_EXTENSION,
  defaultExportFileName,
  summarizeCounts,
} from "./config-sync"

describe("defaultExportFileName", () => {
  it("is filesystem-safe and sorts chronologically", () => {
    const name = defaultExportFileName(new Date("2026-05-04T11:32:07.456Z"))
    expect(name).toBe(
      `codeg-config-2026-05-04-11-32-07.${CONFIG_EXPORT_EXTENSION}`
    )
    // Windows rejects ':' in file names — the timestamp must not smuggle one in.
    expect(name).not.toMatch(/[:]/)
    const earlier = defaultExportFileName(new Date("2026-05-04T11:32:06.000Z"))
    expect([name, earlier].sort()).toEqual([earlier, name])
  })
})

describe("summarizeCounts", () => {
  it("keeps the fixed domain order and hides empty domains", () => {
    const [first, second] = CONFIG_DOMAIN_IDS
    const summary = summarizeCounts({ [second]: 2, [first]: 3 } as never)
    expect(summary).toEqual([
      { id: first, count: 3 },
      { id: second, count: 2 },
    ])
  })

  it("treats a missing domain as zero rather than crashing", () => {
    expect(summarizeCounts({} as never)).toEqual([])
  })
})
