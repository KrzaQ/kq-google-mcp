<script setup lang="ts">
// The kq mark with its Q repainted in Google's four colours, one to a side.
// The silhouette is the house logo, untouched; only the Q's fill says which
// of the house tools this one is. Same drawing as public/favicon.svg.
//
// Each instance needs its own clip-path ids, because ids are global to the
// document and a second copy would otherwise clip against the first one's.
let seq = 0
const uid = `kq${(seq += 1)}`
withDefaults(defineProps<{ size?: number; wordmark?: boolean }>(), { size: 28, wordmark: true })

const Q =
  'M1155.39,1337.31L1219,1337.31L1219,1414.85L1187.19,1400.92L1155.39,1400.92L1155.39,1337.31Z' +
  'M1167.73,1349.65L1206.66,1349.65L1206.66,1397.11L1187.19,1388.59L1167.73,1388.59L1167.73,1349.65Z'

// Quarter wedges from the middle of the Q, so every side of the ring is one
// flat colour and the seams fall on the corner diagonals.
const sides = [
  { id: 'qt', wedge: 'M1187.19,1376.08 L1045.77,1234.66 L1328.61,1234.66 Z', fill: '#4285F4' },
  { id: 'qr', wedge: 'M1187.19,1376.08 L1328.61,1234.66 L1328.61,1517.50 Z', fill: '#EA4335' },
  { id: 'qb', wedge: 'M1187.19,1376.08 L1328.61,1517.50 L1045.77,1517.50 Z', fill: '#FBBC05' },
  { id: 'ql', wedge: 'M1187.19,1376.08 L1045.77,1517.50 L1045.77,1234.66 Z', fill: '#34A853' },
]
</script>

<template>
  <span class="inline-flex items-center gap-2">
    <svg
      :width="size"
      :height="size"
      viewBox="911.7 -99.2 1812.4 1812.4"
      xmlns="http://www.w3.org/2000/svg"
      style="fill-rule: evenodd; clip-rule: evenodd"
      aria-hidden="true"
    >
      <defs>
        <clipPath v-for="s in sides" :id="`${uid}-${s.id}`" :key="s.id">
          <path :d="s.wedge" />
        </clipPath>
      </defs>
      <g transform="translate(0,-738.189)">
        <g transform="matrix(20.8175,0,0,20.8175,-22896.2,-27101.3)">
          <g v-for="s in sides" :key="s.id" :clip-path="`url(#${uid}-${s.id})`">
            <path :d="Q" :fill="s.fill" />
          </g>
        </g>
      </g>
    </svg>
    <span v-if="wordmark" class="text-lg font-semibold tracking-tight text-fg">gmcp</span>
  </span>
</template>
