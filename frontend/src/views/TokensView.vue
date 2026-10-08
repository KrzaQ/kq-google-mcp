<script setup lang="ts">
// Bearer tokens for MCP clients. The capability grid is drawn from
// /api/scopes rather than from a copy of the matrix here, so adding a service
// stays one change on the server; the ticking rules live in lib/scopes.ts.
//
// "Fill as new" copies a token into the form through the same rules the form
// enforces, so replacing a token is one changed box and Create, not retyping.
// A name an active token already carries is asked about before anything is
// created. "Create and revoke" creates first and revokes only after that
// succeeded, so a failure never leaves the person without a working token.
//
// The secret exists for one render. Everything a person needs to paste it
// somewhere is shown next to it, because there is no second chance.
import { computed, onMounted, ref } from 'vue'
import { api } from '@/api/client'
import type { ConnectionDto, ScopeRegistry, TokenCreated, TokenDto } from '@/api/types'
import ConfirmDialog from '@/components/ConfirmDialog.vue'
import {
  activeNamed,
  copyNotes,
  emptyForm,
  formFromToken,
  pickerDisabled,
  setScope,
  tokenInput,
  whyNotCreatable,
  type TokenForm,
} from '@/lib/scopes'
import { publicOrigin, snippetsFor } from '@/lib/snippets'
import { formatAgo, formatMinute } from '@/lib/time'
import { useSession } from '@/stores/session'

const session = useSession()
const registry = ref<ScopeRegistry | null>(null)
const tokens = ref<TokenDto[]>([])
const connections = ref<ConnectionDto[]>([])
const created = ref<TokenCreated | null>(null)
const error = ref<string | null>(null)
const revoking = ref<TokenDto | null>(null)
const form = ref<TokenForm>(emptyForm())
// What the last "Fill as new" had to leave out, one sentence each.
const notes = ref<string[]>([])
const formEl = ref<HTMLFormElement | null>(null)
const nameEl = ref<HTMLInputElement | null>(null)
// The active tokens that already carry the name being created. The question
// is open while this is not empty.
const clash = ref<TokenDto[]>([])
// A revoked token stays in the list for the record, but the person comes here
// to work with the active ones. The choice lasts as long as the page does.
const showRevoked = ref(false)

const revokedCount = computed(() => tokens.value.filter((t) => t.revoked_at).length)
const shownTokens = computed(() =>
  showRevoked.value ? tokens.value : tokens.value.filter((t) => !t.revoked_at),
)

const pickerOff = computed(() => pickerDisabled(form.value.delegate))
const blocked = computed(() => whyNotCreatable(form.value))
const clashText = computed(() => {
  const n = clash.value.length
  const name = clash.value[0]?.name ?? ''
  return n === 1
    ? `An active token is already named "${name}". Create the new token and revoke the old one, or keep both? The old one is revoked only after the new one exists.`
    : `${n} active tokens are already named "${name}". Create the new token and revoke the old ones, or keep them all? The old ones are revoked only after the new one exists.`
})
const snippets = computed(() =>
  created.value ? snippetsFor(publicOrigin(), created.value.secret) : [],
)

function messageOf(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

async function load() {
  error.value = null
  try {
    const [r, t, c] = await Promise.all([api.scopes(), api.tokens.list(), api.connections.list()])
    registry.value = r
    tokens.value = t
    connections.value = c
    if (!form.value.client) form.value.client = r.clients[0] ?? 'generic'
  } catch (e) {
    error.value = messageOf(e)
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

function fillFrom(t: TokenDto) {
  if (!registry.value) return
  const copy = formFromToken(registry.value, t, connections.value)
  form.value = copy.form
  notes.value = copyNotes(copy)
  formEl.value?.scrollIntoView({ behavior: 'smooth', block: 'start' })
  // The smooth scroll is already under way; focus must not jump past it.
  nameEl.value?.focus({ preventScroll: true })
}

function submit() {
  if (!registry.value || blocked.value) return
  const same = activeNamed(tokens.value, form.value.name)
  if (same.length > 0) clash.value = same
  else void create([])
}

function answerClash(revokeOld: boolean) {
  const old = clash.value
  clash.value = []
  void create(revokeOld ? old : [])
}

/**
 * Create the token in the form, then revoke `replaced`. When the create
 * fails, nothing is revoked. When a revoke fails, the message says that the
 * new token exists and the old one still works, so nobody has to guess.
 */
async function create(replaced: readonly TokenDto[]) {
  if (!registry.value) return
  error.value = null
  try {
    created.value = await api.tokens.create(tokenInput(registry.value, form.value))
  } catch (e) {
    error.value = messageOf(e)
    return
  }
  const name = created.value.name
  form.value = emptyForm(registry.value.clients[0] ?? 'generic')
  notes.value = []

  const failures: string[] = []
  for (const old of replaced) {
    try {
      await api.tokens.revoke(old.id)
    } catch (e) {
      failures.push(messageOf(e))
    }
  }
  if (failures.length > 0) {
    const one = failures.length === 1
    const which =
      replaced.length === 1
        ? `The old token "${replaced[0]!.name}" was not revoked`
        : `${failures.length} of the ${replaced.length} old tokens named "${name}" ${one ? 'was' : 'were'} not revoked`
    const still = one ? 'It is still active. Revoke it' : 'They are still active. Revoke them'
    error.value =
      `The new token "${name}" was created, and its secret is below. ` +
      `${which}: ${failures[0]}. ${still} in the table.`
  }

  try {
    tokens.value = await api.tokens.list()
  } catch (e) {
    error.value ??= messageOf(e)
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
    error.value = messageOf(e)
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
          v-if="s.body"
          class="overflow-x-auto rounded bg-surface px-3 py-2 font-mono text-xs text-fg select-all"
          >{{ s.body }}</pre>
        <!-- One box per value: these go into separate inputs, so selecting
             them all together would be the wrong thing to copy. -->
        <dl v-if="s.fields" class="space-y-1">
          <div v-for="f in s.fields" :key="f.label" class="flex items-baseline gap-2">
            <dt class="w-28 shrink-0 text-xs text-muted">{{ f.label }}</dt>
            <dd
              class="min-w-0 flex-1 rounded bg-surface px-3 py-1.5 font-mono text-xs break-all text-fg select-all"
            >
              {{ f.value }}
            </dd>
          </div>
        </dl>
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
            v-for="t in shownTokens"
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
              {{ formatMinute(t.created_at, session.zone) }}
            </td>
            <td class="px-3 py-2 text-xs whitespace-nowrap">
              {{ formatAgo(t.last_used_at, new Date(), session.zone) }}
            </td>
            <td class="px-3 py-2 text-right whitespace-nowrap">
              <button class="link text-xs" data-testid="fill-as-new" @click="fillFrom(t)">
                Fill as new
              </button>
              <button
                v-if="!t.revoked_at"
                class="ml-3 link text-xs text-danger"
                data-testid="revoke"
                @click="revoking = t"
              >
                Revoke
              </button>
            </td>
          </tr>
          <tr v-if="shownTokens.length === 0">
            <td colspan="7" class="px-3 py-6 text-center text-muted" data-testid="tokens-empty">
              {{ tokens.length === 0 ? 'No tokens yet.' : 'No active tokens.' }}
            </td>
          </tr>
        </tbody>
      </table>
      <div v-if="revokedCount > 0" class="border-t border-edge px-3 py-2 text-xs">
        <button class="link" data-testid="toggle-revoked" @click="showRevoked = !showRevoked">
          {{ showRevoked ? 'Hide revoked' : `Show ${revokedCount} revoked` }}
        </button>
      </div>
    </div>

    <form
      v-if="registry"
      ref="formEl"
      class="card space-y-4 p-4"
      data-testid="token-form"
      @submit.prevent="submit"
    >
      <h2 class="font-medium">New token</h2>

      <div class="flex flex-wrap items-end gap-4">
        <label class="text-sm">
          Name<br />
          <input
            ref="nameEl"
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

      <div v-if="notes.length" class="note-warn space-y-1" data-testid="copy-notes">
        <p v-for="n in notes" :key="n">{{ n }}</p>
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

    <ConfirmDialog
      :open="clash.length > 0"
      title="Name already in use"
      :message="clashText"
      :confirm-label="
        clash.length === 1 ? 'Create and revoke the old one' : 'Create and revoke the old ones'
      "
      :other-label="clash.length === 1 ? 'Create and keep both' : 'Create and keep them all'"
      danger
      @confirm="answerClash(true)"
      @other="answerClash(false)"
      @cancel="clash = []"
    />
  </main>
</template>
