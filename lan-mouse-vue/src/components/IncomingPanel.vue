<script setup lang="ts">
import { daemonStore, removeAuthorizedKey } from '@/store'
import IconMic from '@/components/icons/IconMic.vue'
import IconTrash from '@/components/icons/IconTrash.vue'

const emit = defineEmits<{ 'open-authorize': [] }>()
</script>

<template>
  <div class="section-header-row">
    <button @click="emit('open-authorize')">
      <span style="display: inline-flex">
        <IconMic :size="16" />
      </span>
      Authorize
    </button>
  </div>

  <div
    v-if="Object.keys(daemonStore.authorized).length === 0 && !daemonStore.pendingConnectionAttempt"
    class="card"
  >
    <div class="empty">
      <div>No devices registered.</div>
      <div class="hint">authorize a device via the Authorize button above.</div>
    </div>
  </div>

  <div v-for="(description, fp) in daemonStore.authorized" :key="fp" class="card card-row">
    <div style="flex: 1; min-width: 0">
      <div style="font-weight: 500">{{ description }}</div>
      <div
        class="mono"
        style="font-size: 11px; color: var(--fg-muted); word-break: break-all; margin-top: 2px"
      >
        {{ fp }}
      </div>
    </div>
    <button class="icon danger" @click="removeAuthorizedKey(fp)" title="remove">
      <IconTrash />
    </button>
  </div>
</template>
