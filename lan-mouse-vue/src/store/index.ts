import { reactive, ref, type Ref as VRef } from 'vue'
import type {
  ClientConfig,
  ClientHandle,
  ClientState,
  ConnState,
  DaemonSocket,
  FrontendEvent,
  InitialInfo,
  Position,
  Status,
  Toast,
  ToastKind,
} from '../api/ipc'
import { DaemonSocket as RealDaemonSocket } from '../api/ipc'

export interface Connection {
  handle: ClientHandle
  config: ClientConfig
  state: ClientState
  expanded: boolean
}

export interface DaemonStore {
  port: number
  portError: string | null
  fingerprint: string
  clients: Map<ClientHandle, Connection>
  authorized: Record<string, string>
  captureStatus: Status
  emulationStatus: Status
  /** Fingerprint awaiting user approval (from a ConnectionAttempt
   *  event). Rendered inline as a banner in the Incoming panel. */
  pendingConnectionAttempt: string | null
  toasts: Toast[]
  /** Daemon-provided boot info (hostname, LAN IPs, web port).
   *  Populated once at startup via `fetch('/api/info')`. */
  info: InitialInfo | null
  /** QUIC `max_idle_timeout` in seconds, last echoed by the daemon
   *  via the `QuicConfig` event. Defaults to 5 to match the
   *  daemon-side default — overwritten by the first `QuicConfig`
   *  event after WS open. Changes via `setQuicIdleTimeout` only
   *  take effect on the next daemon restart. */
  quicIdleTimeoutSecs: number
}

const state = reactive<DaemonStore>({
  port: 0,
  portError: null,
  fingerprint: '',
  clients: new Map(),
  authorized: {},
  captureStatus: 'Disabled',
  emulationStatus: 'Disabled',
  pendingConnectionAttempt: null,
  toasts: [],
  info: null,
  quicIdleTimeoutSecs: 5,
})

/** Mirrors the daemon connectivity state for the AppHeader pill.
 *
 *  Transitions:
 *    (start)          → `Fail`
 *    WS `onopen`      → `Prepared`
 *    WS `onclose`     → `Fail`
 *    QUIC peer event  → `Paired` (latched; survives peer disconnect)
 *    WS reconnect     → resets to `Prepared` (peer has to re-pair)
 *
 *  Kept separate from `daemonStore` because it's a single typed
 *  primitive the templates bind to, and we want it cheap to update
 *  without triggering deep reactivity on the whole store. */
const connState: VRef<ConnState> = ref('Fail')

let nextToastId = 1

function pushToast(kind: ToastKind, message: string) {
  const id = nextToastId++
  state.toasts.push({ id, kind, message })
  if (kind !== 'error') {
    setTimeout(() => dismissToast(id), 4000)
  }
}

function dismissToast(id: number) {
  const idx = state.toasts.findIndex((t) => t.id === id)
  if (idx >= 0) state.toasts.splice(idx, 1)
}

function mergeClient(handle: ClientHandle, config: ClientConfig, cs: ClientState) {
  const existing = state.clients.get(handle)
  if (existing) {
    existing.config = config
    existing.state = cs
  } else {
    state.clients.set(handle, {
      handle,
      config,
      state: cs,
      expanded: false,
    })
  }
}

function applyEvent(event: FrontendEvent) {
  const key = Object.keys(event)[0] as keyof FrontendEvent
  const value = (event as Record<string, unknown>)[key]

  switch (key) {
    case 'Created':
    case 'State': {
      const [handle, cfg, st] = value as [ClientHandle, ClientConfig, ClientState]
      mergeClient(handle, cfg, st)
      if (key === 'Created') pushToast('success', `added client ${cfg.hostname ?? handle}`)
      break
    }
    case 'Deleted': {
      const handle = value as ClientHandle
      const conn = state.clients.get(handle)
      if (conn) {
        pushToast('info', `removed client ${conn.config.hostname ?? handle}`)
        state.clients.delete(handle)
      }
      break
    }
    case 'Enumerate': {
      const list = value as Array<[ClientHandle, ClientConfig, ClientState]>
      state.clients.clear()
      for (const [h, cfg, st] of list) mergeClient(h, cfg, st)
      // If the daemon already has any peer with an active QUIC
      // session when we connect, the DeviceConnected event we
      // normally latch on has already fired before our WS opened.
      // Pick it up here so the badge jumps straight to `Paired`
      // instead of getting stuck in `Prepared`.
      if (list.some(([, , cs]) => cs.active)) {
        connState.value = 'Paired'
      }
      break
    }
    case 'PortChanged': {
      const [port, err] = value as [number, string | null]
      state.port = port
      state.portError = err
      if (err) pushToast('warning', `port change failed: ${err}`)
      else if (port !== 0) pushToast('success', `listening on port ${port}`)
      break
    }
    case 'Error':
      pushToast('error', String(value))
      break
    case 'CaptureStatus':
      state.captureStatus = value as Status
      break
    case 'EmulationStatus':
      state.emulationStatus = value as Status
      break
    case 'AuthorizedUpdated':
      state.authorized = { ...(value as Record<string, string>) }
      break
    case 'PublicKeyFingerprint':
      state.fingerprint = String(value)
      break
    case 'DeviceConnected':
      // Peer completed its QUIC handshake — latch the badge into
      // `Paired`. Idempotent: a second peer connecting later is a
      // no-op.
      connState.value = 'Paired'
      pushToast('success', `device connected: ${(value as any).addr}`)
      break
    case 'DeviceEntered':
      // Same as DeviceConnected from the badge's perspective — the
      // peer session is alive enough for `Enter` to fire.
      connState.value = 'Paired'
      pushToast('info', `device entered screen (${(value as any).pos})`)
      break
    case 'IncomingDisconnected':
      pushToast('info', `peer disconnected`)
      break
    case 'ConnectionAttempt':
      state.pendingConnectionAttempt = (value as { fingerprint: string }).fingerprint
      break
    case 'NoSuchClient':
      break
    case 'QuicConfig':
      // Echo from the daemon — either the initial sync after WS open
      // or a confirmation of a `setQuicIdleTimeout` write. The
      // template re-syncs its draft via the `watch()` in
      // GeneralPanel.vue.
      state.quicIdleTimeoutSecs = (value as { idle_timeout_secs: number }).idle_timeout_secs
      break
  }
}

// ---- socket singleton ------------------------------------------------

let socket: DaemonSocket | null = null

export function initSocket() {
  const url = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}/ws`
  const impl = new RealDaemonSocket(url, connState, applyEvent)
  impl.connect()
  socket = impl
}

/** One-shot fetch of the daemon's `InitialInfo`. Called from
 *  `main.ts` on boot so the first Vue render already has the LAN IP
 *  + port, no waiting for any FrontendEvent round-trip. Best-effort:
 *  on failure we leave `state.info = null` and the template renders
 *  placeholder dashes. */
export async function loadInfo() {
  try {
    const res = await fetch('/api/info')
    if (!res.ok) throw new Error(`status ${res.status}`)
    state.info = (await res.json()) as InitialInfo
  } catch (e) {
    console.warn('could not load /api/info:', e)
  }
}

export function getSocket(): DaemonSocket {
  if (!socket) throw new Error('socket not initialised')
  return socket
}

export const daemonStore = state
export const connStateRef = connState

// ---- request helpers ------------------------------------------------

export function toggleClient(handle: ClientHandle, active: boolean) {
  getSocket().request({ Activate: [handle, active] })
}

export function deleteClient(handle: ClientHandle) {
  getSocket().request({ Delete: handle })
}

export function addClient() {
  getSocket().request({ Create: null })
}

export function changePort(port: number) {
  getSocket().request({ ChangePort: port })
}

/** Persist a new QUIC `idle_timeout_secs`. The daemon writes it to
 *  TOML and echoes the value back via the `QuicConfig` event; the
 *  running endpoint is not rebuilt until the daemon restarts. */
export function setQuicIdleTimeout(secs: number) {
  getSocket().request({ SetQuicIdleTimeout: secs })
}

export function enableCapture() {
  getSocket().request({ EnableCapture: null })
}

export function enableEmulation() {
  getSocket().request({ EnableEmulation: null })
}

export function authorizeKey(desc: string, fp: string) {
  getSocket().request({ AuthorizeKey: [desc, fp] })
}

export function removeAuthorizedKey(fp: string) {
  getSocket().request({ RemoveAuthorizedKey: fp })
}

export function updateClientConfig(handle: ClientHandle, patch: Partial<ClientConfig>) {
  const conn = state.clients.get(handle)
  if (!conn) return
  if (patch.hostname !== undefined && patch.hostname !== conn.config.hostname)
    getSocket().request({
      UpdateHostname: [handle, patch.hostname || null],
    })
  if (patch.port !== undefined && patch.port !== conn.config.port)
    getSocket().request({ UpdatePort: [handle, patch.port] })
  if (patch.pos !== undefined && patch.pos !== conn.config.pos)
    getSocket().request({ UpdatePosition: [handle, patch.pos] })
  if (patch.input_channels !== undefined)
    getSocket().request({
      SetClientInputChannels: [handle, patch.input_channels],
    })
}

export function resolveDns(handle: ClientHandle) {
  getSocket().request({ ResolveDns: handle })
}

export function acceptConnection(fp: string, desc: string) {
  authorizeKey(desc, fp)
  state.pendingConnectionAttempt = null
}

export function rejectConnection() {
  state.pendingConnectionAttempt = null
}

export { pushToast, dismissToast }
export const useStore = () => state
