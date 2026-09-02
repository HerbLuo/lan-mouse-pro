<script setup lang="ts">
// Three-state connection pill in the header:
//
//   Fail      — WebSocket bridge to the daemon is dead (initial
//               handshake never completed, or it disconnected and
//               the reconnect timer hasn't restored it yet). Red dot.
//   Prepared  — WebSocket is up, but no QUIC peer has completed a
//               handshake in this session. Daemon is reachable and
//               answering requests; LAN path to peers is unproven.
//               Amber dot, slow pulse.
//   Paired    — At least one peer has completed a QUIC handshake
//               since the WS last opened. Latched: the badge stays
//               green even if every peer later drops. Green dot.
//
// The label is the literal state name ("Fail" / "Prepared" /
// "Paired") — short, unambiguous, easy to grep in screenshots.
import { connStateRef } from '@/store'
</script>

<template>
  <header class="appbar">
    <div>
      <h1>Lan Mouse Pro</h1>
      <div class="status-bar">
        <div class="subtitle">Share your mouse & keyboard & clipboard across devices</div>
        <div class="badge" :class="connStateRef.toLowerCase()">
          <span class="dot"></span>
          {{ connStateRef }}
        </div>
      </div>
    </div>
  </header>
</template>

<style scoped>
h1 {
  margin: 0;
  padding: 20px;
  border-bottom: var(--border);
}
.status-bar {
  display: flex;
  justify-content: space-between;
  align-items: center;
  padding: 16px 20px;
  border-bottom: var(--border);
  gap: 12px;
}

.badge {
  display: inline-flex;
  align-items: center;
  gap: 6px;
  padding: 4px 10px;
  border-radius: 999px;
  font-size: 12px;
  background: var(--bg-elev);
  color: var(--fg-muted);
  border: 1px solid var(--border);
  font-family: ui-monospace, 'SF Mono', Menlo, Consolas, 'Liberation Mono', monospace;
  letter-spacing: 0.4px;
  flex-shrink: 0;
}

.badge .dot {
  width: 8px;
  height: 8px;
  border-radius: 50%;
  background: var(--fg-muted);
}

/* Fail — WebSocket bridge dead. */
.badge.fail {
  color: var(--error);
  border-color: rgba(255, 107, 107, 0.4);
}
.badge.fail .dot {
  background: var(--error);
  box-shadow: 0 0 6px rgba(255, 107, 107, 0.6);
}

/* Prepared — daemon reachable, no peer has connected yet. */
.badge.prepared {
  color: var(--warning);
  border-color: rgba(255, 179, 71, 0.4);
}
.badge.prepared .dot {
  background: var(--warning);
  box-shadow: 0 0 6px rgba(255, 179, 71, 0.6);
  animation: pulse 1.8s ease-in-out infinite;
}

/* Paired — at least one peer handshaked; LAN path proven. */
.badge.paired {
  color: var(--success);
  border-color: rgba(92, 217, 122, 0.4);
}
.badge.paired .dot {
  background: var(--success);
  box-shadow: 0 0 6px var(--success);
}

@keyframes pulse {
  0%,
  100% {
    opacity: 1;
  }
  50% {
    opacity: 0.4;
  }
}
</style>
