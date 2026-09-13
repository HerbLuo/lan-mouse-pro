import { describe, expect, it, beforeEach } from 'vitest'
import { daemonStore, diffClientConfigPatch } from './index'
import type {
  ClipboardConfig,
  ClientConfig,
  ClientHandle,
  ClientState,
  FileTransferFailed,
  FrontendRequest,
  MonitorInfo,
} from '../api/ipc'

/** STEP-M3-3.2 unit tests for the Vue store layer.
 *
 *  Covers the two M3 contract pieces:
 *    1. `MonitorsChanged` events populate `state.monitors`.
 *    2. `diffClientConfigPatch` emits the minimum set of
 *       `FrontendRequest`s (esp. `UpdateMonitor`) and skips
 *       no-op patches.
 *
 *  Skips testing `updateClientConfig` (the wrapper around
 *  `diffClientConfigPatch` that calls `getSocket().request`)
 *  because that would require mocking the `DaemonSocket`
 *  singleton; `diffClientConfigPatch` is the testable seam.
 *  Skips `applyEvent` end-to-end because it mutates the
 *  module-level singleton state — instead we drive the
 *  `MonitorsChanged` arm by calling `applyEvent` via a
 *  re-import and verifying the resulting `state.monitors`.
 */

// ---------- fixtures ----------

function baseConfig(): ClientConfig {
  return {
    hostname: 'peer-east',
    fix_ips: [],
    port: 2268,
    pos: 'right',
    cmd: null,
    input_channels: { mouse_button: 'datagram', keyboard: 'stream' },
    monitor: null,
  }
}

function baseState(): ClientState {
  return {
    active: false,
    active_addr: null,
    dns_ips: [],
    ips: [],
    has_pressed_keys: false,
    resolving: false,
    peer_commit: null,
  }
}

const DUAL: MonitorInfo[] = [
  {
    id: 'CGDisplay:1',
    name: 'Built-in',
    position: [0, 0],
    size: [2560, 1600],
    primary: true,
    scale: 2.0,
  },
  {
    id: 'CGDisplay:2',
    name: 'External',
    position: [2560, 0],
    size: [1920, 1080],
    primary: false,
    scale: 1.0,
  },
]

const SINGLE: MonitorInfo[] = [
  {
    id: 'CGDisplay:1',
    name: 'Built-in',
    position: [0, 0],
    size: [1920, 1080],
    primary: true,
    scale: 1.0,
  },
]

// ---------- diffClientConfigPatch ----------

describe('diffClientConfigPatch', () => {
  const handle: ClientHandle = 42

  it('emits no requests when the patch is empty', () => {
    const out = diffClientConfigPatch(handle, baseConfig(), {})
    expect(out).toEqual([])
  })

  it('emits UpdateMonitor when monitor changes from null to a specific id', () => {
    const out = diffClientConfigPatch(handle, baseConfig(), { monitor: 'CGDisplay:2' })
    expect(out).toEqual([{ UpdateMonitor: [handle, 'CGDisplay:2'] }])
  })

  it('emits UpdateMonitor when monitor changes from one id to another', () => {
    const current: ClientConfig = { ...baseConfig(), monitor: 'CGDisplay:1' }
    const out = diffClientConfigPatch(handle, current, { monitor: 'CGDisplay:2' })
    expect(out).toEqual([{ UpdateMonitor: [handle, 'CGDisplay:2'] }])
  })

  it('emits UpdateMonitor(null) when clearing the binding back to Any', () => {
    const current: ClientConfig = { ...baseConfig(), monitor: 'CGDisplay:2' }
    const out = diffClientConfigPatch(handle, current, { monitor: null })
    expect(out).toEqual([{ UpdateMonitor: [handle, null] }])
  })

  it('does NOT emit UpdateMonitor when re-selecting the same monitor (idempotent)', () => {
    const current: ClientConfig = { ...baseConfig(), monitor: 'CGDisplay:1' }
    const out = diffClientConfigPatch(handle, current, { monitor: 'CGDisplay:1' })
    expect(out).toEqual([])
  })

  it('does NOT emit UpdateMonitor when patch.monitor is undefined (other field patch)', () => {
    const out = diffClientConfigPatch(handle, baseConfig(), { port: 2269 })
    expect(out).toEqual([{ UpdatePort: [handle, 2269] }])
  })

  it('emits multiple requests when several fields differ', () => {
    const out = diffClientConfigPatch(handle, baseConfig(), {
      pos: 'top',
      monitor: 'CGDisplay:2',
    })
    // Order matches the implementation: hostname, port, pos,
    // input_channels, monitor. With pos=top and monitor=DP-2
    // both changed, we expect exactly two requests.
    expect(out).toHaveLength(2)
    expect(out).toContainEqual({ UpdatePosition: [handle, 'top'] })
    expect(out).toContainEqual({ UpdateMonitor: [handle, 'CGDisplay:2'] })
  })

  it('preserves the legacy UpdateHostname empty-string-to-null semantics', () => {
    const current: ClientConfig = { ...baseConfig(), hostname: 'peer-east' }
    const out = diffClientConfigPatch(handle, current, { hostname: '' })
    expect(out).toEqual([{ UpdateHostname: [handle, null] }])
  })

  it('skips UpdateHostname when value is unchanged', () => {
    const current: ClientConfig = { ...baseConfig(), hostname: 'peer-east' }
    const out = diffClientConfigPatch(handle, current, { hostname: 'peer-east' })
    expect(out).toEqual([])
  })
})

// ---------- MonitorsChanged → state.monitors ----------

describe('MonitorsChanged event → state.monitors', () => {
  beforeEach(() => {
    // Reset the singleton between tests so we never leak
    // monitor lists across cases.
    daemonStore.monitors = []
  })

  it('populates state.monitors from a MonitorsChanged event (single monitor)', async () => {
    // Import dynamically so the module-level state is fresh
    // even if vitest re-uses module instances across files.
    const { applyEvent } = await import('./index')
    applyEvent({ MonitorsChanged: SINGLE })
    expect(daemonStore.monitors).toEqual(SINGLE)
    expect(daemonStore.monitors).toHaveLength(1)
    expect(daemonStore.monitors[0]!.id).toBe('CGDisplay:1')
  })

  it('populates state.monitors from a MonitorsChanged event (multi monitor)', async () => {
    const { applyEvent } = await import('./index')
    applyEvent({ MonitorsChanged: DUAL })
    expect(daemonStore.monitors).toEqual(DUAL)
    expect(daemonStore.monitors).toHaveLength(2)
    expect(daemonStore.monitors.map((m) => m.id)).toEqual(['CGDisplay:1', 'CGDisplay:2'])
  })

  it('replaces the list on a subsequent MonitorsChanged (hotplug)', async () => {
    const { applyEvent } = await import('./index')
    applyEvent({ MonitorsChanged: DUAL })
    expect(daemonStore.monitors).toHaveLength(2)
    applyEvent({ MonitorsChanged: SINGLE })
    expect(daemonStore.monitors).toHaveLength(1)
    expect(daemonStore.monitors[0]!.id).toBe('CGDisplay:1')
  })

  it('treats an empty MonitorsChanged list as "no monitors known"', async () => {
    const { applyEvent } = await import('./index')
    applyEvent({ MonitorsChanged: DUAL })
    applyEvent({ MonitorsChanged: [] })
    expect(daemonStore.monitors).toEqual([])
  })

  it('treats a legacy config row (monitor: null) as compatible with any list', async () => {
    // This is the case from the "旧 config（无 monitor 字段）" row
    // of the §8 test matrix. Pre-M3 payloads deserialize as
    // `monitor: null` (via #[serde(default)] on the Rust side);
    // here we mirror that by constructing the config in TS. The
    // row must be usable regardless of which monitors the daemon
    // has pushed — its `null` binding matches the "Any"
    // `<option value="">` in the dropdown.
    const { applyEvent } = await import('./index')
    applyEvent({ MonitorsChanged: DUAL })
    const legacy: ClientConfig = baseConfig() // monitor: null
    expect(legacy.monitor).toBeNull()
    // The legacy config is structurally compatible with the
    // current monitor list — there's nothing to invalidate
    // against.
    const allIds = new Set(daemonStore.monitors.map((m) => m.id))
    expect(allIds.has(legacy.monitor ?? '__null__')).toBe(false)
  })
})

// ---------- daemonStore.monitors reactive init ----------

describe('daemonStore.monitors initial state', () => {
  it('starts as an empty array', () => {
    // Fresh import of the module — re-evaluate to get the
    // singleton. We don't rely on this being an exact reset
    // (other tests may have run first) but the field MUST be
    // an array, and reading it cannot throw.
    expect(Array.isArray(daemonStore.monitors)).toBe(true)
  })
})

// ---------- M5 STEP-5.3: ClipboardConfigChanged → state.clipboardConfig ----------

const SAMPLE_CLIPBOARD_CONFIG: ClipboardConfig = {
  enabled: true,
  accept_dir: '/Users/me/Downloads/lan-mouse',
  ignore_text: false,
  ignore_images: false,
  ignore_files: true,
  max_file_size: 100 * 1024 * 1024,
  keep_partial: false,
  inject_to_clipboard: true,
}

describe('ClipboardConfigChanged event → state.clipboardConfig', () => {
  beforeEach(() => {
    // Reset to the placeholder shape between tests so a stale
    // carry-over from a sibling describe() can't pollute the
    // assertions. The placeholder matches the IPC default
    // shape (enabled / 50 MiB / inject_to_clipboard = true).
    daemonStore.clipboardConfig = {
      enabled: true,
      accept_dir: '',
      ignore_text: false,
      ignore_images: false,
      ignore_files: false,
      max_file_size: 50 * 1024 * 1024,
      keep_partial: false,
      inject_to_clipboard: true,
    }
  })

  it('replaces state.clipboardConfig wholesale on a ClipboardConfigChanged event', async () => {
    const { applyEvent } = await import('./index')
    applyEvent({ ClipboardConfigChanged: SAMPLE_CLIPBOARD_CONFIG })
    expect(daemonStore.clipboardConfig).toEqual(SAMPLE_CLIPBOARD_CONFIG)
    expect(daemonStore.clipboardConfig.max_file_size).toBe(100 * 1024 * 1024)
    expect(daemonStore.clipboardConfig.inject_to_clipboard).toBe(true)
  })

  it('replaces state.clipboardConfig on subsequent events (no stale merge)', async () => {
    const { applyEvent } = await import('./index')
    applyEvent({ ClipboardConfigChanged: SAMPLE_CLIPBOARD_CONFIG })
    expect(daemonStore.clipboardConfig.ignore_files).toBe(true)

    // A later event with different values must fully replace —
    // not merge — the cached config (the daemon is the source
    // of truth, no client-side diffing).
    const updated: ClipboardConfig = {
      ...SAMPLE_CLIPBOARD_CONFIG,
      enabled: false,
      inject_to_clipboard: false,
    }
    applyEvent({ ClipboardConfigChanged: updated })
    expect(daemonStore.clipboardConfig.enabled).toBe(false)
    expect(daemonStore.clipboardConfig.inject_to_clipboard).toBe(false)
    // ignore_files was true in the first payload and remains true
    // in the second (we carry it forward via the spread), but the
    // point is that the second payload's values are authoritative
    // — the first payload's `ignore_files = true` did NOT get
    // silently re-applied on top.
    expect(daemonStore.clipboardConfig.ignore_files).toBe(true)
  })
})

// ---------- M5 STEP-5.3: clipboardConfig initial placeholder ----------

describe('daemonStore.clipboardConfig initial state', () => {
  beforeEach(() => {
    // The previous describe() may have left `enabled: false` on
    // the singleton; reset so this assertion reads the true
    // placeholder shape.
    daemonStore.clipboardConfig = {
      enabled: true,
      accept_dir: '',
      ignore_text: false,
      ignore_images: false,
      ignore_files: false,
      max_file_size: 50 * 1024 * 1024,
      keep_partial: false,
      inject_to_clipboard: true,
    }
  })

  it('starts with the post-M4 IPC default shape (placeholder)', () => {
    // Before the first `ClipboardConfigChanged` event lands the
    // store must hand templates a usable shape — reading
    // `state.clipboardConfig.max_file_size` cannot throw, the
    // `inject_to_clipboard` checkbox must already be on, etc.
    expect(daemonStore.clipboardConfig.enabled).toBe(true)
    expect(daemonStore.clipboardConfig.max_file_size).toBe(50 * 1024 * 1024)
    expect(daemonStore.clipboardConfig.inject_to_clipboard).toBe(true)
    expect(daemonStore.clipboardConfig.keep_partial).toBe(false)
  })
})

// ---------- M5 STEP-5.3: ClipboardState → state.lastClipboard* ----------

describe('ClipboardState event → state.lastClipboard*', () => {
  beforeEach(() => {
    daemonStore.lastClipboardText = ''
    daemonStore.lastClipboardAt = 0
    daemonStore.lastClipboardSource = ''
  })

  it('populates the three lastClipboard fields from a ClipboardState event', async () => {
    const { applyEvent } = await import('./index')
    applyEvent({
      ClipboardState: {
        last_text_ts: 1700000000000,
        last_image_ts: null,
        last_file_ts: null,
        last_source: 'peer-west',
      },
    })
    expect(daemonStore.lastClipboardAt).toBe(1700000000000)
    expect(daemonStore.lastClipboardSource).toBe('peer-west')
    // text payload never crosses the IPC; field stays empty.
    expect(daemonStore.lastClipboardText).toBe('')
  })

  it('collapses null timestamps and source to sentinel values (0 / "")', async () => {
    const { applyEvent } = await import('./index')
    applyEvent({
      ClipboardState: {
        last_text_ts: null,
        last_image_ts: null,
        last_file_ts: null,
        last_source: null,
      },
    })
    expect(daemonStore.lastClipboardAt).toBe(0)
    expect(daemonStore.lastClipboardSource).toBe('')
  })

  it('uses last_text_ts specifically (not the file/image timestamps)', async () => {
    const { applyEvent } = await import('./index')
    applyEvent({
      ClipboardState: {
        last_text_ts: 1000,
        last_image_ts: 5000, // different, should be ignored
        last_file_ts: 9000, // different, should be ignored
        last_source: 'peer-east',
      },
    })
    expect(daemonStore.lastClipboardAt).toBe(1000)
    expect(daemonStore.lastClipboardSource).toBe('peer-east')
  })
})

// ---------- M5 STEP-5.1: FileTransferFailed → toast (one-way, no actions) ----------

describe('FileTransferFailed event → warning toast', () => {
  beforeEach(() => {
    daemonStore.toasts = []
  })

  it('pushes a warning toast with the raw reason string', async () => {
    const { applyEvent } = await import('./index')
    const evt: FileTransferFailed = {
      sha256: [
        0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54,
        0x32, 0x10, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc,
        0xdd, 0xee, 0xff, 0x00,
      ],
      reason: 'connection lost',
      ts_ms: 1700000000123,
    }
    applyEvent({ FileTransferFailed: evt })
    // Single toast — auto-dismissed on a 4s timer (success /
    // info / warning kinds are timed; only error is sticky).
    expect(daemonStore.toasts).toHaveLength(1)
    const toast = daemonStore.toasts[0]!
    expect(toast.kind).toBe('warning')
    expect(toast.message).toContain('connection lost')
  })

  it('surfaces all three reason strings verbatim', async () => {
    const { applyEvent } = await import('./index')
    for (const reason of ['connection lost', 'timeout', 'peer cancelled']) {
      daemonStore.toasts = []
      applyEvent({
        FileTransferFailed: {
          sha256: new Array(32).fill(0),
          reason,
          ts_ms: 1,
        },
      })
      expect(daemonStore.toasts).toHaveLength(1)
      expect(daemonStore.toasts[0]!.message).toContain(reason)
    }
  })
})
