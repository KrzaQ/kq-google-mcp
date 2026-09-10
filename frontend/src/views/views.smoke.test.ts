// Every view mounts against fixture API responses without a console error,
// and each one is prodded where it does something the fixtures alone cannot
// prove: the reconnect hand-over, the callback banner, the secret and its
// snippets, the log's cursor.
import { flushPromises, mount } from '@vue/test-utils'
import { createPinia, setActivePinia } from 'pinia'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createMemoryHistory, createRouter } from 'vue-router'
import * as fixtures from '@/api/fixtures'
import { useSession } from '@/stores/session'
import ActivityView from './ActivityView.vue'
import ConnectionsView from './ConnectionsView.vue'
import HomeView from './HomeView.vue'
import LoginView from './LoginView.vue'
import NotFoundView from './NotFoundView.vue'
import TokensView from './TokensView.vue'

function json(data: unknown, status = 200) {
  return new Response(JSON.stringify(data), {
    status,
    headers: { 'content-type': 'application/json' },
  })
}

const calls: string[] = []
const bodies = new Map<string, unknown>()

function fakeFetch(input: RequestInfo | URL, init?: RequestInit): Promise<Response> {
  const url = String(input)
  const path = url.split('?')[0]!
  const method = init?.method ?? 'GET'
  calls.push(`${method} ${url}`)
  if (typeof init?.body === 'string') bodies.set(`${method} ${path}`, JSON.parse(init.body))

  if (path === '/api/me') return Promise.resolve(json(fixtures.me))
  if (path === '/api/connections' && method === 'GET')
    return Promise.resolve(json(fixtures.connections))
  if (path === '/api/connections/start')
    return Promise.resolve(json({ url: 'https://accounts.google.example/o/oauth2/v2/auth?x=1' }))
  if (path.endsWith('/reconnect'))
    return Promise.resolve(json({ url: 'https://accounts.google.example/o/oauth2/v2/auth?x=2' }))
  if (path.startsWith('/api/connections/') && method === 'PATCH')
    return Promise.resolve(json(fixtures.connections[0]))
  if (path.startsWith('/api/connections/') && method === 'DELETE')
    return Promise.resolve(new Response(null, { status: 204 }))
  if (path === '/api/tokens' && method === 'GET') return Promise.resolve(json(fixtures.tokens))
  if (path === '/api/tokens' && method === 'POST')
    return Promise.resolve(
      json({ ...fixtures.tokens[0], name: 'new', secret: 'gg_shown_once' }, 201),
    )
  if (path.startsWith('/api/tokens/') && method === 'DELETE')
    return Promise.resolve(new Response(null, { status: 204 }))
  if (path === '/api/scopes') return Promise.resolve(json(fixtures.registry))
  if (path === '/api/audit') {
    if (url.includes('before='))
      return Promise.resolve(json({ entries: [fixtures.audit.entries[0]], next: null }))
    return Promise.resolve(json(fixtures.audit))
  }
  return Promise.resolve(
    json({ error: { code: 'not_found', message: `no fixture for ${url}` } }, 404),
  )
}

describe('views', () => {
  let errors: unknown[][]
  let assign: ReturnType<typeof vi.fn>

  beforeEach(async () => {
    setActivePinia(createPinia())
    vi.stubGlobal('fetch', vi.fn(fakeFetch))
    assign = vi.fn()
    // jsdom refuses a real navigation; the views only ever ask for one.
    Object.defineProperty(window, 'location', {
      value: { ...window.location, origin: 'https://gmcp.example', assign },
      writable: true,
      configurable: true,
    })
    calls.length = 0
    bodies.clear()
    errors = []
    vi.spyOn(console, 'error').mockImplementation((...args) => errors.push(args))
    vi.spyOn(console, 'warn').mockImplementation((...args) => errors.push(args))
    await useSession().load()
  })

  afterEach(() => {
    vi.restoreAllMocks()
    vi.unstubAllGlobals()
  })

  async function render(component: unknown, path = '/') {
    const router = createRouter({
      history: createMemoryHistory(),
      routes: [{ path: '/:pathMatch(.*)*', component: { template: '<div />' } }],
    })
    await router.push(path)
    await router.isReady()
    const w = mount(component as never, { global: { plugins: [router] } })
    await flushPromises()
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

  it('HomeView shows a card per connection and the last of the log', async () => {
    const w = await render(HomeView)
    const cards = w.findAll('[data-testid="connection-card"]')
    expect(cards).toHaveLength(2)
    expect(cards[0]!.text()).toContain('work')
    expect(cards[0]!.text()).toContain('alice@work.example')
    expect(cards[0]!.find('[data-testid="status-badge"]').text()).toBe('ok')
    expect(cards[1]!.find('[data-testid="status-badge"]').text()).toBe('needs re-auth')
    expect(w.findAll('[data-testid="audit-row"]')).toHaveLength(3)
    expect(w.find('[data-testid="audit-table"]').text()).toContain('gmail_search')
    expect(calls.some((c) => c.startsWith('GET /api/audit?limit=50'))).toBe(true)
    expect(errors).toEqual([])
  })

  it('HomeView offers Reconnect only where Google wants the person back', async () => {
    const w = await render(HomeView)
    const buttons = w.findAll('[data-testid="reconnect"]')
    expect(buttons).toHaveLength(1)
    await buttons[0]!.trigger('click')
    await flushPromises()
    expect(calls).toContain('POST /api/connections/2/reconnect')
    expect(assign).toHaveBeenCalledWith('https://accounts.google.example/o/oauth2/v2/auth?x=2')
    expect(errors).toEqual([])
  })

  it('ConnectionsView warns about a partial grant and hands the gateway toggle on', async () => {
    const w = await render(ConnectionsView, '/connections')
    expect(w.findAll('[data-testid="connection-row"]')).toHaveLength(2)
    expect(w.findAll('[data-testid="partial-warning"]')).toHaveLength(1)
    const toggles = w.findAll('[data-testid="gateway-toggle"]')
    expect((toggles[0]!.element as HTMLInputElement).checked).toBe(true)
    await toggles[1]!.setValue(true)
    await flushPromises()
    expect(bodies.get('PATCH /api/connections/2')).toEqual({ delegate_ok: true })
    expect(errors).toEqual([])
  })

  it('ConnectionsView ticks drive along with docs and locks it', async () => {
    const w = await render(ConnectionsView, '/connections')
    const drive = w.find('[data-testid="service-drive"]')
    expect((drive.element as HTMLInputElement).checked).toBe(false)
    await w.find('[data-testid="service-docs"]').setValue(true)
    expect((drive.element as HTMLInputElement).checked).toBe(true)
    expect((drive.element as HTMLInputElement).disabled).toBe(true)
    expect(w.find('[data-testid="drive-note"]').exists()).toBe(true)

    await w.find('[data-testid="connect-label"]').setValue('second')
    await w.find('[data-testid="connect-form"]').trigger('submit')
    await flushPromises()
    expect(bodies.get('POST /api/connections/start')).toEqual({
      label: 'second',
      services: ['gmail', 'drive', 'docs'],
    })
    expect(assign).toHaveBeenCalledWith('https://accounts.google.example/o/oauth2/v2/auth?x=1')
    expect(errors).toEqual([])
  })

  it('ConnectionsView reads the callback banner and then clears the query', async () => {
    const w = await render(ConnectionsView, '/connections?connected=1')
    expect(w.find('[data-testid="callback-banner"]').text()).toContain('work')
    expect(w.vm.$route.query).toEqual({})
    expect(errors).toEqual([])
  })

  it('ConnectionsView turns a callback error code into a sentence', async () => {
    const w = await render(ConnectionsView, '/connections?error=different_account')
    const banner = w.find('[data-testid="callback-banner"]')
    expect(banner.text()).toContain('different Google account')
    expect(banner.classes()).toContain('note-danger')
    expect(errors).toEqual([])
  })

  it('ConnectionsView asks before removing a connection', async () => {
    const w = await render(ConnectionsView, '/connections')
    await w.findAll('[data-testid="remove"]')[0]!.trigger('click')
    await flushPromises()
    expect(calls.some((c) => c.startsWith('DELETE'))).toBe(false)
    const dialog = document.querySelector('[data-testid="confirm-yes"]') as HTMLElement
    expect(dialog).toBeTruthy()
    dialog.click()
    await flushPromises()
    expect(calls).toContain('DELETE /api/connections/1')
    expect(errors).toEqual([])
  })

  it('TokensView lists tokens and draws the grid from the registry', async () => {
    const w = await render(TokensView, '/tokens')
    expect(w.findAll('[data-testid="token-row"]')).toHaveLength(2)
    expect(w.text()).toContain('all connections')
    expect(w.text()).toContain('gateway')
    expect(w.find('[data-testid="scope-gmail:draft"]').exists()).toBe(true)
    expect(w.find('[data-testid="scope-calendar:write"]').exists()).toBe(true)
    expect(errors).toEqual([])
  })

  it('TokensView ticks the read level along and disables the picker for a delegate', async () => {
    const w = await render(TokensView, '/tokens')
    await w.find('[data-testid="scope-docs:write"]').setValue(true)
    expect((w.find('[data-testid="scope-docs:read"]').element as HTMLInputElement).checked).toBe(
      true,
    )
    const picker = w.find('[data-testid="connection-picker"]')
    expect((picker.element as HTMLFieldSetElement).disabled).toBe(false)
    await w.find('[data-testid="token-delegate"]').setValue(true)
    expect((picker.element as HTMLFieldSetElement).disabled).toBe(true)
    expect(w.find('[data-testid="picker-note"]').exists()).toBe(true)
    expect(errors).toEqual([])
  })

  it('TokensView shows the secret once, with a snippet per client', async () => {
    const w = await render(TokensView, '/tokens')
    await w.find('[data-testid="token-name"]').setValue('claude-code')
    await w.find('[data-testid="scope-gmail:draft"]').setValue(true)
    await w.find('[data-testid="token-form"]').trigger('submit')
    await flushPromises()
    expect(bodies.get('POST /api/tokens')).toEqual({
      name: 'claude-code',
      client: 'generic',
      scopes: ['gmail:read', 'gmail:draft'],
      all_connections: true,
      connection_ids: [],
    })
    const shown = w.find('[data-testid="token-secret"]')
    expect(shown.text()).toContain('gg_shown_once')
    expect(w.find('[data-testid="snippet-claude-code"]').text()).toContain(
      'claude mcp add --transport http gmcp https://gmcp.example/mcp',
    )
    expect(w.find('[data-testid="snippet-opencode"]').text()).toContain('"type": "remote"')
    const openwebui = w.find('[data-testid="snippet-openwebui"]').text()
    // Its header box is parsed as JSON, and each value goes into its own input.
    expect(openwebui).toContain('{"X-Gmcp-User": "{{USER_EMAIL}}"}')
    expect(openwebui).toContain('Extra headers')
    await w.find('[data-testid="secret-done"]').trigger('click')
    expect(w.find('[data-testid="token-secret"]').exists()).toBe(false)
    expect(errors).toEqual([])
  })

  it('ActivityView filters and pages by the cursor', async () => {
    const w = await render(ActivityView, '/activity')
    expect(w.findAll('[data-testid="audit-row"]')).toHaveLength(3)

    await w.find('[data-testid="filter-kind"]').setValue('tool_call')
    await w.find('[data-testid="filter-connection"]').setValue('1')
    await w.find('[data-testid="filter-tool"]').setValue('gmail_search')
    await w.find('[data-testid="filter-from"]').setValue('2026-09-01')
    await w.find('[data-testid="activity-filters"]').trigger('submit')
    await flushPromises()
    const filtered = calls.filter((c) => c.startsWith('GET /api/audit')).at(-1)!
    expect(filtered).toContain('connection=1')
    expect(filtered).toContain('kind=tool_call')
    expect(filtered).toContain('tool=gmail_search')
    // A day typed in the filter becomes an instant on the reader's own clock;
    // which instant is lib/time's business and is tested there.
    expect(filtered).toMatch(/from=\d{4}-\d{2}-\d{2}T/)

    await w.find('[data-testid="load-more"]').trigger('click')
    await flushPromises()
    const paged = calls.filter((c) => c.startsWith('GET /api/audit')).at(-1)!
    expect(paged).toContain(`before=${encodeURIComponent(fixtures.audit.next!)}`)
    expect(w.findAll('[data-testid="audit-row"]')).toHaveLength(4)
    expect(w.find('[data-testid="load-more"]').exists()).toBe(false)
    expect(errors).toEqual([])
  })

  it('ActivityView pages the query it applied, not the one being typed', async () => {
    const w = await render(ActivityView, '/activity')
    await w.find('[data-testid="filter-tool"]').setValue('gmail_search')
    await w.find('[data-testid="activity-filters"]').trigger('submit')
    await flushPromises()

    // The person starts typing a second search and does not press Apply. The
    // rows on screen are still the first one's, and so is its next page.
    await w.find('[data-testid="filter-tool"]').setValue('drive_search')
    await w.find('[data-testid="load-more"]').trigger('click')
    await flushPromises()
    const paged = calls.filter((c) => c.startsWith('GET /api/audit')).at(-1)!
    expect(paged).toContain('tool=gmail_search')
    expect(paged).not.toContain('drive_search')
    expect(paged).toContain(`before=${encodeURIComponent(fixtures.audit.next!)}`)
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
