<script setup lang="ts">
import { NavigationFailureType, isNavigationFailure } from 'vue-router'
import AppLogo from '@/components/AppLogo.vue'
import ThemeChooser from '@/components/ThemeChooser.vue'
import { bumpReload } from '@/lib/reload'

const links = [
  { to: '/', label: 'Home', match: '/home' },
  { to: '/connections', label: 'Connections', match: '/connections' },
  { to: '/tokens', label: 'Tokens', match: '/tokens' },
  { to: '/activity', label: 'Activity', match: '/activity' },
]

/**
 * Going nowhere is the interesting case: the router reports a duplicated
 * navigation, and that is the click that asks for the page again. A modified
 * click (new tab, new window) never reaches here; `navigate` lets it be.
 */
async function follow(e: MouseEvent, navigate: (e: MouseEvent) => Promise<unknown>) {
  const failure = await navigate(e)
  if (isNavigationFailure(failure, NavigationFailureType.duplicated)) bumpReload()
}
</script>

<template>
  <header class="border-b border-edge bg-surface">
    <nav class="mx-auto flex max-w-6xl flex-wrap items-center gap-x-5 gap-y-2 px-4 py-2.5">
      <RouterLink v-slot="{ href, navigate }" to="/" custom>
        <a :href="href" class="mr-2" @click="follow($event, navigate)"><AppLogo :size="26" /></a>
      </RouterLink>
      <RouterLink v-for="l in links" :key="l.label" v-slot="{ href, navigate }" :to="l.to" custom>
        <a
          :href="href"
          class="text-sm text-muted hover:text-fg"
          :class="{
            'font-medium text-fg': $route.path === l.to || $route.path.startsWith(l.match),
          }"
          @click="follow($event, navigate)"
        >
          {{ l.label }}
        </a>
      </RouterLink>
      <span class="flex-1"></span>
      <ThemeChooser />
    </nav>
  </header>
</template>
