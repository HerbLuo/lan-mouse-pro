<script setup lang="ts">
import { ref, watch } from 'vue'
import { acceptConnection, authorizeKey, daemonStore, pushToast, rejectConnection } from '@/store'

// ---- local UI state ----------------------------------------------

const show = ref(false)
const pendingFp = ref('')
const pendingDesc = ref('')
const newAuthFp = ref('')
const newAuthDesc = ref('')

// Auto-open when the daemon reports an incoming connection attempt.
// We seed `pendingFp` from the daemon-supplied fingerprint so the
// user only needs to give it a label.
watch(
  () => daemonStore.pendingConnectionAttempt,
  (fp) => {
    if (fp) {
      pendingFp.value = fp
      pendingDesc.value = ''
      show.value = true
    }
  },
)

// ---- imperative API (used by AppHeader / IncomingPanel) ---------

function open() {
  show.value = true
}

function close() {
  show.value = false
}

defineExpose({ open, close })

// ---- handlers ---------------------------------------------------

function submitNewAuth() {
  if (!newAuthFp.value.trim()) {
    pushToast('error', 'fingerprint is required')
    return
  }
  authorizeKey(newAuthDesc.value.trim() || newAuthFp.value.slice(0, 8), newAuthFp.value.trim())
  newAuthFp.value = ''
  newAuthDesc.value = ''
  close()
}

function confirmPending() {
  acceptConnection(pendingFp.value, pendingDesc.value || pendingFp.value.slice(0, 8))
  close()
}

function cancelPending() {
  rejectConnection()
  pendingFp.value = ''
  pendingDesc.value = ''
  close()
}
</script>

<template>
  <div v-if="show" class="modal-backdrop" @click.self="close">
    <div class="modal">
      <h3 style="margin: 0 0 12px; font-size: 16px">
        {{ pendingFp ? 'New device is requesting access' : 'Authorize a device' }}
      </h3>
      <p v-if="pendingFp" style="font-size: 13px; color: var(--fg-muted); margin: 0 0 12px">
        A device with the following fingerprint is trying to connect. Give it a friendly label and
        approve to let it control your mouse & keyboard.
      </p>
      <p v-else style="font-size: 13px; color: var(--fg-muted); margin: 0 0 12px">
        Paste the SHA-256 fingerprint of the device you want to trust.
      </p>

      <label style="display: block; margin-bottom: 12px">
        <span style="font-size: 12px; color: var(--fg-muted)">description</span>
        <input
          v-if="pendingFp"
          v-model="pendingDesc"
          type="text"
          placeholder="my desktop, dad's laptop, …"
          style="margin-top: 4px"
        />
        <input
          v-else
          v-model="newAuthDesc"
          type="text"
          placeholder="my desktop, dad's laptop, …"
          style="margin-top: 4px"
        />
      </label>

      <label style="display: block; margin-bottom: 16px">
        <span style="font-size: 12px; color: var(--fg-muted)">sha256 fingerprint</span>
        <input
          v-if="pendingFp"
          v-model="pendingFp"
          class="mono"
          type="text"
          placeholder="a4:9b:47:…"
          style="margin-top: 4px"
        />
        <input
          v-else
          v-model="newAuthFp"
          class="mono"
          type="text"
          placeholder="a4:9b:47:…"
          style="margin-top: 4px"
        />
      </label>

      <div style="display: flex; gap: 8px; justify-content: flex-end">
        <button @click="cancelPending">Cancel</button>
        <button class="primary" @click="pendingFp ? confirmPending() : submitNewAuth()">
          Authorize
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.modal-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.55);
  backdrop-filter: blur(2px);
  display: flex;
  align-items: center;
  justify-content: center;
  z-index: 999;
  animation: fadein 0.18s ease-out;
}
@keyframes fadein {
  from {
    opacity: 0;
  }
  to {
    opacity: 1;
  }
}
.modal {
  background: #2e2e2e;
  border: var(--border);
  border-radius: var(--radius);
  padding: 20px 24px;
  width: min(420px, 92vw);
  box-shadow: var(--shadow);
}
</style>
