<script setup lang="ts">
import { daemonStore, dismissToast } from '@/store'
import IconClose from '@/components/icons/IconClose.vue'
</script>

<template>
  <div class="toast-stack">
    <div v-for="t in daemonStore.toasts" :key="t.id" class="toast" :class="t.kind">
      <span style="flex: 1">{{ t.message }}</span>
      <button class="close" @click="dismissToast(t.id)">
        <IconClose />
      </button>
    </div>
  </div>
</template>
