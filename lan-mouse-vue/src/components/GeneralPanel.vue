<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { changePort, daemonStore, enableCapture, enableEmulation, pushToast } from '@/store'
import IconCopy from '@/components/icons/IconCopy.vue'
import IconChevron from '@/components/icons/IconChevron.vue'
import IconMic from '@/components/icons/IconMic.vue'
import IconWarning from '@/components/icons/IconWarning.vue'
import { copyToClipboard, validatePort } from '@/utils/utils'

// ---- local UI state ----------------------------------------------

const portDraft = ref<number>(daemonStore.port)

watch(
  () => daemonStore.port,
  (v) => (portDraft.value = v),
)

// The IP we display in the General panel. Initialised from the
// daemon-provided `primary_ip` and stays in sync with whatever the
// user overrides via the IP dropdown (when more than one NIC is
// live). If the host has zero IPs we fall back to the system
// hostname so the field is never empty.
const selectedIp = ref<string>('')

// When `info` finishes loading, seed `selectedIp` with the daemon's
// preferred address. The watcher below keeps it in sync if the user
// explicitly picks a different NIC.
watch(
  () => daemonStore.info,
  (info) => {
    if (!info) return
    if (selectedIp.value === '' || !info.all_ips.includes(selectedIp.value)) {
      selectedIp.value = info.primary_ip || info.hostname
    }
  },
  { immediate: true },
)

// What we actually show in the "hostname & port" input.
const displayIp = computed(() => {
  if (selectedIp.value) return selectedIp.value
  return daemonStore.info?.primary_ip || daemonStore.info?.hostname || ''
})

// True iff the daemon reported multiple NICs — gates the dropdown
// chevron next to the IP field.
const multipleIps = computed(() => (daemonStore.info?.all_ips.length ?? 0) > 1)

const captureWarning = computed(() => daemonStore.captureStatus === 'Disabled')
const emulationWarning = computed(() => daemonStore.emulationStatus === 'Disabled')

// Dropdown for picking which NIC to surface when several are live.
// `ipFieldRef` covers the IP display + chevron + popover so a click
// outside the wrapper closes the popover without us having to wire
// per-element handlers.
const ipPickerOpen = ref(false)
const ipFieldRef = ref<HTMLElement | null>(null)

function togglePicker() {
  ipPickerOpen.value = !ipPickerOpen.value
}

function pickIp(ip: string) {
  selectedIp.value = ip
  ipPickerOpen.value = false
}

function onDocumentClick(ev: MouseEvent) {
  if (!ipPickerOpen.value) return
  const el = ipFieldRef.value
  if (el && !el.contains(ev.target as Node)) {
    ipPickerOpen.value = false
  }
}

function onEscape(ev: KeyboardEvent) {
  if (ev.key === 'Escape' && ipPickerOpen.value) {
    ipPickerOpen.value = false
  }
}

onMounted(() => {
  // capture phase so we run before any inner click handlers; without
  // capture the chevron click would race with the document handler.
  document.addEventListener('click', onDocumentClick, true)
  document.addEventListener('keydown', onEscape)
})

onBeforeUnmount(() => {
  document.removeEventListener('click', onDocumentClick, true)
  document.removeEventListener('keydown', onEscape)
})

// ---- handlers ---------------------------------------------------

function commitPort() {
  const err = validatePort(portDraft.value)
  if (err) {
    pushToast('error', `invalid port: ${portDraft.value}`)
    return
  }
  changePort(portDraft.value)
}

async function copy(text: string, label = 'copied') {
  const ok = await copyToClipboard(text)
  pushToast(ok ? 'success' : 'error', ok ? label : 'copy failed')
}
</script>

<template>
  <div class="card">
    <!-- hostname & port — the address peers dial into. Shown
         prominently with a copy button next to the editable
         port. If multiple NICs are live a chevron opens a
         dropdown to pick the one peers should use. -->
    <div>
      <div>
        <div class="muted card-title">Hostname &amp; Port</div>
        <div
          style="
            display: flex;
            gap: 8px;
            justify-content: flex-start;
            align-items: center;
            margin-right: 12px;
          "
        >
          <div ref="ipFieldRef" class="ip-field">
            <div class="mono em1 ip-display" :title="displayIp || 'no address detected'">
              {{ displayIp || '—' }}
            </div>

            <button
              v-if="multipleIps"
              class="ip-chevron"
              type="button"
              :aria-expanded="ipPickerOpen"
              :aria-label="ipPickerOpen ? 'hide address list' : 'choose address'"
              :title="ipPickerOpen ? 'hide address list' : 'choose address'"
              @click="togglePicker"
            >
              <IconChevron />
            </button>

            <div v-if="ipPickerOpen && multipleIps" class="ip-popover">
              <button
                v-for="ip in daemonStore.info?.all_ips ?? []"
                :key="ip"
                type="button"
                class="ip-option"
                :class="{ selected: ip === selectedIp }"
                @click="pickIp(ip)"
              >
                <span class="ip-text">{{ ip }}</span>
                <span v-if="ip === daemonStore.info?.primary_ip" class="ip-tag">primary</span>
              </button>
            </div>
          </div>
          <span class="port-sep mono">:</span>
          <input
            type="number"
            min="1"
            max="65535"
            v-model="portDraft"
            @change="commitPort"
            placeholder="4242"
            class="mono"
            style="width: 66px"
          />
          <button
            class="icon ghost"
            title="copy host:port"
            @click="copy(`${displayIp}:${daemonStore.port}`, 'host:port copied')"
          >
            <IconCopy />
          </button>
        </div>
        <span
          v-if="daemonStore.portError"
          style="color: var(--error); margin-top: 6px; display: block"
          >{{ daemonStore.portError }}</span
        >
      </div>
    </div>

    <div style="margin-top: 14px">
      <div>
        <div class="muted card-title">Certificate fingerprint (sha256)</div>
        <div style="display: flex; align-items: center; margin: 0 12px; gap: 8px">
          <div class="mono em1" style="word-break: break-all; color: var(--fg-default);">
            {{ daemonStore.fingerprint || '—' }}
          </div>
          <button
            class="icon ghost"
            :disabled="!daemonStore.fingerprint"
            @click="copy(daemonStore.fingerprint, 'fingerprint copied')"
            title="copy"
          >
            <IconCopy />
          </button>
        </div>
      </div>
    </div>

    <div v-if="captureWarning || emulationWarning" style="margin-top: 12px">
      <div class="warning-banner" style="margin: 0; flex: 1">
        <IconWarning />
        <div class="body">
          <strong>input is disabled</strong>
          grant Accessibility / capture permission to enable mouse sharing.
        </div>
        <button v-if="captureWarning" class="primary" @click="enableCapture">
          Reenable capture
        </button>
        <button v-else-if="emulationWarning" class="primary" @click="enableEmulation">
          Reenable emulation
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
/* IP picker: input-like field with an inline chevron that opens
 * a dropdown of every NIC the daemon reported. */
.ip-field {
  position: relative;
  flex: 1;
  min-width: 0;
  display: flex;
  align-items: center;
  padding: 0 12px;
  height: 36px;
  transition:
    border-color 0.15s,
    background 0.15s;
}
.ip-field:focus-within {
  border-color: var(--accent);
  background: #2c2c2c;
}
.ip-display {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  background: transparent !important;
  border: none !important;
  padding: 0 !important;
  color: var(--fg-default);
  line-height: 1.2;
}
.ip-chevron {
  appearance: none;
  background: transparent;
  border: none;
  color: var(--fg-muted);
  cursor: pointer;
  padding: 6px;
  border-radius: 4px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  flex-shrink: 0;
  transition:
    transform 0.18s,
    background 0.15s,
    color 0.15s;
}
.ip-chevron:hover {
  background: var(--bg-elev);
  color: var(--fg-default);
}
.ip-chevron[aria-expanded='true'] {
  transform: rotate(180deg);
  color: var(--accent);
}
.ip-popover {
  position: absolute;
  top: calc(100% + 6px);
  left: 0;
  right: 0;
  background: #3d3d3d;
  border: 1px solid var(--border);
  border-radius: var(--radius-sm);
  box-shadow: var(--shadow);
  padding: 4px;
  z-index: 100;
  max-height: 240px;
  overflow-y: auto;
}
.ip-option {
  display: flex;
  align-items: center;
  gap: 8px;
  width: 100%;
  text-align: left;
  background: transparent;
  border: none;
  color: var(--fg-default);
  padding: 7px 10px;
  border-radius: 4px;
  font-family: ui-monospace, 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace;
  font-size: 1em;
  cursor: pointer;
}
.ip-option:hover {
  background: var(--bg-elev);
}
.ip-option.selected {
  background: var(--accent-soft);
  color: var(--accent);
}
.ip-text {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.ip-tag {
  font-size: 10px;
  letter-spacing: 0.4px;
  text-transform: uppercase;
  color: var(--accent);
  background: var(--accent-soft);
  border-radius: 999px;
  padding: 1px 6px;
  flex-shrink: 0;
}
</style>
