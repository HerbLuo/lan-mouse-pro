<script setup lang="ts">
import { deleteClient, resolveDns, toggleClient, updateClientConfig } from '@/store'
import type { Connection } from '@/store'
import type { ChannelMode, ClientConfig, Position } from '@/api/ipc'
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
</script>

<template>
  <div class="card" :class="{ 'is-active': connection.state.active }">
    <div style="display: flex; justify-content: space-between;">
      <label class="switch connection-toggle">
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
        </div>
        <div
          class="meta"
          :class="{
            'meta-warn':
              connection.state.peer_commit && connection.state.peer_commit !== '????????',
            'meta-ok': connection.state.peer_commit && connection.state.peer_commit === '????????',
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

    <div v-if="connection.expanded" class="connection-body">
      <label>
        <span class="lbl">hostname</span>
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
        <span class="lbl">port</span>
        <input
          type="number"
          :value="connection.config.port"
          @change="
            setField({
              port: Number(($event.target as HTMLInputElement).value),
            })
          "
          placeholder="4242"
        />
      </label>
      <label>
        <span class="lbl">position</span>
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
      <label class="full">
        <span class="lbl">mouse button channel</span>
        <select
          :value="connection.config.input_channels.mouse_button"
          @change="setChannel('mouse_button', $event)"
        >
          <option value="datagram">Datagram (real-time)</option>
          <option value="stream">Stream (reliable)</option>
        </select>
        <span class="desc"
          >Datagram is lowest-latency and may drop clicks on a flaky link; Stream is reliable but
          may add head-of-line delay.</span
        >
      </label>
      <label class="full">
        <span class="lbl">keyboard channel</span>
        <select
          :value="connection.config.input_channels.keyboard"
          @change="setChannel('keyboard', $event)"
        >
          <option value="stream">Stream (reliable)</option>
          <option value="datagram">Datagram (real-time)</option>
        </select>
        <span class="desc"
          >Stream is reliable (no dropped keys); Datagram is the game-friendly low-latency choice if
          you tolerate occasional lost keystrokes.</span
        >
      </label>
      <div class="row-actions">
        <button class="danger" @click="deleteClient(connection.handle)">
          <IconTrash />
          delete this client
        </button>
      </div>
    </div>
  </div>
</template>
<style scoped>

</style>
