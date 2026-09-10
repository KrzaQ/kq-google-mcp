import { defineStore } from 'pinia'
import { computed, ref } from 'vue'
import { ApiError, api } from '@/api/client'
import type { Me } from '@/api/types'
import { systemZone } from '@/lib/time'

export const useSession = defineStore('session', () => {
  const me = ref<Me | null>(null)
  const checked = ref(false)

  /**
   * The clock everything in the portal is shown on: the person's own zone as
   * the server has it. Before the session loads — and for a page nobody is
   * logged in to — the browser's own zone stands in, so nothing is ever
   * rendered in UTC by accident.
   */
  const zone = computed(() => me.value?.user.timezone || systemZone())

  async function load(): Promise<Me | null> {
    try {
      me.value = await api.me()
    } catch (e) {
      if (e instanceof ApiError && e.status === 401) me.value = null
      else throw e
    } finally {
      checked.value = true
    }
    return me.value
  }

  /** The server validates the name; what comes back is what it stored. */
  async function setZone(timezone: string) {
    me.value = await api.updateMe({ timezone })
  }

  async function logout() {
    await api.logout()
    me.value = null
    checked.value = true
  }

  return { me, checked, zone, load, setZone, logout }
})
