<script setup lang="ts">
// Rotated 180° when used as a "collapse" affordance. Caller passes a
// transform via the wrapper span if they need animation; this keeps
// the SVG itself static and predictable.
withDefaults(defineProps<{ size?: number }>(), { size: 12 })
</script>

<template>
  <svg
    :width="size"
    :height="size"
    viewBox="0 0 16 16"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
    stroke-linecap="round"
    stroke-linejoin="round"
  >
    <path d="m4 6 4 4 4-4" />
  </svg>
</template>
