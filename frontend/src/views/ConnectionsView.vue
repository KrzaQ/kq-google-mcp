<script setup lang="ts">
// One Google account per connection. Connecting is a browser round trip
// through Google's consent screen, so the form does not create anything: it
// asks the server for a URL and hands the browser over. The callback lands
// back here with ?connected=<id> or ?error=<code>, which is what the banner
// reads before the query is cleared away.
import { computed, onMounted, ref } from 'vue'
import { useRoute, useRouter } from 'vue-router'
import { api } from '@/api/client'
import type { ConnectionDto } from '@/api/types'
import ConfirmDialog from '@/components/ConfirmDialog.vue'
import StatusBadge from '@/components/StatusBadge.vue'
import { connectErrorMessage } from '@/lib/connectErrors'
import { SERVICE_ORDER, driveLocked, setService, withImplied } from '@/lib/services'
import { formatAgo } from '@/lib/time'
import { useSession } from '@/stores/session'

const route = useRoute()
const router = useRouter()
const session = useSession()

const connections = ref<ConnectionDto[]>([])
const error = ref<string | null>(null)
const banner = ref<{ kind: 'ok' | 'error'; text: string } | null>(null)
const busy = ref(false)

const label = ref('')
const services = ref<string[]>(['gmail'])
const driveIsLocked = computed(() => driveLocked(services.value))
const googleConfigured = computed(() => session.me?.google_configured !== false)

const renaming = ref<number | null>(null)
const newLabel = ref('')
const removing = ref<ConnectionDto | null>(null)

async function load() {
  error.value = null
  try {
    connections.value = await api.connections.list()
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}

/**
 * The banner is read from the query and the query is then dropped, so a
 * refresh or a bookmark does not repeat a message about something that
 * happened once.
 */
function readCallback() {
  const connected = route.query.connected
  const failed = route.query.error
  if (connected != null) {
    const id = Number(connected)
    banner.value = { kind: 'ok', text: `Connected. The account is listed below.` }
    if (Number.isFinite(id)) {
      const found = connections.value.find((c) => c.id === id)
      if (found) banner.value.text = `Connected ${found.label} (${found.google_email}).`
    }
  } else if (failed != null) {
    banner.value = { kind: 'error', text: connectErrorMessage(String(failed)) }
  }
  if (connected != null || failed != null) router.replace({ path: route.path, query: {} })
}

function toggleService(name: string, on: boolean) {
  services.value = setService(services.value, name, on)
}

async function connect() {
  error.value = null
  busy.value = true
  try {
    const { url } = await api.connections.start({
      label: label.value.trim(),
      services: withImplied(services.value),
    })
    window.location.assign(url)
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
    busy.value = false
  }
}

async function reconnect(c: ConnectionDto) {
  error.value = null
  try {
    const { url } = await api.connections.reconnect(c.id)
    window.location.assign(url)
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}

async function setGateway(c: ConnectionDto, delegate_ok: boolean) {
  try {
    await api.connections.update(c.id, { delegate_ok })
    await load()
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
    await load()
  }
}

function startRename(c: ConnectionDto) {
  renaming.value = c.id
  newLabel.value = c.label
}

async function saveRename(c: ConnectionDto) {
  const renamed = newLabel.value.trim()
  renaming.value = null
  if (!renamed || renamed === c.label) return
  try {
    await api.connections.update(c.id, { label: renamed })
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
  await load()
}

async function remove() {
  const c = removing.value
  removing.value = null
  if (!c) return
  try {
    await api.connections.remove(c.id)
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
  await load()
}

onMounted(async () => {
  await load()
  readCallback()
})
</script>

<template>
  <main class="mx-auto max-w-5xl space-y-6 px-4 py-6" data-testid="connections-view">
    <div>
      <h1 class="text-xl font-semibold">Connections</h1>
      <p class="mt-1 text-sm text-muted">
        One Google account per connection, each with a label the tools name as
        <code>account</code>.
      </p>
    </div>

    <p
      v-if="banner"
      :class="banner.kind === 'ok' ? 'note-ok' : 'note-danger'"
      data-testid="callback-banner"
    >
      {{ banner.text }}
    </p>
    <p v-if="error" class="note-danger" data-testid="connections-error">{{ error }}</p>
    <p v-if="!googleConfigured" class="note-warn" data-testid="google-unconfigured">
      This server has no Google client configured, so nothing can be connected yet.
    </p>

    <section class="space-y-3">
      <article
        v-for="c in connections"
        :key="c.id"
        class="card p-4"
        data-testid="connection-row"
        :data-status="c.status"
      >
        <div class="flex flex-wrap items-start justify-between gap-3">
          <div class="min-w-0">
            <div v-if="renaming === c.id" class="flex items-center gap-2">
              <input
                v-model="newLabel"
                class="w-40"
                data-testid="rename-input"
                @keyup.enter="saveRename(c)"
                @keyup.esc="renaming = null"
              />
              <button
                class="btn-secondary text-xs"
                data-testid="rename-save"
                @click="saveRename(c)"
              >
                Save
              </button>
            </div>
            <h2 v-else class="font-medium">
              {{ c.label }}
              <button class="link ml-2 text-xs" data-testid="rename" @click="startRename(c)">
                rename
              </button>
            </h2>
            <p class="text-xs text-muted">{{ c.google_email }}</p>
          </div>
          <StatusBadge :status="c.status" :detail="c.status_detail" />
        </div>

        <p class="mt-3 flex flex-wrap gap-1">
          <span v-for="s in c.services" :key="s" class="chip">{{ s }}</span>
        </p>

        <p v-if="c.partial" class="mt-3 note-warn" data-testid="partial-warning">
          Partial grant: Google gave less than these services need, so some tools will fail.
          Reconnect and leave every box on the consent screen ticked.
        </p>
        <p v-if="c.status_detail" class="mt-2 text-xs text-warn">{{ c.status_detail }}</p>

        <div class="mt-3 flex flex-wrap items-center gap-4 text-sm">
          <label class="flex items-center gap-2" title="Reachable through a delegate token">
            <input
              type="checkbox"
              :checked="c.delegate_ok"
              data-testid="gateway-toggle"
              @change="setGateway(c, ($event.target as HTMLInputElement).checked)"
            />
            <span class="text-muted">Gateway may reach it</span>
          </label>
          <span class="text-xs text-muted">
            Last use {{ formatAgo(c.last_used_at, new Date(), session.zone) }}
          </span>
          <span class="flex-1"></span>
          <button class="link text-xs" data-testid="reconnect" @click="reconnect(c)">
            Reconnect
          </button>
          <button class="link text-xs text-danger" data-testid="remove" @click="removing = c">
            Remove
          </button>
        </div>
      </article>
      <p v-if="connections.length === 0" class="text-sm text-muted">No accounts connected yet.</p>
    </section>

    <form class="card space-y-4 p-4" data-testid="connect-form" @submit.prevent="connect">
      <h2 class="font-medium">Connect account</h2>
      <label class="block text-sm">
        Label<br />
        <input
          v-model="label"
          required
          class="w-48"
          placeholder="work"
          data-testid="connect-label"
        />
      </label>
      <div class="space-y-1">
        <p class="text-sm">Services</p>
        <label
          v-for="s in SERVICE_ORDER"
          :key="s"
          class="mr-4 inline-flex items-center gap-2 text-sm"
        >
          <input
            type="checkbox"
            :checked="services.includes(s)"
            :disabled="s === 'drive' && driveIsLocked"
            :data-testid="`service-${s}`"
            @change="toggleService(s, ($event.target as HTMLInputElement).checked)"
          />
          <span :class="s === 'drive' && driveIsLocked ? 'text-muted' : ''">{{ s }}</span>
        </label>
        <p v-if="driveIsLocked" class="text-xs text-muted" data-testid="drive-note">
          Docs and Sheets are searched and exported through Drive, so Drive comes with them.
        </p>
      </div>
      <button
        class="btn"
        type="submit"
        :disabled="busy || !label.trim() || services.length === 0 || !googleConfigured"
        data-testid="connect-submit"
      >
        Connect at Google
      </button>
      <p class="text-xs text-muted">
        Google shows an "unverified app" screen once per account; continue under Advanced.
      </p>
    </form>

    <ConfirmDialog
      :open="removing !== null"
      title="Remove connection"
      :message="
        removing
          ? `Remove ${removing.label} (${removing.google_email})? The grant is revoked at Google and every tool that names this account stops working.`
          : ''
      "
      confirm-label="Remove"
      danger
      @confirm="remove"
      @cancel="removing = null"
    />
  </main>
</template>
