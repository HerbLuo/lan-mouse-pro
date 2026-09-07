import { fileURLToPath, URL } from 'node:url'

import { defineConfig } from 'vitest/config'
import vue from '@vitejs/plugin-vue'

// STEP-M3-3.2 — vitest setup for the Vue UI layer.
//
// Mirrors `vite.config.ts` (the dev / build config) for the bits
// the test runtime actually needs:
//   - `vue()` so `.vue` SFCs compile
//   - the `@/...` path alias so tests can `import` from `@/store`
//     the same way the source files do
//
// `environment: 'happy-dom'` provides `window`, `document`,
// `WebSocket`, `console`, etc. — the minimum surface the store
// and components touch at module-load time. We don't render into
// a real browser; snapshot tests use vue-test-utils' `mount()`.
//
// Excluded from `vite.config.ts` deliberately — keeping the dev
// server lean and not pulling happy-dom into the production
// bundle.
export default defineConfig({
  plugins: [vue()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  test: {
    environment: 'happy-dom',
    include: ['src/**/*.test.ts'],
  },
})
