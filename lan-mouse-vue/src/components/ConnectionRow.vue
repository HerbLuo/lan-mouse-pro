<script setup lang="ts">
import { computed } from 'vue'
import { daemonStore, deleteClient, resolveDns, toggleClient, updateClientConfig } from '@/store'
import type { Connection } from '@/store'
import type { ChannelMode, ClientConfig, MonitorInfo, Position } from '@/api/ipc'
import IconChevron from '@/components/icons/IconChevron.vue'
import IconRefresh from '@/components/icons/IconRefresh.vue'
import IconTrash from '@/components/icons/IconTrash.vue'

const { connection } = defineProps<{ connection: Connection }>()

// Single setter for the three text/number/select fields. The store's
// `updateClientConfig` already diffs and only sends a WS request when
// the value actually changed, so callers don't need to guard repeats.
function setField(patch: Partial<ClientConfig>) {
  updateClientConfig(connection.handle, patch)
}

// Mouse/keyboard channel selectors share an identical structure:
// read the current InputChannelConfig, swap one key, send if changed.
function setChannel(key: 'mouse_button' | 'keyboard', ev: Event) {
  const v = (ev.target as HTMLSelectElement).value as ChannelMode
  const cfg = connection.config.input_channels
  if (cfg[key] === v) return
  setField({ input_channels: { ...cfg, [key]: v } })
}

/** **STEP-M3-3.2 — monitor `<select>` binding**.
 *
 *  `daemonStore.monitors` is the source of truth. The current row's
 *  binding is `connection.config.monitor` (string | null):
 *   - `null`   → "Any (back-compat)" — the legacy pre-M3 behavior;
 *     the daemon ignores the monitor dimension when computing the
 *     `BarrierKey`.
 *   - string  → must match one of `state.monitors[i].id`; the
 *     dropdown uses `id` as the `<option :value>` so a value match
 *     is a direct equality check. (If the daemon's binding points
 *     at an id that no longer exists, the row stays selected to
 *     that id but the dropdown displays no `<option>` for it —
 *     browsers render `<select>` as a blank box in that case, which
 *     is the visual cue to also expect a `BindingInvalid` badge.)
 *
 *  The tooltip on each option surfaces the monitor's geometry
 *  (position + size + scale) so the user can disambiguate two
 *  displays that share a name ("DELL U2415" × 2 in a row). */
const monitorOptions = computed(() => daemonStore.monitors)

function monitorLabel(m: MonitorInfo): string {
  const tag = m.primary ? ' (primary)' : ''
  return `${m.name}${tag}`
}

function monitorTooltip(m: MonitorInfo): string {
  return `${m.name}\nposition: (${m.position[0]}, ${m.position[1]})\nsize: ${m.size[0]} × ${m.size[1]}\nscale: ${m.scale}`
}

/** Convert `string | null` → the string value the `<select>` binds
 *  to. `null` is the empty string (matches the "Any" `<option>`). */
function monitorSelectValue(): string {
  return connection.config.monitor ?? ''
}

function setMonitor(ev: Event) {
  const v = (ev.target as HTMLSelectElement).value
  // empty string = "Any" sentinel; convert back to null so the
  // wire payload matches the Rust `Option<String>` shape and a
  // legacy pre-M3 config (which carries `null`) round-trips as a
  // no-op against itself.
  setField({ monitor: v === '' ? null : v })
}
</script>

<template>
  <div
    class="row"
    :class="{ invalid: connection.invalidReason != null }"
    :title="connection.invalidReason ?? ''"
  >
    <div :class="{ summary: true, expand: connection.expanded }">
      <div style="display: flex; justify-content: flex-start">
        <label class="connection-toggle" style="margin-right: 12px">
          <input
            type="checkbox"
            :checked="connection.state.active"
            @change="toggleClient(connection.handle, ($event.target as HTMLInputElement).checked)"
          />
          <span class="slider"></span>
        </label>
        <div class="title">
          <div class="name">
            {{ connection.config.hostname || `client #${connection.handle}` }}
            <!-- STEP-M2-2.6: badge surfaces the
                 BindingInvalid reason without forcing the user
                 to expand the row. The native `title=` on the
                 wrapping `<div class="row">` carries the same
                 string for the hover tooltip, so the badge
                 text and the tooltip stay in sync visually. -->
            <span
              v-if="connection.invalidReason"
              class="invalid-badge"
              :title="connection.invalidReason"
            >
              ⚠ invalid
            </span>
          </div>
          <div
            class="meta"
            :class="{
              'meta-warn':
                connection.state.peer_commit && connection.state.peer_commit !== '????????',
              'meta-ok':
                connection.state.peer_commit && connection.state.peer_commit === '????????',
            }"
          >
            <template v-if="connection.state.peer_commit">
              Peer version: {{ connection.state.peer_commit }} ·
              {{ connection.state.peer_commit === '????????' ? 'matched' : 'mismatch' }}
            </template>
            <template v-else>Peer version: unknown</template>
            ·
            <span v-if="connection.state.resolving">resolving…</span>
            <span v-else-if="connection.state.ips.length === 0">no addresses</span>
            <span v-else>{{ connection.state.ips.join(', ') }}</span>
          </div>
        </div>
      </div>

      <div style="display: flex; align-items: center">
        <button class="icon ghost" @click="resolveDns(connection.handle)" :title="'re-resolve DNS'">
          <IconRefresh />
        </button>

        <button
          class="icon ghost"
          @click="connection.expanded = !connection.expanded"
          :title="connection.expanded ? 'collapse' : 'expand'"
        >
          <span
            :style="{
              display: 'inline-flex',
              transform: connection.expanded ? 'rotate(180deg)' : 'rotate(0)',
              transition: 'transform 0.18s',
            }"
          >
            <IconChevron :size="16" />
          </span>
        </button>
      </div>
    </div>

    <div v-if="connection.expanded" class="connection-body">
      <div>
        <label>
          <span class="lbl">Hostname</span>
          <input
            type="text"
            :value="connection.config.hostname ?? ''"
            @change="
              setField({
                hostname: ($event.target as HTMLInputElement).value,
              })
            "
            placeholder="192.168.1.x or my-laptop"
          />
        </label>
        <label>
          <span class="lbl">Port</span>
          <input
            type="number"
            :value="connection.config.port"
            @change="
              setField({
                port: Number(($event.target as HTMLInputElement).value),
              })
            "
            placeholder="2268"
          />
        </label>
        <label>
          <span class="lbl">Position</span>
          <select
            :value="connection.config.pos"
            @change="
              setField({
                pos: ($event.target as HTMLSelectElement).value as Position,
              })
            "
          >
            <option value="left">Left</option>
            <option value="right">Right</option>
            <option value="top">Top</option>
            <option value="bottom">Bottom</option>
          </select>
        </label>
        <!-- STEP-M3-3.2: per-row monitor binding. Options are
             [Any (back-compat), ...state.monitors] in daemon order.
             The `<select>`'s `value` binds to the row's current
             monitor id (empty string for Any). Re-rendering
             happens automatically when `daemonStore.monitors`
             changes because Vue tracks the reactive read. -->
        <label>
          <span class="lbl">Monitor</span>
          <select
            :value="monitorSelectValue()"
            @change="setMonitor($event)"
            title="Bind this client to a specific monitor, or Any to inherit the legacy behavior"
          >
            <option value="" title="Any connected monitor — legacy pre-M3 behavior">
              Any (back-compat)
            </option>
            <option
              v-for="m in monitorOptions"
              :key="m.id"
              :value="m.id"
              :title="monitorTooltip(m)"
            >
              {{ monitorLabel(m) }}
            </option>
          </select>
        </label>
        <label class="full">
          <span class="lbl">Mouse button channel</span>
          <select
            :value="connection.config.input_channels.mouse_button"
            @change="setChannel('mouse_button', $event)"
          >
            <option value="datagram">Datagram (real-time)</option>
            <option value="stream">Stream (reliable)</option>
          </select>
          <span
            class="desc"
            tooltip="Datagram is lowest-latency and may drop clicks on a flaky link; Stream is reliable but may add head-of-line delay."
            >?</span
          >
        </label>
        <label class="full">
          <span class="lbl">Keyboard channel</span>
          <select
            :value="connection.config.input_channels.keyboard"
            @change="setChannel('keyboard', $event)"
          >
            <option value="stream">Stream (reliable)</option>
            <option value="datagram">Datagram (real-time)</option>
          </select>
          <span
            class="desc"
            tooltip="Stream is reliable (no dropped keys); Datagram is the game-friendly low-latency choice if you tolerate occasional lost keystrokes."
            >?</span
          >
        </label>
      </div>
      <div class="row-actions">
        <button class="danger" @click="deleteClient(connection.handle)">
          <IconTrash />
          Delete
        </button>
      </div>
    </div>
  </div>
</template>
<style scoped>
.row {
  margin-top: 16px;
  border: var(--border);
  border-radius: 2px;
  padding: 12px;
}
/* STEP-M2-2.6: visually flag a row whose bound monitor
   disappeared. Red outline + faint pink fill so the badge
   reads even on a low-contrast monitor, and the cursor
   `cursor: not-allowed` over the toggle signals "you can't
   re-arm this until you do something". The fill is light
   enough to leave the row text readable. */
.row.invalid {
  border: 1px solid #d33;
  background: rgba(221, 51, 51, 0.06);
}
.row.invalid .connection-toggle {
  cursor: not-allowed;
  opacity: 0.55;
}
/* Inline badge next to the client name; the wrapping
   `<div class="row">` also carries the same `title=` so
   the OS-native hover tooltip shows the full reason. */
.invalid-badge {
  display: inline-block;
  margin-left: 8px;
  padding: 1px 6px;
  border-radius: 8px;
  background: #d33;
  color: #fff;
  font-size: 11px;
  font-weight: 600;
  vertical-align: middle;
}
.summary {
  display: flex;
  justify-content: space-between;
}
.expand {
  border-bottom: var(--border);
  padding-bottom: 12px;
}
.connection-body {
  display: flex;
  padding: 12px;
}
.connection-body-left {
  display: flex;
  flex-direction: column;
}
.connection-body label {
  margin-bottom: 6px;
}
.connection-body label .lbl {
  display: inline-block;
  margin-right: 6px;
  width: 200px;
}
.row-actions {
  display: flex;
  align-items: center;
}
.row-actions button {
  height: 66px;
}
.desc {
  display: inline-flex;
  justify-content: center;
  align-items: center;
  font-size: 12px;
  width: 14px;
  height: 14px;
  margin-left: 6px;
  border-radius: 50%;
  border: 1px solid var(--fg-default);
}
</style>
