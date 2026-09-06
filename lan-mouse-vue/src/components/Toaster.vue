<script setup lang="ts">
import { daemonStore, dismissToast } from '@/store'
import IconClose from '@/components/icons/IconClose.vue'
</script>

<template>
  <div class="toast-stack">
    <div v-for="t in daemonStore.toasts" :key="t.id" class="toast" :class="t.kind">
      <span class="message">{{ t.message }}</span>
      <button class="close" @click="dismissToast(t.id)">
        <IconClose />
      </button>
    </div>
  </div>
</template>

<style scoped>
.toast-stack {
  position: fixed;
  top: 16px;
  right: 16px;
  display: flex;
  flex-direction: column;
  gap: 8px;
  z-index: 998;
  pointer-events: none;
}

.toast {
  background: #2e2e2e;
  border: var(--border);
  border-left-width: 3px;
  border-radius: 2px;
  padding: 10px 12px;
  display: flex;
  align-items: center;
  gap: 8px;
  min-width: 240px;
  max-width: 360px;
  box-shadow: var(--shadow);
  font-size: 14px;
  color: var(--fg-default);
  pointer-events: auto;
  animation: toast-in 0.18s ease-out;
}

.toast .message {
  flex: 1;
  line-height: 1.35;
}

.toast .close {
  appearance: none;
  border: 0;
  background: transparent;
  padding: 0;
  width: 18px;
  height: 18px;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  color: var(--fg-muted);
  cursor: pointer;
  opacity: 0.7;
  transition:
    opacity 0.15s,
    color 0.15s;
}

.toast .close:hover {
  opacity: 1;
  color: var(--fg-default);
}

.toast.error {
  border-left-color: var(--error);
  color: var(--error);
}
.toast.error .close {
  color: var(--error);
}

.toast.success {
  border-left-color: #66bb6a;
  color: #a5d6a7;
}
.toast.success .close {
  color: #a5d6a7;
}

.toast.warning {
  border-left-color: #ffa726;
  color: #ffcc80;
}
.toast.warning .close {
  color: #ffcc80;
}

.toast.info {
  border-left-color: #82b1ff;
  color: #bbdefb;
}
.toast.info .close {
  color: #bbdefb;
}

@keyframes toast-in {
  from {
    opacity: 0;
    transform: translateY(8px);
  }
  to {
    opacity: 1;
    transform: translateY(0);
  }
}
</style>
