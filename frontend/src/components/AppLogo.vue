<script setup lang="ts">
// The mark: a rounded tile with a locked door, which is what the portal is —
// the way in to accounts it keeps the keys to. Same drawing as
// public/favicon.svg.
withDefaults(defineProps<{ size?: number; wordmark?: boolean }>(), { size: 28, wordmark: true })
</script>

<template>
  <span class="inline-flex items-center gap-2">
    <svg
      :width="size"
      :height="size"
      viewBox="0 0 64 64"
      xmlns="http://www.w3.org/2000/svg"
      aria-hidden="true"
    >
      <defs>
        <linearGradient id="gmcp-tile" x1="0" y1="0" x2="1" y2="1">
          <stop offset="0" stop-color="#3a8ee0" />
          <stop offset="1" stop-color="#1b4f9c" />
        </linearGradient>
      </defs>
      <rect x="2" y="2" width="60" height="60" rx="14" fill="url(#gmcp-tile)" />
      <path
        fill="#ffffff"
        fill-rule="evenodd"
        d="M32 11c-7.7 0-14 6.3-14 14v28h28V25c0-7.7-6.3-14-14-14Zm0 19a4.5 4.5 0 0 0-2.5 8.2V44h5v-6.8A4.5 4.5 0 0 0 32 30Z"
      />
    </svg>
    <span v-if="wordmark" class="text-lg font-semibold tracking-tight text-fg">gmcp</span>
  </span>
</template>
