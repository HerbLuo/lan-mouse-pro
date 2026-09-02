<script setup lang="ts">
import { onMounted, ref } from 'vue'
import { daemonStore, loadInfo } from '@/store'
import AppHeader from '@/components/AppHeader.vue'
import AuthorizeModal from '@/components/AuthorizeModal.vue'
import ConnectionsPanel from '@/components/ConnectionsPanel.vue'
import GeneralPanel from '@/components/GeneralPanel.vue'
import IncomingPanel from '@/components/IncomingPanel.vue'
import Toaster from '@/components/Toaster.vue'

// Holds the imperative `open()` exposed by AuthorizeModal so
// IncomingPanel's "Authorize" button can pop it from outside.
const authorizeModalRef = ref<InstanceType<typeof AuthorizeModal> | null>(null)

onMounted(() => {
  // Main.ts already opened the socket; main.ts also kicked off
  // loadInfo(). This hook is a defensive re-fetch in case the user
  // hard-reloads the tab before the first fetch resolved — keeps
  // the IP field populated if the WebSocket is still handshaking.
  if (!daemonStore.info) void loadInfo()
})
</script>

<template>
  <div class="container">
    <AppHeader />

    <main class="page">
      <section class="section">
        <h2 class="section-title">General</h2>
        <GeneralPanel />
      </section>

      <section class="section">
        <h2 class="section-title">Connections</h2>
        <ConnectionsPanel />
      </section>

      <section class="section">
        <h2 class="section-title">Incoming Connections</h2>
        <IncomingPanel @open-authorize="authorizeModalRef?.open()" />
      </section>
    </main>

    <AuthorizeModal ref="authorizeModalRef" />
    <Toaster />
  </div>
</template>

<style scoped>
.container {
  width: 666px;
  height: 100vh;
  margin: 0 auto;
  overflow: scroll;
  border-left: var(--border);
  border-right: var(--border);
}
.section {
  padding: 16px 20px;
  border-bottom: var(--border);
}
h2 {
  margin: 0;
  padding-bottom: 16px;
  border-bottom: var(--border);
}
</style>
