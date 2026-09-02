import './styles/main.css'

import { createApp } from 'vue'
import App from './App.vue'
import { initSocket, loadInfo } from './store'

createApp(App).mount('#app')

// Open the WebSocket to the daemon before mount so the first render
// already includes whatever state comes back on the wire.
initSocket()
// Pull the boot info (hostname, LAN IPs, web port) so the General
// panel can show "this is who I am on the LAN" without waiting for a
// FrontendEvent round-trip.
void loadInfo()
