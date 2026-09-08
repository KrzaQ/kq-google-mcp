<script setup lang="ts">
// Bearer tokens for MCP clients. The capability grid is drawn from
// /api/scopes rather than from a copy of the matrix here, so adding a service
// stays one change on the server; the ticking rules live in lib/scopes.ts.
//
// The secret exists for one render. Everything a person needs to paste it
// somewhere is shown next to it, because there is no second chance.
import { computed, onMounted, ref } from 'vue'
import { api } from '@/api/client'
import type { ConnectionDto, ScopeRegistry, TokenCreated, TokenDto } from '@/api/types'
import ConfirmDialog from '@/components/ConfirmDialog.vue'
import {
  emptyForm,
  pickerDisabled,
  setScope,
  tokenInput,
  whyNotCreatable,
  type TokenForm,
} from '@/lib/scopes'
import { publicOrigin, snippetsFor } from '@/lib/snippets'
import { formatAgo, formatMinute } from '@/lib/time'

const registry = ref<ScopeRegistry | null>(null)
const tokens = ref<TokenDto[]>([])
const connections = ref<ConnectionDto[]>([])
const created = ref<TokenCreated | null>(null)
const error = ref<string | null>(null)
const revoking = ref<TokenDto | null>(null)
const form = ref<TokenForm>(emptyForm())

const pickerOff = computed(() => pickerDisabled(form.value.delegate))
const blocked = computed(() => whyNotCreatable(form.value))
const snippets = computed(() =>
  created.value ? snippetsFor(publicOrigin(), created.value.secret) : [],
)

async function load() {
  error.value = null
  try {
    const [r, t, c] = await Promise.all([api.scopes(), api.tokens.list(), api.connections.list()])
    registry.value = r
    tokens.value = t
    connections.value = c
    if (!form.value.client) form.value.client = r.clients[0] ?? 'generic'
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}

function toggleScope(scope: string, on: boolean) {
  if (!registry.value) return
  form.value.scopes = setScope(registry.value, form.value.scopes, scope, on)
}

function toggleConnection(id: number, on: boolean) {
  const ids = new Set(form.value.connectionIds)
  if (on) ids.add(id)
  else ids.delete(id)
  form.value.connectionIds = [...ids]
}

async function create() {
  if (!registry.value || blocked.value) return
  error.value = null
  try {
    created.value = await api.tokens.create(tokenInput(registry.value, form.value))
    form.value = emptyForm(registry.value.clients[0] ?? 'generic')
    tokens.value = await api.tokens.list()
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}

async function revoke() {
  const t = revoking.value
  revoking.value = null
  if (!t) return
  try {
    await api.tokens.revoke(t.id)
    tokens.value = await api.tokens.list()
  } catch (e) {
    error.value = e instanceof Error ? e.message : String(e)
  }
}

function reach(t: TokenDto): string {
  if (t.delegate) return 'the gateway’s flagged connections'
  if (t.all_connections) return 'all connections'
  const names = t.connection_ids.map(
    (id) => connections.value.find((c) => c.id === id)?.label ?? `#${id}`,
  )
  return names.length ? names.join(', ') : 'none'
}

onMounted(load)
</script>

<template>
  <main class="mx-auto max-w-5xl space-y-6 px-4 py-6" data-testid="tokens-view">
    <div>
      <h1 class="text-xl font-semibold">Tokens</h1>
      <p class="mt-1 text-sm text-muted">
        Bearer tokens for MCP clients. A token's capabilities decide which tools it can even see. A
        personal token acts as you; a <code>delegate</code> token is for a gateway such as Open
        WebUI, names the acting person in <code>X-Gmcp-User</code> on every call, and reaches only
        the connections flagged for the gateway.
      </p>
    </div>

    <p v-if="error" class="note-danger" data-testid="tokens-error">{{ error }}</p>

    <section v-if="created" class="note-ok space-y-3" data-testid="token-secret">
      <p class="font-medium">
        Token "{{ created.name }}" created. Copy it now; it is never shown again.
      </p>
      <code class="block rounded bg-surface px-3 py-2 font-mono text-xs break-all select-all">{{
        created.secret
      }}</code>
      <div v-for="s in snippets" :key="s.id" class="space-y-1" :data-testid="`snippet-${s.id}`">
        <p class="text-xs font-medium text-fg">{{ s.title }}</p>
        <pre
          class="overflow-x-auto rounded bg-surface px-3 py-2 font-mono text-xs text-fg select-all"
          >{{ s.body }}</pre>
        <p v-if="s.note" class="text-xs text-muted">{{ s.note }}</p>
      </div>
      <button class="btn-secondary" data-testid="secret-done" @click="created = null">Done</button>
    </section>

    <div class="overflow-x-auto card">
      <table class="w-full text-sm">
        <thead class="table-head">
          <tr>
            <th class="px-3 py-2 font-medium">Name</th>
            <th class="px-3 py-2 font-medium">Client</th>
            <th class="px-3 py-2 font-medium">Capabilities</th>
            <th class="px-3 py-2 font-medium">Reaches</th>
            <th class="px-3 py-2 font-medium">Created</th>
            <th class="px-3 py-2 font-medium">Last used</th>
            <th class="px-3 py-2"></th>
          </tr>
        </thead>
        <tbody>
          <tr
            v-for="t in tokens"
            :key="t.id"
            class="border-t border-edge align-top"
            :class="{ 'text-faint line-through': t.revoked_at }"
            data-testid="token-row"
          >
            <td class="px-3 py-2">{{ t.name }}</td>
            <td class="px-3 py-2">{{ t.client }}</td>
            <td class="px-3 py-2 font-mono text-xs">{{ t.scopes.join(' ') }}</td>
            <td class="px-3 py-2">{{ reach(t) }}</td>
            <td class="px-3 py-2 font-mono text-xs whitespace-nowrap">
              {{ formatMinute(t.created_at) }}
            </td>
            <td class="px-3 py-2 text-xs whitespace-nowrap">{{ formatAgo(t.last_used_at) }}</td>
            <td class="px-3 py-2 text-right">
              <button
                v-if="!t.revoked_at"
                class="link text-xs text-danger"
                data-testid="revoke"
                @click="revoking = t"
              >
                Revoke
              </button>
            </td>
          </tr>
          <tr v-if="tokens.length === 0">
            <td colspan="7" class="px-3 py-6 text-center text-muted">No tokens yet.</td>
          </tr>
        </tbody>
      </table>
    </div>

    <form
      v-if="registry"
      class="card space-y-4 p-4"
      data-testid="token-form"
      @submit.prevent="create"
    >
      <h2 class="font-medium">New token</h2>

      <div class="flex flex-wrap items-end gap-4">
        <label class="text-sm">
          Name<br />
          <input
            v-model="form.name"
            required
            class="w-48"
            placeholder="claude-code"
            data-testid="token-name"
          />
        </label>
        <label class="text-sm">
          Client profile<br />
          <select v-model="form.client" data-testid="token-client">
            <option v-for="c in registry.clients" :key="c" :value="c">{{ c }}</option>
          </select>
        </label>
      </div>

      <div class="overflow-x-auto">
        <table class="text-sm" data-testid="scope-grid">
          <thead class="table-head">
            <tr>
              <th class="px-3 py-1.5 font-medium">Service</th>
              <th class="px-3 py-1.5 font-medium">Levels</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="s in registry.services" :key="s.service" class="border-t border-edge">
              <td class="px-3 py-1.5 font-medium">{{ s.service }}</td>
              <td class="px-3 py-1.5">
                <label
                  v-for="l in s.levels"
                  :key="l.scope"
                  class="mr-4 inline-flex items-center gap-1.5"
                  :title="l.tools.join(', ')"
                >
                  <input
                    type="checkbox"
                    :checked="form.scopes.includes(l.scope)"
                    :data-testid="`scope-${l.scope}`"
                    @change="toggleScope(l.scope, ($event.target as HTMLInputElement).checked)"
                  />
                  <span>{{ l.level }}</span>
                </label>
              </td>
            </tr>
          </tbody>
        </table>
      </div>
      <p class="text-xs text-muted">
        A write level is useless without its read level, so ticking one ticks the other; unticking a
        read level drops what leans on it.
      </p>

      <label class="flex items-center gap-2 text-sm">
        <input v-model="form.delegate" type="checkbox" data-testid="token-delegate" />
        <span
          ><code>delegate</code> — a gateway token, acting for whoever
          <code>X-Gmcp-User</code> names</span
        >
      </label>

      <fieldset class="space-y-2" :disabled="pickerOff" data-testid="connection-picker">
        <legend class="text-sm">Connections</legend>
        <label class="flex items-center gap-2 text-sm">
          <input
            v-model="form.allConnections"
            type="radio"
            :value="true"
            data-testid="all-connections"
          />
          <span :class="pickerOff ? 'text-faint' : ''">All my connections, now and later</span>
        </label>
        <label class="flex items-center gap-2 text-sm">
          <input
            v-model="form.allConnections"
            type="radio"
            :value="false"
            data-testid="some-connections"
          />
          <span :class="pickerOff ? 'text-faint' : ''">Only these</span>
        </label>
        <div v-if="!form.allConnections" class="ml-6 flex flex-wrap gap-x-4 gap-y-1">
          <label
            v-for="c in connections"
            :key="c.id"
            class="inline-flex items-center gap-1.5 text-sm"
          >
            <input
              type="checkbox"
              :checked="form.connectionIds.includes(c.id)"
              :data-testid="`connection-${c.id}`"
              @change="toggleConnection(c.id, ($event.target as HTMLInputElement).checked)"
            />
            <span>{{ c.label }}</span>
          </label>
          <span v-if="connections.length === 0" class="text-sm text-muted">
            No connections to pick from yet.
          </span>
        </div>
        <p v-if="pickerOff" class="text-xs text-muted" data-testid="picker-note">
          A delegate token has no list of its own: it reaches whatever the acting person has flagged
          for the gateway.
        </p>
      </fieldset>

      <div class="flex items-center gap-3">
        <button class="btn" type="submit" :disabled="blocked !== null" data-testid="token-create">
          Create token
        </button>
        <span v-if="blocked" class="text-xs text-muted">{{ blocked }}</span>
      </div>
    </form>

    <ConfirmDialog
      :open="revoking !== null"
      title="Revoke token"
      :message="
        revoking
          ? `Revoke ${revoking.name}? Any client still using it stops working immediately.`
          : ''
      "
      confirm-label="Revoke"
      danger
      @confirm="revoke"
      @cancel="revoking = null"
    />
  </main>
</template>
