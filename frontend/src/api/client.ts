// Same-origin JSON client. Every failure becomes an ApiError carrying the
// server's error envelope, so views can branch on `code`.
//
// `/api/*` never redirects on 401; the browser has to notice the expired
// session itself. Rather than importing the router here — which would make a
// cycle, since the router imports the session store which imports this file —
// the router registers what to do, and every 401 that is not the session
// probe's own goes through it.

import type {
  AuditPage,
  ConnectInput,
  ConnectionDto,
  ConnectionPatchInput,
  ConsentUrl,
  Me,
  MePatch,
  ReconnectInput,
  ScopeRegistry,
  TokenCreated,
  TokenDto,
  TokenInput,
} from './types'

export class ApiError extends Error {
  constructor(
    public status: number,
    public code: string,
    message: string,
  ) {
    super(message)
  }
}

type Query = Record<string, string | number | boolean | undefined>

function url(path: string, query?: Query): string {
  const params = new URLSearchParams()
  for (const [k, v] of Object.entries(query ?? {})) {
    if (v !== undefined && v !== '') params.set(k, String(v))
  }
  const q = params.toString()
  return q ? `${path}?${q}` : path
}

let onUnauthorized: ((path: string) => void) | null = null

/** The router says where an expired session should land. */
export function setUnauthorizedHandler(handler: (path: string) => void) {
  onUnauthorized = handler
}

export async function request<T>(
  method: string,
  path: string,
  body?: unknown,
  query?: Query,
): Promise<T> {
  const init: RequestInit = { method, headers: {}, credentials: 'same-origin' }
  if (body !== undefined) {
    init.headers = { 'content-type': 'application/json' }
    init.body = JSON.stringify(body)
  }
  const res = await fetch(url(path, query), init)
  if (res.status === 204) return undefined as T
  const text = await res.text()
  let data: unknown = null
  if (text) {
    try {
      data = JSON.parse(text)
    } catch {
      data = text
    }
  }
  if (!res.ok) {
    const env = data as { error?: { code?: string; message?: string } } | null
    if (res.status === 401 && path !== '/api/me') onUnauthorized?.(path)
    throw new ApiError(
      res.status,
      env?.error?.code ?? 'error',
      env?.error?.message ?? res.statusText,
    )
  }
  return data as T
}

export type AuditQuery = {
  from?: string
  to?: string
  connection?: number
  token?: number
  kind?: string
  tool?: string
  limit?: number
  before?: string
}

export const api = {
  me: () => request<Me>('GET', '/api/me'),
  updateMe: (patch: MePatch) => request<Me>('PATCH', '/api/me', patch),
  logout: () => request<void>('POST', '/api/auth/logout'),

  connections: {
    list: () => request<ConnectionDto[]>('GET', '/api/connections'),
    start: (input: ConnectInput) => request<ConsentUrl>('POST', '/api/connections/start', input),
    reconnect: (id: number, input: ReconnectInput = {}) =>
      request<ConsentUrl>('POST', `/api/connections/${id}/reconnect`, input),
    update: (id: number, patch: ConnectionPatchInput) =>
      request<ConnectionDto>('PATCH', `/api/connections/${id}`, patch),
    remove: (id: number) => request<void>('DELETE', `/api/connections/${id}`),
  },
  tokens: {
    list: () => request<TokenDto[]>('GET', '/api/tokens'),
    create: (input: TokenInput) => request<TokenCreated>('POST', '/api/tokens', input),
    revoke: (id: number) => request<void>('DELETE', `/api/tokens/${id}`),
  },
  scopes: () => request<ScopeRegistry>('GET', '/api/scopes'),
  audit: (q: AuditQuery = {}) => request<AuditPage>('GET', '/api/audit', undefined, q),
}
