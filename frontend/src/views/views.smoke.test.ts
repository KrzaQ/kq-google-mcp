// Every view mounts without console errors. The views are empty until the
// API exists; the harness is here so each one is covered as it is filled in.
import { flushPromises, mount } from '@vue/test-utils'
import { createPinia, setActivePinia } from 'pinia'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createMemoryHistory, createRouter } from 'vue-router'
import ActivityView from './ActivityView.vue'
import ConnectionsView from './ConnectionsView.vue'
import HomeView from './HomeView.vue'
import LoginView from './LoginView.vue'
import NotFoundView from './NotFoundView.vue'
import TokensView from './TokensView.vue'

describe('views', () => {
  let errors: unknown[][]
  beforeEach(() => {
    setActivePinia(createPinia())
    errors = []
    vi.spyOn(console, 'error').mockImplementation((...args) => errors.push(args))
    vi.spyOn(console, 'warn').mockImplementation((...args) => errors.push(args))
  })
  afterEach(() => {
    vi.restoreAllMocks()
  })

  async function render(component: unknown, path = '/') {
    const router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/:pathMatch(.*)*', component: { template: '<div />' } }],
    })
    await router.push(path)
    const w = mount(component as never, { global: { plugins: [router] } })
    await flushPromises()
    return w
  }

  const views: [string, unknown, string][] = [
    ['home-view', HomeView, '/'],
    ['connections-view', ConnectionsView, '/connections'],
    ['tokens-view', TokensView, '/tokens'],
    ['activity-view', ActivityView, '/activity'],
    ['login-view', LoginView, '/login'],
    ['not-found-view', NotFoundView, '/nowhere'],
  ]

  it.each(views)('%s mounts', async (testid, component, path) => {
    const w = await render(component, path)
    expect(w.find(`[data-testid="${testid}"]`).exists()).toBe(true)
    expect(errors).toEqual([])
  })

  it('the login button carries the page that was asked for', async () => {
    const w = await render(LoginView, '/login?next=/tokens')
    expect(w.find('[data-testid="login-button"]').attributes('href')).toBe(
      '/api/auth/login?next=%2Ftokens',
    )
  })

  it('an off-site next is ignored', async () => {
    const w = await render(LoginView, '/login?next=//evil.example')
    expect(w.find('[data-testid="login-button"]').attributes('href')).toBe(
      '/api/auth/login?next=%2F',
    )
  })
})
