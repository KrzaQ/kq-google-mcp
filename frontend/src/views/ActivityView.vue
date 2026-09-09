<script setup lang="ts">
// The whole log. "Why did the model draft that" is answered here, so every
// column the API records is filterable and the page walks backwards through
// the (at, id) cursor rather than by offset — two rows can share an instant.
import { computed, onMounted, reactive, ref } from 'vue'
import { api, type AuditQuery } from '@/api/client'
import type { AuditDto, ConnectionDto, TokenDto } from '@/api/types'
import AuditTable from '@/components/AuditTable.vue'
import { AUDIT_KINDS, kindLabel } from '@/lib/audit'
import { dayBoundary } from '@/lib/time'

const PAGE = 100

const entries = ref<AuditDto[]>([])
const next = ref<string | null>(null)
const connections = ref<ConnectionDto[]>([])
const tokens = ref<TokenDto[]>([])
const error = ref<string | null>(null)
const loading = ref(false)

const filters = reactive({
  connection: '',
  token: '',
  kind: '',
  tool: '',
  from: '',
  to: '',
})

// The query the rows on screen came from. Paging walks on from this rather
// than from the live filters, so a filter typed but not applied cannot make
// the second page of a result set come from a different search.
const applied = ref<AuditQuery>({ limit: PAGE })

const query = computed<AuditQuery>(() => ({
  connection: filters.connection ? Number(filters.connection) : undefined,
  token: filters.token ? Number(filters.token) : undefined,
  kind: filters.kind || undefined,
  tool: filters.tool.trim() || undefined,
  from: filters.from ? dayBoundary(filters.from, false) : undefined,
  to: filters.to ? dayBoundary(filters.to, true) : undefined,
  limit: PAGE,
}))

async function search() {
  loading.value = true
  error.value = null
  const asked = query.value
  try {
    const page = await api.audit(asked)
    applied.value = asked
    entries.value = page.entries
    next.value = page.next ?? null
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

async function more() {
  if (!next.value) return
  loading.value = true
  try {
    const page = await api.audit({ ...applied.value, before: next.value })
    entries.value = [...entries.value, ...page.entries]
    next.value = page.next ?? null
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  } finally {
    loading.value = false
  }
}

function reset() {
  filters.connection = ''
  filters.token = ''
  filters.kind = ''
  filters.tool = ''
  filters.from = ''
  filters.to = ''
  search()
}

onMounted(async () => {
  try {
    const [c, t] = await Promise.all([api.connections.list(), api.tokens.list()])
    connections.value = c
    tokens.value = t
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
  await search()
})
</script>

<template>
  <main class="mx-auto max-w-6xl space-y-4 px-4 py-6" data-testid="activity-view">
    <div>
      <h1 class="text-xl font-semibold">Activity</h1>
      <p class="mt-1 text-sm text-muted">
        Every tool call, link and grant, newest first. Arguments are stripped of message bodies;
        tool output is never logged.
      </p>
    </div>

    <p v-if="error" class="note-danger" data-testid="activity-error">{{ error }}</p>

    <form
      class="flex flex-wrap items-end gap-3 card p-4"
      data-testid="activity-filters"
      @submit.prevent="search"
    >
      <label class="text-sm">
        Connection<br />
        <select v-model="filters.connection" data-testid="filter-connection">
          <option value="">any</option>
          <option v-for="c in connections" :key="c.id" :value="String(c.id)">{{ c.label }}</option>
        </select>
      </label>
      <label class="text-sm">
        Token<br />
        <select v-model="filters.token" data-testid="filter-token">
          <option value="">any</option>
          <option v-for="t in tokens" :key="t.id" :value="String(t.id)">{{ t.name }}</option>
        </select>
      </label>
      <label class="text-sm">
        Kind<br />
        <select v-model="filters.kind" data-testid="filter-kind">
          <option value="">any</option>
          <option v-for="k in AUDIT_KINDS" :key="k" :value="k">{{ kindLabel(k) }}</option>
        </select>
      </label>
      <label class="text-sm">
        Tool<br />
        <input
          v-model="filters.tool"
          class="w-44"
          placeholder="gmail_search"
          data-testid="filter-tool"
        />
      </label>
      <label class="text-sm">
        From<br />
        <input v-model="filters.from" type="date" data-testid="filter-from" />
      </label>
      <label class="text-sm">
        To<br />
        <input v-model="filters.to" type="date" data-testid="filter-to" />
      </label>
      <button class="btn" type="submit" :disabled="loading" data-testid="filter-apply">
        Apply
      </button>
      <button class="btn-secondary" type="button" @click="reset">Clear</button>
    </form>

    <AuditTable
      :entries="entries"
      :connections="connections"
      :tokens="tokens"
      wide
      empty="Nothing matches those filters."
    />

    <div class="flex items-center gap-3">
      <button
        v-if="next"
        class="btn-secondary"
        :disabled="loading"
        data-testid="load-more"
        @click="more"
      >
        Load more
      </button>
      <span class="text-xs text-muted">{{ entries.length }} rows</span>
    </div>
  </main>
</template>
