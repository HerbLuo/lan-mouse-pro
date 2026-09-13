import { describe, expect, it, beforeEach } from 'vitest'
import { mount } from '@vue/test-utils'
import GeneralPanel from './GeneralPanel.vue'
import { daemonStore } from '@/store'
import type { ClipboardConfig } from '@/api/ipc'

/** M5 STEP-5.4 — clipboard section in GeneralPanel.
 *
 *  Covers:
 *    - Eight controls render with stable `data-testid` attrs
 *      (so future snapshot / contract tests can lock the shape).
 *    - Each draft ref initializes from `state.clipboardConfig`.
 *    - MiB → bytes conversion (`100 MiB → 104,857,600` on the wire).
 *    - The `inject_to_clipboard` checkbox wires through to a
 *      `SetClipboardConfig` IPC request via the test seam.
 *
 *  The clipboard section lives inside `GeneralPanel` (it also
 *  has the host/port, QUIC idle, and fingerprint blocks), so we
 *  mount the whole panel and drill into the `[data-testid]`
 *  scope — keeps the test honest about the section's placement
 *  in the DOM. */

const CLIPBOARD_DEFAULTS: ClipboardConfig = {
  enabled: true,
  accept_dir: '/Users/me/Downloads/lan-mouse',
  ignore_text: false,
  ignore_images: false,
  ignore_files: false,
  max_file_size: 50 * 1024 * 1024,
  keep_partial: false,
  inject_to_clipboard: true,
}

beforeEach(() => {
  daemonStore.clipboardConfig = { ...CLIPBOARD_DEFAULTS }
  daemonStore.port = 2268
  daemonStore.info = null
  daemonStore.fingerprint = ''
})

function mountPanel() {
  return mount(GeneralPanel)
}

describe('GeneralPanel clipboard section', () => {
  it('renders all 8 controls with stable testids', () => {
    const wrapper = mountPanel()
    const section = wrapper.find('[data-testid="clipboard-section"]')
    expect(section.exists()).toBe(true)

    expect(wrapper.find('[data-testid="clipboard-enabled"]').exists()).toBe(true)
    expect(wrapper.find('[data-testid="clipboard-accept-dir"]').exists()).toBe(true)
    expect(wrapper.find('[data-testid="clipboard-ignore-text"]').exists()).toBe(true)
    expect(wrapper.find('[data-testid="clipboard-ignore-images"]').exists()).toBe(true)
    expect(wrapper.find('[data-testid="clipboard-ignore-files"]').exists()).toBe(true)
    expect(wrapper.find('[data-testid="clipboard-max-file-size"]').exists()).toBe(true)
    expect(wrapper.find('[data-testid="clipboard-keep-partial"]').exists()).toBe(true)
    expect(wrapper.find('[data-testid="clipboard-inject-to-clipboard"]').exists()).toBe(true)
  })

  it('mirrors state.clipboardConfig into the form drafts on mount', () => {
    daemonStore.clipboardConfig = {
      ...CLIPBOARD_DEFAULTS,
      enabled: false,
      ignore_files: true,
      max_file_size: 100 * 1024 * 1024, // 100 MiB on the wire
      inject_to_clipboard: false,
    }
    const wrapper = mountPanel()
    const enabledEl = wrapper.find(
      '[data-testid="clipboard-enabled"]',
    ).element as HTMLInputElement
    const acceptEl = wrapper.find(
      '[data-testid="clipboard-accept-dir"]',
    ).element as HTMLInputElement
    const ignoreFilesEl = wrapper.find(
      '[data-testid="clipboard-ignore-files"]',
    ).element as HTMLInputElement
    const maxEl = wrapper.find(
      '[data-testid="clipboard-max-file-size"]',
    ).element as HTMLInputElement
    const injectEl = wrapper.find(
      '[data-testid="clipboard-inject-to-clipboard"]',
    ).element as HTMLInputElement

    expect(enabledEl.checked).toBe(false)
    expect(acceptEl.value).toBe('/Users/me/Downloads/lan-mouse')
    expect(ignoreFilesEl.checked).toBe(true)
    // 100 MiB in bytes (100 * 1024 * 1024 = 104,857,600) → 100 MiB UI
    expect(maxEl.value).toBe('100')
    expect(injectEl.checked).toBe(false)
  })

  it('display max_file_size as MiB (50 MiB default = 52428800 bytes)', () => {
    // Default = 50 MiB → wire shape is 52428800 bytes → UI shows "50".
    const wrapper = mountPanel()
    const el = wrapper.find(
      '[data-testid="clipboard-max-file-size"]',
    ).element as HTMLInputElement
    expect(el.value).toBe('50')
  })

  it('re-syncs drafts when state.clipboardConfig changes (daemon echo)', async () => {
    const wrapper = mountPanel()
    // Simulate the daemon echoing back a 200 MiB cap with inject off.
    daemonStore.clipboardConfig = {
      ...CLIPBOARD_DEFAULTS,
      max_file_size: 200 * 1024 * 1024,
      inject_to_clipboard: false,
    }
    await wrapper.vm.$nextTick()

    const maxEl = wrapper.find(
      '[data-testid="clipboard-max-file-size"]',
    ).element as HTMLInputElement
    const injectEl = wrapper.find(
      '[data-testid="clipboard-inject-to-clipboard"]',
    ).element as HTMLInputElement
    expect(maxEl.value).toBe('200')
    expect(injectEl.checked).toBe(false)
  })

  it('MiB → bytes conversion: input of 100 commits 104857600 bytes on the wire', async () => {
    // Drive the `setClipboardConfig` helper via the test seam.
    const requests: any[] = []
    const { _setSocketForTest } = await import('@/store')
    _setSocketForTest!({ request: (r: any) => requests.push(r) } as any)
    try {
      const wrapper = mountPanel()
      const maxEl = wrapper.find(
        '[data-testid="clipboard-max-file-size"]',
      ).element as HTMLInputElement
      // Type 100 MiB, then dispatch the `@change` event manually
      // (vue-test-utils' `setValue` doesn't fire `@change` for
      // number inputs — only `input`).
      maxEl.value = '100'
      await wrapper.find('[data-testid="clipboard-max-file-size"]').trigger('change')

      expect(requests).toHaveLength(1)
      expect(requests[0].SetClipboardConfig.max_file_size).toBe(100 * 1024 * 1024)
      expect(requests[0].SetClipboardConfig.max_file_size).toBe(104857600)
    } finally {
      _setSocketForTest!(null)
    }
  })

  it('MiB → bytes: 0 = "no limit" sentinel preserved verbatim on the wire', async () => {
    const requests: any[] = []
    const { _setSocketForTest } = await import('@/store')
    _setSocketForTest!({ request: (r: any) => requests.push(r) } as any)
    try {
      const wrapper = mountPanel()
      const maxEl = wrapper.find(
        '[data-testid="clipboard-max-file-size"]',
      ).element as HTMLInputElement
      maxEl.value = '0'
      await wrapper.find('[data-testid="clipboard-max-file-size"]').trigger('change')

      expect(requests).toHaveLength(1)
      expect(requests[0].SetClipboardConfig.max_file_size).toBe(0)
    } finally {
      _setSocketForTest!(null)
    }
  })

  it('MiB → bytes: negative input snaps back to 0 (wire "no limit" sentinel)', async () => {
    const requests: any[] = []
    const { _setSocketForTest } = await import('@/store')
    _setSocketForTest!({ request: (r: any) => requests.push(r) } as any)
    try {
      const wrapper = mountPanel()
      const maxEl = wrapper.find(
        '[data-testid="clipboard-max-file-size"]',
      ).element as HTMLInputElement
      maxEl.value = '-5'
      await wrapper.find('[data-testid="clipboard-max-file-size"]').trigger('change')

      expect(requests).toHaveLength(1)
      // Negative → commitMaxFileSize clamps the draft to 0, so the
      // wire sees 0 bytes (the daemon's "no limit" sentinel).
      expect(requests[0].SetClipboardConfig.max_file_size).toBe(0)
      // And the DOM input is also snapped back.
      expect((wrapper.find('[data-testid="clipboard-max-file-size"]').element as HTMLInputElement).value).toBe('0')
    } finally {
      _setSocketForTest!(null)
    }
  })

  it('commitClipboard sends the full 8-field payload on every checkbox / input change', async () => {
    const requests: any[] = []
    const { _setSocketForTest } = await import('@/store')
    _setSocketForTest!({ request: (r: any) => requests.push(r) } as any)
    try {
      const wrapper = mountPanel()
      // Toggle the inject_to_clipboard checkbox.
      await wrapper.find('[data-testid="clipboard-inject-to-clipboard"]').setValue(false)

      expect(requests).toHaveLength(1)
      const sent = requests[0].SetClipboardConfig
      expect(sent).toBeDefined()
      expect(sent.enabled).toBe(true)
      expect(sent.accept_dir).toBe('/Users/me/Downloads/lan-mouse')
      expect(sent.ignore_text).toBe(false)
      expect(sent.ignore_images).toBe(false)
      expect(sent.ignore_files).toBe(false)
      expect(sent.max_file_size).toBe(50 * 1024 * 1024) // 50 MiB default
      expect(sent.keep_partial).toBe(false)
      expect(sent.inject_to_clipboard).toBe(false) // toggled
    } finally {
      _setSocketForTest!(null)
    }
  })
})