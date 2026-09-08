<script setup lang="ts">
// The page a person opens when something failed in chat: which accounts are
// healthy, and what the models have been doing with them.
import { onMounted, ref } from 'vue'
import { api } from '@/api/client'
import type { AuditDto, ConnectionDto } from '@/api/types'
import AuditTable from '@/components/AuditTable.vue'
import StatusBadge from '@/components/StatusBadge.vue'
import { formatAgo } from '@/lib/time'

const connections = ref<ConnectionDto[]>([])
const entries = ref<AuditDto[]>([])
const error = ref<string | null>(null)
const reconnecting = ref<number | null>(null)

async function load() {
  error.value = null
  try {
    const [c, page] = await Promise.all([api.connections.list(), api.audit({ limit: 50 })])
    connections.value = c
    entries.value = page.entries
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}

/** Re-consent is a browser round trip through Google, as connecting is. */
async function reconnect(c: ConnectionDto) {
  reconnecting.value = c.id
  try {
    const { url } = await api.connections.reconnect(c.id)
    window.location.assign(url)
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
    reconnecting.value = null
  }
}

onMounted(load)
</script>

<template>
  <main class="mx-auto max-w-6xl space-y-6 px-4 py-6" data-testid="home-view">
    <div>
      <h1 class="text-xl font-semibold">Home</h1>
      <p class="mt-1 text-sm text-muted">
        The connections and their health, and the last calls made through them.
      </p>
    </div>

    <p v-if="error" class="note-danger" data-testid="home-error">{{ error }}</p>

    <section class="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
      <article
        v-for="c in connections"
        :key="c.id"
        class="card p-4"
        data-testid="connection-card"
        :data-status="c.status"
      >
        <div class="flex items-start justify-between gap-2">
          <div class="min-w-0">
            <h2 class="truncate font-medium">{{ c.label }}</h2>
            <p class="truncate text-xs text-muted">{{ c.google_email }}</p>
          </div>
          <StatusBadge :status="c.status" :detail="c.status_detail" />
        </div>
        <p class="mt-3 flex flex-wrap gap-1">
          <span v-for="s in c.services" :key="s" class="chip">{{ s }}</span>
        </p>
        <p v-if="c.partial" class="mt-3 note-warn" data-testid="partial-warning">
          Google granted less than these services need. Reconnect and leave every box ticked.
        </p>
        <p v-if="c.status_detail" class="mt-3 text-xs text-warn">{{ c.status_detail }}</p>
        <div class="mt-3 flex items-center justify-between gap-2">
          <span class="text-xs text-muted">Last use {{ formatAgo(c.last_used_at) }}</span>
          <button
            v-if="c.status === 'needs_reauth'"
            class="btn-secondary text-xs"
            :disabled="reconnecting === c.id"
            data-testid="reconnect"
            @click="reconnect(c)"
          >
            Reconnect
          </button>
        </div>
      </article>
      <p v-if="connections.length === 0" class="text-sm text-muted" data-testid="no-connections">
        No accounts connected yet.
        <RouterLink to="/connections" class="link">Connect one</RouterLink>.
      </p>
    </section>

    <section class="space-y-2">
      <h2 class="text-sm font-medium text-muted">Recent activity</h2>
      <AuditTable :entries="entries" :connections="connections" />
      <p class="text-xs text-muted">
        <RouterLink to="/activity" class="link">The whole log</RouterLink>, with filters.
      </p>
    </section>
  </main>
</template>
