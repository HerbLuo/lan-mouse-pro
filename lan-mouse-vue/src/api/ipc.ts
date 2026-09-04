// TypeScript counterparts to lan_mouse_ipc::FrontendEvent / FrontendRequest.
// We don't import them directly (the Rust side serializes them with
// serde; the .ts side mirrors the JSON shape). Keeping the shapes here
// means we don't need a build-time codegen step and the wire format
// stays the single source of truth on the Rust side.

import type { Ref } from 'vue'

export type Position = 'left' | 'right' | 'top' | 'bottom'

export type ChannelMode = 'stream' | 'datagram'

export interface InputChannelConfig {
  mouse_button: ChannelMode
  keyboard: ChannelMode
}

export interface ClientConfig {
  hostname: string | null
  fix_ips: string[]
  port: number
  pos: Position
  cmd: string | null
  input_channels: InputChannelConfig
}

export interface ClientState {
  active: boolean
  active_addr: string | null
  dns_ips: string[]
  ips: string[]
  has_pressed_keys: boolean
  resolving: boolean
  /** 8-char ASCII short commit, or null if peer hasn't sent Hello yet. */
  peer_commit: string | null
}

export type ClientHandle = number

export type Status = 'Enabled' | 'Disabled'

export type ToastKind = 'info' | 'success' | 'warning' | 'error'

export interface Toast {
  id: number
  kind: ToastKind
  message: string
}

export interface InitialInfo {
  hostname: string
  web_port: number
  /** Outbound-facing IPv4 the daemon picked up via the UDP-probe
   *  trick. Empty string if the host is offline or only has IPv6. */
  primary_ip: string
  /** Every non-loopback IPv4/IPv6 the OS exposes, IPv4 first. The
   *  user picks the right one when more than one NIC is live. */
  all_ips: string[]
}

/** Connection state machine for the AppHeader status pill.
 *
 *  - `Fail`     — the WebSocket bridge to the daemon is dead. Either
 *                 the initial handshake never completed or it
 *                 disconnected and the reconnect timer hasn't yet
 *                 restored it. Nothing in the UI is authoritative.
 *  - `Prepared` — the WebSocket is up but no QUIC peer has
 *                 successfully completed its handshake yet. The
 *                 daemon is reachable and answering requests; the
 *                 LAN path to peers just hasn't been exercised.
 *  - `Paired`   — at least one peer has completed a QUIC handshake
 *                 during this WS session (`DeviceConnected` or
 *                 `DeviceEntered` arrived). Latched: the badge
 *                 stays in this state even if every peer later
 *                 disconnects, because it proves the LAN path is
 *                 working. Resets to `Prepared` on WS reconnect. */
export type ConnState = 'Fail' | 'Prepared' | 'Paired'

export type FrontendEvent =
  | { Created: [ClientHandle, ClientConfig, ClientState] }
  | { NoSuchClient: ClientHandle }
  | { State: [ClientHandle, ClientConfig, ClientState] }
  | { Deleted: ClientHandle }
  | { PortChanged: [number, string | null] }
  | { Enumerate: Array<[ClientHandle, ClientConfig, ClientState]> }
  | { Error: string }
  | { CaptureStatus: Status }
  | { EmulationStatus: Status }
  | { AuthorizedUpdated: Record<string, string> }
  | { PublicKeyFingerprint: string }
  | {
      DeviceConnected: { addr: string; fingerprint: string }
    }
  | {
      DeviceEntered: { fingerprint: string; addr: string; pos: Position }
    }
  | { IncomingDisconnected: string }
  | { ConnectionAttempt: { fingerprint: string } }
  | { QuicConfig: { idle_timeout_secs: number } }

export type FrontendRequest =
  | { Activate: [ClientHandle, boolean] }
  | { Create: null }
  | { ChangePort: number }
  | { Delete: ClientHandle }
  | { Enumerate: null }
  | { ResolveDns: ClientHandle }
  | { UpdateHostname: [ClientHandle, string | null] }
  | { UpdatePort: [ClientHandle, number] }
  | { UpdatePosition: [ClientHandle, Position] }
  | { UpdateFixIps: [ClientHandle, string[]] }
  | { EnableCapture: null }
  | { EnableEmulation: null }
  | { Sync: null }
  | { AuthorizeKey: [string, string] }
  | { RemoveAuthorizedKey: string }
  | { UpdateEnterHook: [number, string | null] }
  | { SetClientInputChannels: [ClientHandle, InputChannelConfig] }
  | { SaveConfiguration: null }
  | { SetQuicIdleTimeout: number }

/**
 * Reactive WebSocket connection to the daemon's `/ws` endpoint.
 * Calls `onEvent` for every received FrontendEvent and exposes
 * `request()` to send FrontendRequests.
 */
export class DaemonSocket {
  private ws: WebSocket | null = null
  private connState: Ref<ConnState>
  private reconnectTimer: number | null = null
  private reconnectDelay = 1000

  constructor(
    private url: string,
    connState: Ref<ConnState>,
    private onEvent: (event: FrontendEvent) => void,
  ) {
    this.connState = connState
  }

  connect() {
    this.ws = new WebSocket(this.url)
    this.ws.onopen = () => {
      // Reset to Prepared on every fresh handshake. Any "Paired"
      // badge from the previous session is forgotten — the new
      // session has to prove its own QUIC peer connectivity before
      // we upgrade the indicator again.
      this.connState.value = 'Prepared'
      this.reconnectDelay = 1000
      // Ask the daemon for a fresh full-state dump so we don't have
      // to wait for any in-flight Enumerate from boot.
      this.request({ Sync: null })
    }
    this.ws.onmessage = (ev) => {
      try {
        const parsed = JSON.parse(ev.data) as FrontendEvent
        this.onEvent(parsed)
      } catch (e) {
        console.warn('invalid event from daemon:', e)
      }
    }
    this.ws.onclose = () => {
      this.connState.value = 'Fail'
      this.scheduleReconnect()
    }
    this.ws.onerror = () => {
      // onclose will fire next; reconnect handled there.
    }
  }

  private scheduleReconnect() {
    if (this.reconnectTimer != null) return
    const delay = Math.min(this.reconnectDelay, 15000)
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = null
      this.reconnectDelay = Math.min(this.reconnectDelay * 2, 15000)
      this.connect()
    }, delay)
  }

  request(req: FrontendRequest) {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) {
      console.warn('socket not open, dropping request', req)
      return
    }
    this.ws.send(JSON.stringify(req))
  }
}
