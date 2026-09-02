import { fileURLToPath, URL } from 'node:url'

import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import vueDevTools from 'vite-plugin-vue-devtools'

// https://vite.dev/config/
//
// In dev (`npm run dev`) Vite runs on its own port (5173 by default)
// and only serves the .vue / .ts sources. The Rust daemon, however,
// owns the real `/api/*` and `/ws` endpoints on its own port
// (3939 by default, overridable via LAN_MOUSE_WEB_PORT or
// `[frontend].port` in config.toml). Without proxying, fetch('/api/info')
// 404s and the WebSocket refuses to upgrade — the UI appears frozen.
//
// The proxy below forwards both routes transparently. The
// LAN_MOUSE_WEB_PORT env var lets CI / other developers point at a
// non-default daemon without editing this file.
const devBackend =
  process.env.LAN_MOUSE_WEB_PORT != null
    ? `http://127.0.0.1:${process.env.LAN_MOUSE_WEB_PORT}`
    : 'http://127.0.0.1:3939'

export default defineConfig({
  plugins: [vue(), vueDevTools()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  server: {
    // Listen on all interfaces so a peer machine on the LAN can open
    // the dev server in their browser too — handy when debugging
    // multi-host setups.
    host: true,
    port: 5173,
    proxy: {
      // REST: /api/info, /api/whatever
      '/api': {
        target: devBackend,
        changeOrigin: false,
      },
      // WebSocket: /ws. Vite forwards the upgrade automatically
      // when `ws: true` is set on the proxy entry.
      '/ws': {
        target: devBackend,
        ws: true,
        changeOrigin: false,
      },
    },
  },
})