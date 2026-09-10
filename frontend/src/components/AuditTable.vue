<script setup lang="ts">
// The log as a table. Home shows the last fifty of it, Activity the filtered
// whole; both want the same six columns, so they share this.
import { computed } from 'vue'
import type { AuditDto } from '@/api/types'
import { auditRow, outcomeClass, type Named } from '@/lib/audit'
import { useSession } from '@/stores/session'

const props = withDefaults(
  defineProps<{
    entries: AuditDto[]
    connections?: Named[]
    tokens?: Named[]
    /** Home leaves out the token and the duration; Activity shows them. */
    wide?: boolean
    empty?: string
  }>(),
  { connections: () => [], tokens: () => [], wide: false, empty: 'Nothing logged yet.' },
)

const session = useSession()

const rows = computed(() =>
  props.entries.map((e) => ({
    ...auditRow(e, props.connections, props.tokens, session.zone),
    duration: e.duration_ms == null ? '' : `${e.duration_ms} ms`,
  })),
)
</script>

<template>
  <div class="overflow-x-auto card">
    <table class="w-full text-sm" data-testid="audit-table">
      <thead class="table-head">
        <tr>
          <th class="px-3 py-2 font-medium whitespace-nowrap">Time ({{ session.zone }})</th>
          <th class="px-3 py-2 font-medium">Kind</th>
          <th class="px-3 py-2 font-medium">Tool</th>
          <th class="px-3 py-2 font-medium">Connection</th>
          <th v-if="wide" class="px-3 py-2 font-medium">Token</th>
          <th class="px-3 py-2 font-medium">Outcome</th>
          <th class="px-3 py-2 font-medium">Detail</th>
          <th v-if="wide" class="px-3 py-2 text-right font-medium">Took</th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="r in rows"
          :key="r.id"
          class="border-t border-edge align-top"
          data-testid="audit-row"
        >
          <td class="px-3 py-1.5 font-mono text-xs whitespace-nowrap">{{ r.time }}</td>
          <td class="px-3 py-1.5 whitespace-nowrap">{{ r.kind }}</td>
          <td class="px-3 py-1.5 font-mono text-xs">{{ r.tool }}</td>
          <td class="px-3 py-1.5">{{ r.connection }}</td>
          <td v-if="wide" class="px-3 py-1.5">{{ r.token }}</td>
          <td class="px-3 py-1.5" :class="outcomeClass(r.outcome)">{{ r.outcome }}</td>
          <td class="max-w-md px-3 py-1.5 text-muted" :title="r.args || undefined">
            {{ r.detail }}
          </td>
          <td v-if="wide" class="px-3 py-1.5 text-right font-mono text-xs text-muted">
            {{ r.duration }}
          </td>
        </tr>
        <tr v-if="rows.length === 0">
          <td :colspan="wide ? 8 : 6" class="px-3 py-6 text-center text-muted">{{ empty }}</td>
        </tr>
      </tbody>
    </table>
  </div>
</template>
