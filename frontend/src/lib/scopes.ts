// The token grid, as logic rather than as markup.
//
// A token's capabilities are `service:level` strings and the registry at
// /api/scopes says which of them needs which other one: a write level is
// useless without the read level of the same service, and the server refuses
// the pair outright. So the grid ticks the requirement along, and unticking a
// requirement takes everything that leans on it with it — otherwise a person
// unticks `gmail:read` and the form quietly posts a set the API rejects.
//
// Everything here is a pure function over the registry so the rule is tested
// without a component.

import type { ScopeDto, ScopeRegistry, TokenDto, TokenInput } from '@/api/types'

/** Every service scope the registry knows, in its canonical order. */
export function allScopes(registry: ScopeRegistry): ScopeDto[] {
  return registry.services.flatMap((s) => s.levels)
}

function requirementOf(registry: ScopeRegistry, scope: string): string | null {
  return allScopes(registry).find((s) => s.scope === scope)?.requires ?? null
}

/** Registry order, so a set never depends on the order it was ticked in. */
function inOrder(registry: ScopeRegistry, chosen: Iterable<string>): string[] {
  const want = new Set(chosen)
  return allScopes(registry)
    .map((s) => s.scope)
    .filter((s) => want.has(s))
}

/**
 * Tick a scope and everything it needs. The chain is followed rather than
 * assumed one deep, so a third level added to a service later still works.
 */
export function tick(registry: ScopeRegistry, chosen: readonly string[], scope: string): string[] {
  const next = new Set(chosen)
  const seen = new Set<string>()
  let current: string | null = scope
  while (current && !seen.has(current)) {
    seen.add(current)
    next.add(current)
    current = requirementOf(registry, current)
  }
  return inOrder(registry, next)
}

/** Everything that needs `scope`, directly or through another scope. */
function dependants(registry: ScopeRegistry, scope: string): string[] {
  const doomed = new Set([scope])
  let grew = true
  while (grew) {
    grew = false
    for (const s of allScopes(registry)) {
      if (s.requires && doomed.has(s.requires) && !doomed.has(s.scope)) {
        doomed.add(s.scope)
        grew = true
      }
    }
  }
  return [...doomed]
}

/** Untick a scope and everything that would be left dangling. */
export function untick(
  registry: ScopeRegistry,
  chosen: readonly string[],
  scope: string,
): string[] {
  const gone = new Set(dependants(registry, scope))
  return inOrder(
    registry,
    chosen.filter((s) => !gone.has(s)),
  )
}

/** What a checkbox does, either way. */
export function setScope(
  registry: ScopeRegistry,
  chosen: readonly string[],
  scope: string,
  on: boolean,
): string[] {
  return on ? tick(registry, chosen, scope) : untick(registry, chosen, scope)
}

/**
 * A delegate token belongs to nobody: it reaches whatever the acting person
 * has flagged for the gateway, so it has no connection list of its own and
 * the API refuses one. The picker is therefore disabled, not merely ignored.
 */
export function pickerDisabled(delegate: boolean): boolean {
  return delegate
}

export type TokenForm = {
  name: string
  client: string
  scopes: string[]
  delegate: boolean
  allConnections: boolean
  connectionIds: number[]
}

export function emptyForm(client = 'generic'): TokenForm {
  return {
    name: '',
    client,
    scopes: [],
    delegate: false,
    allConnections: true,
    connectionIds: [],
  }
}

/** The body of `POST /api/tokens`, with the delegate rule applied. */
export function tokenInput(registry: ScopeRegistry, form: TokenForm): TokenInput {
  const scopes = inOrder(registry, form.scopes)
  if (form.delegate) {
    return { name: form.name.trim(), client: form.client, scopes: [...scopes, 'delegate'] }
  }
  return {
    name: form.name.trim(),
    client: form.client,
    scopes,
    all_connections: form.allConnections,
    connection_ids: form.allConnections ? [] : form.connectionIds,
  }
}

/** Why the create button is off, or null when it is on. */
export function whyNotCreatable(form: TokenForm): string | null {
  if (!form.name.trim()) return 'a token needs a name'
  if (form.scopes.length === 0) return 'tick at least one capability'
  if (!form.delegate && !form.allConnections && form.connectionIds.length === 0)
    return 'pick at least one connection'
  return null
}

/** A token copied into the form, and what the copy had to leave out. */
export type CopiedForm = {
  form: TokenForm
  /** Capabilities the registry no longer knows. */
  droppedScopes: string[]
  /** Allowlisted connections that no longer exist. */
  droppedConnections: number[]
  /** A client profile the registry no longer knows, or null. */
  droppedClient: string | null
}

/**
 * Copy a token into the form, so the person can change one thing and create
 * it again. The copy goes through the same rules as the checkboxes: every
 * scope is ticked with `setScope`, so a write level brings its read level.
 * The form never holds anything the server would refuse. A capability, a
 * connection or a client profile that is gone is left out, and the result
 * names it.
 */
export function formFromToken(
  registry: ScopeRegistry,
  token: TokenDto,
  connections: readonly { id: number }[],
): CopiedForm {
  const known = new Set(allScopes(registry).map((s) => s.scope))
  let scopes: string[] = []
  const droppedScopes: string[] = []
  for (const scope of token.scopes) {
    // The delegate flag travels in the scope list; the form has its own box.
    if (scope === registry.delegate.scope) continue
    if (known.has(scope)) scopes = setScope(registry, scopes, scope, true)
    else droppedScopes.push(scope)
  }

  const fallback = registry.clients[0] ?? 'generic'
  const clientKnown = registry.clients.includes(token.client)

  // A delegate token has no list of its own, and neither does a token for
  // every connection. Only an allowlist is copied, and only what still exists.
  const allowlist = !token.delegate && !token.all_connections
  const exists = new Set(connections.map((c) => c.id))
  const listed = allowlist ? token.connection_ids : []

  return {
    form: {
      name: token.name,
      client: clientKnown ? token.client : fallback,
      scopes,
      delegate: token.delegate,
      allConnections: !allowlist,
      connectionIds: listed.filter((id) => exists.has(id)),
    },
    droppedScopes,
    droppedConnections: listed.filter((id) => !exists.has(id)),
    droppedClient: clientKnown ? null : token.client,
  }
}

/** One sentence per thing a copy left out; empty when it left out nothing. */
export function copyNotes(copy: CopiedForm): string[] {
  const notes: string[] = []
  const scopes = copy.droppedScopes
  if (scopes.length === 1)
    notes.push(`Not copied: ${scopes[0]}. This server no longer knows that capability.`)
  else if (scopes.length > 1)
    notes.push(`Not copied: ${scopes.join(', ')}. This server no longer knows these capabilities.`)
  const gone = copy.droppedConnections.map((id) => `connection #${id}`)
  if (gone.length === 1) notes.push(`Not copied: ${gone[0]}. It no longer exists.`)
  else if (gone.length > 1) notes.push(`Not copied: ${gone.join(', ')}. They no longer exist.`)
  if (copy.droppedClient)
    notes.push(
      `Not copied: client profile ${copy.droppedClient}. This server no longer knows it, so the form uses ${copy.form.client}.`,
    )
  return notes
}

/**
 * The active tokens that already carry this name, trimmed and compared
 * without regard to case. A revoked token does not count: its name is free.
 */
export function activeNamed(tokens: readonly TokenDto[], name: string): TokenDto[] {
  const want = name.trim().toLowerCase()
  return tokens.filter((t) => !t.revoked_at && t.name.trim().toLowerCase() === want)
}
