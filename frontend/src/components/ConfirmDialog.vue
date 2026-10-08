<script setup lang="ts">
import {
  DialogClose,
  DialogContent,
  DialogDescription,
  DialogOverlay,
  DialogPortal,
  DialogRoot,
  DialogTitle,
} from 'reka-ui'

defineProps<{
  open: boolean
  title: string
  message: string
  confirmLabel?: string
  danger?: boolean
  /** A second way to go ahead, shown between Cancel and the confirm button. */
  otherLabel?: string
}>()
const emit = defineEmits<{ confirm: []; other: []; cancel: [] }>()
</script>

<template>
  <DialogRoot :open="open" @update:open="(v: boolean) => !v && emit('cancel')">
    <DialogPortal>
      <DialogOverlay class="fixed inset-0 bg-black/30" />
      <DialogContent
        class="fixed top-1/2 left-1/2 w-[28rem] max-w-[90vw] -translate-x-1/2 -translate-y-1/2 rounded-lg bg-surface p-6 shadow-xl"
        data-testid="confirm-dialog"
      >
        <DialogTitle class="text-lg font-semibold">{{ title }}</DialogTitle>
        <DialogDescription class="mt-2 text-sm text-muted">{{ message }}</DialogDescription>
        <div class="mt-6 flex flex-wrap justify-end gap-2">
          <DialogClose as-child>
            <button class="btn-secondary" @click="emit('cancel')">Cancel</button>
          </DialogClose>
          <button
            v-if="otherLabel"
            class="btn-secondary"
            data-testid="confirm-other"
            @click="emit('other')"
          >
            {{ otherLabel }}
          </button>
          <button
            :class="danger ? 'btn-danger' : 'btn'"
            data-testid="confirm-yes"
            @click="emit('confirm')"
          >
            {{ confirmLabel ?? 'Confirm' }}
          </button>
        </div>
      </DialogContent>
    </DialogPortal>
  </DialogRoot>
</template>
