<script setup lang="ts">
// The page a person opens when something failed in chat: which accounts are
// healthy, and what the models have been doing with them.
import { onMounted, ref, watch } from 'vue'
import { api } from '@/api/client'
import type { AuditDto, ConnectionDto } from '@/api/types'
import AuditTable from '@/components/AuditTable.vue'
import StatusBadge from '@/components/StatusBadge.vue'
import { COMMON_ZONES, formatAgo } from '@/lib/time'
import { useSession } from '@/stores/session'

const session = useSession()
const connections = ref<ConnectionDto[]>([])
const entries = ref<AuditDto[]>([])
const error = ref<string | null>(null)
const reconnecting = ref<number | null>(null)

// The zone control. The draft is what is typed; the store holds what the
// server has, and the two only meet when Save succeeds.
const zoneDraft = ref(session.zone)
const zoneError = ref<string | null>(null)
const savingZone = ref(false)
watch(
  () => session.zone,
  (z) => (zoneDraft.value = z),
)

async function saveZone() {
  zoneError.value = null
  savingZone.value = true
  try {
    await session.setZone(zoneDraft.value.trim())
  } catch (e) {
    zoneError.value = e instanceof Error ? e.message : String(e)
  } finally {
    savingZone.value = false
  }
}

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

    <section class="card p-4" data-testid="timezone-card">
      <h2 class="font-medium">Time zone</h2>
      <p class="mt-1 text-sm text-muted">
        Every time on this site is shown on this clock, and so is every time the models see. A time
        given to a tool without an offset — "3pm tomorrow" — is read on it too.
      </p>
      <form class="mt-3 flex flex-wrap items-center gap-2" @submit.prevent="saveZone">
        <input
          v-model="zoneDraft"
          list="common-zones"
          class="w-64"
          aria-label="IANA time zone"
          placeholder="Europe/Warsaw"
          data-testid="timezone-input"
        />
        <datalist id="common-zones">
          <option v-for="z in COMMON_ZONES" :key="z" :value="z" />
        </datalist>
        <button
          type="submit"
          class="btn"
          :disabled="savingZone || !zoneDraft.trim() || zoneDraft.trim() === session.zone"
          data-testid="timezone-save"
        >
          Save
        </button>
        <span class="text-xs text-muted" data-testid="timezone-current">
          Showing times in {{ session.zone }}
        </span>
      </form>
      <p v-if="zoneError" class="mt-2 note-danger" data-testid="timezone-error">{{ zoneError }}</p>
    </section>

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
          <span class="text-xs text-muted">
            Last use {{ formatAgo(c.last_used_at, new Date(), session.zone) }}
          </span>
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
