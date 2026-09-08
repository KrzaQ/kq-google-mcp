<script setup lang="ts">
// A connection's health at a glance: green works, amber needs the person,
// grey is gone. The colour is the whole point of the home page, so the word
// is next to it rather than only in a tooltip.
import { computed } from 'vue'

const props = defineProps<{ status: string; detail?: string | null }>()

const known: Record<string, { label: string; dot: string; text: string }> = {
  ok: { label: 'ok', dot: 'bg-ok', text: 'text-ok' },
  needs_reauth: { label: 'needs re-auth', dot: 'bg-warn', text: 'text-warn' },
  revoked: { label: 'revoked', dot: 'bg-faint', text: 'text-faint' },
}
const shown = computed(
  () => known[props.status] ?? { label: props.status, dot: 'bg-faint', text: 'text-muted' },
)
</script>

<template>
  <span
    class="inline-flex items-center gap-1.5 text-xs font-medium"
    :class="shown.text"
    :title="detail ?? undefined"
    data-testid="status-badge"
  >
    <span class="inline-block h-2 w-2 rounded-full" :class="shown.dot" aria-hidden="true"></span>
    {{ shown.label }}
  </span>
</template>
