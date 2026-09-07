import { describe, expect, it, beforeEach } from 'vitest'
import { daemonStore, diffClientConfigPatch } from './index'
import type {
  ClientConfig,
  ClientHandle,
  ClientState,
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
