import { describe, expect, it, beforeEach } from 'vitest'
import { mount } from '@vue/test-utils'
import ConnectionRow from './ConnectionRow.vue'
import { daemonStore, diffClientConfigPatch } from '@/store'
import type { Connection } from '@/store'
import type { ClientConfig, ClientState, MonitorInfo, ClientHandle } from '@/api/ipc'

/** STEP-M3-3.2 snapshot / structural tests for the monitor `<select>`.
 *
 *  Three cases from the §8 test matrix:
 *   - **single monitor**   → dropdown shows [Any, Built-in]
 *   - **multi monitor**    → dropdown shows [Any, primary, secondary]
 *   - **legacy config**    → row.config.monitor is null; dropdown
 *                            selected value is the empty string
 *                            (= the "Any" option)
 *
 *  Plus a fourth case for completeness: a row bound to a monitor id
 *  that is still in the list selects that id (drives the
 *  `monitorSelectValue()` mapping function).
 *
 *  Avoids `initSocket()` / `getSocket()` — we don't drive the live
 *  WS connection. The Vue store singleton is enough; we mount
 *  ConnectionRow with a hand-built `Connection` and inspect the
 *  rendered DOM.
 */

function makeConnection(monitor: string | null): Connection {
  const config: ClientConfig = {
    hostname: 'peer-east',
    fix_ips: [],
    port: 2268,
    pos: 'right',
    cmd: null,
    input_channels: { mouse_button: 'datagram', keyboard: 'stream' },
    monitor,
  }
  const state: ClientState = {
    active: false,
    active_addr: null,
    dns_ips: [],
    ips: ['10.0.0.2'],
    has_pressed_keys: false,
    resolving: false,
    peer_commit: '????????',
  }
  return {
    handle: 7 as ClientHandle,
    config,
    state,
    expanded: true,
    invalidReason: null,
  }
}

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

/** Return the `<select>` whose `<option>`s include "Any (back-compat)" —
 *  that's the monitor dropdown. Other selects in the row are
 *  Position / Mouse channel / Keyboard channel; their first
 *  `<option>` is "Left" / "Datagram (real-time)" / "Stream
 *  (reliable)" respectively. */
function getMonitorSelect(wrapper: ReturnType<typeof mount>) {
  const all = wrapper.findAll('select')
  const monitorSelect = all.find((s) => {
    const first = s.find('option')
    return first.exists() && /Any \(back-compat\)/.test(first.text())
  })
  if (!monitorSelect) {
    throw new Error('monitor <select> not found')
  }
  return monitorSelect
}

describe('ConnectionRow monitor dropdown', () => {
  beforeEach(() => {
    daemonStore.monitors = []
  })

  it('case A — single monitor: shows [Any, Built-in] (1 + 1 = 2 options)', () => {
    daemonStore.monitors = SINGLE
    const wrapper = mount(ConnectionRow, { props: { connection: makeConnection(null) } })
    const sel = getMonitorSelect(wrapper)
    expect(sel.findAll('option')).toHaveLength(2)
    const options = sel.findAll('option').map((o) => o.text())
    expect(options[0]).toMatch(/Any \(back-compat\)/)
    expect(options[1]).toBe('Built-in (primary)')
    // Default selected value is the empty string (Any).
    expect((sel.element as HTMLSelectElement).value).toBe('')
  })

  it('case B — multi monitor: shows [Any, Built-in (primary), External]', () => {
    daemonStore.monitors = DUAL
    const wrapper = mount(ConnectionRow, { props: { connection: makeConnection(null) } })
    const sel = getMonitorSelect(wrapper)
    expect(sel.findAll('option')).toHaveLength(3)
    const options = sel.findAll('option').map((o) => o.text())
    expect(options[0]).toMatch(/Any \(back-compat\)/)
    expect(options[1]).toBe('Built-in (primary)')
    expect(options[2]).toBe('External')
  })

  it('case C — legacy config (monitor: null): dropdown defaults to "Any"', () => {
    daemonStore.monitors = DUAL
    const wrapper = mount(ConnectionRow, {
      props: { connection: makeConnection(null) },
    })
    const sel = getMonitorSelect(wrapper)
    expect((sel.element as HTMLSelectElement).value).toBe('')
    // Selecting the empty-string option must round-trip to
    // null via `diffClientConfigPatch` (not an empty string,
    // which the daemon would treat as a real — but empty —
    // monitor id).
    const requests = diffClientConfigPatch(7, makeConnection(null).config, {
      monitor: '',
    })
    expect(requests).toEqual([])
    // But selecting null explicitly via `setField({ monitor: null })`
    // after the row had a binding → emits UpdateMonitor with null.
    const requests2 = diffClientConfigPatch(7, makeConnection('CGDisplay:1').config, {
      monitor: null,
    })
    expect(requests2).toEqual([{ UpdateMonitor: [7, null] }])
  })

  it('case D — row bound to a specific monitor id selects that id', () => {
    daemonStore.monitors = DUAL
    const wrapper = mount(ConnectionRow, {
      props: { connection: makeConnection('CGDisplay:2') },
    })
    const sel = getMonitorSelect(wrapper)
    expect((sel.element as HTMLSelectElement).value).toBe('CGDisplay:2')
  })

  it('emits no UpdateMonitor request when the user re-selects the same monitor', () => {
    // Pins the diffClientConfigPatch contract that the
    // dropdown depends on for "no-op on reselect" UX.
    const current = makeConnection('CGDisplay:1').config
    const out = diffClientConfigPatch(7, current, { monitor: 'CGDisplay:1' })
    expect(out).toEqual([])
  })

  it('emits UpdateMonitor when the user picks a different monitor', () => {
    const current = makeConnection('CGDisplay:1').config
    const out = diffClientConfigPatch(7, current, { monitor: 'CGDisplay:2' })
    expect(out).toEqual([{ UpdateMonitor: [7, 'CGDisplay:2'] }])
  })

  it('option tooltips expose position / size / scale (helps when two displays share a name)', () => {
    daemonStore.monitors = DUAL
    const wrapper = mount(ConnectionRow, {
      props: { connection: makeConnection(null) },
    })
    const sel = getMonitorSelect(wrapper)
    const opts = sel.findAll('option')
    // The first option is the "Any" sentinel.
    const builtIn = opts[1]!
    expect(builtIn.attributes('title')).toContain('position: (0, 0)')
    expect(builtIn.attributes('title')).toContain('2560 × 1600')
    expect(builtIn.attributes('title')).toContain('scale: 2')
    const external = opts[2]!
    expect(external.attributes('title')).toContain('position: (2560, 0)')
    expect(external.attributes('title')).toContain('1920 × 1080')
    expect(external.attributes('title')).toContain('scale: 1')
  })

  it('rendering falls back to the legacy single-option dropdown when monitors is empty', () => {
    daemonStore.monitors = []
    const wrapper = mount(ConnectionRow, {
      props: { connection: makeConnection(null) },
    })
    const sel = getMonitorSelect(wrapper)
    const opts = sel.findAll('option')
    expect(opts).toHaveLength(1)
    expect(opts[0]!.text()).toMatch(/Any \(back-compat\)/)
  })
})
