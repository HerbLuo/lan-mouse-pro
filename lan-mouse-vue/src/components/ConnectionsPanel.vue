<script setup lang="ts">
import { addClient, daemonStore } from '@/store'
import IconPlus from '@/components/icons/IconPlus.vue'
import ConnectionRow from '@/components/ConnectionRow.vue'
</script>

<template>
  <div class="card">
    <div class="section-header-row">
      <button class="primary" @click="addClient">
        <IconPlus />
        Add
      </button>
    </div>

    <div v-if="daemonStore.clients.size === 0" style="padding-bottom: 0" class="card">
      <div class="empty">
        <div>No connections yet.</div>
        <div class="hint">click + Add to pair with another device</div>
      </div>
    </div>

    <ConnectionRow
      v-for="conn in Array.from(daemonStore.clients.values())"
      :key="conn.handle"
      :connection="conn"
    />
  </div>
</template>

<style scoped></style>
