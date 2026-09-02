<script setup lang="ts">
withDefaults(defineProps<{ size?: number; color?: string }>(), { size: 16, color: 'currentColor' })
</script>

<template>
  <svg
    :width="size"
    :height="size"
    viewBox="0 0 24 24"
    fill="none"
    :stroke="color"
    stroke-width="2"
    stroke-linecap="round"
    stroke-linejoin="round"
  >
    <path d="M12 2a4 4 0 0 0-4 4v3a4 4 0 1 0 8 0V6a4 4 0 0 0-4-4Z" />
    <path d="M6 9v3a6 6 0 0 0 12 0V9" />
    <path d="M12 18v3" />
  </svg>
</template>
