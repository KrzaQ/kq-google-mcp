import { describe, expect, it } from 'vitest'
import { connections, registry, tokens } from '@/api/fixtures'
import type { TokenDto } from '@/api/types'
import {
  copyNotes,
  emptyForm,
  formFromToken,
  pickerDisabled,
  setScope,
  tick,
  tokenInput,
  untick,
  whyNotCreatable,
} from './scopes'

describe('the scope grid', () => {
  it('ticks the read level along with a write level', () => {
    expect(tick(registry, [], 'docs:write')).toEqual(['docs:read', 'docs:write'])
    expect(tick(registry, [], 'gmail:draft')).toEqual(['gmail:read', 'gmail:draft'])
  })

  it('leaves a read level alone', () => {
    expect(tick(registry, [], 'drive:read')).toEqual(['drive:read'])
  })

  it('unticking a read level drops everything that leans on it', () => {
    const chosen = tick(registry, tick(registry, [], 'gmail:draft'), 'gmail:modify')
    expect(chosen).toEqual(['gmail:read', 'gmail:draft', 'gmail:modify'])
    expect(untick(registry, chosen, 'gmail:read')).toEqual([])
  })

  it('leaves the other services where they were', () => {
    const chosen = ['gmail:read', 'gmail:draft', 'drive:read', 'docs:read', 'docs:write']
    expect(untick(registry, chosen, 'gmail:read')).toEqual([
      'drive:read',
      'docs:read',
      'docs:write',
    ])
  })

  it('unticking a write level keeps the read level', () => {
    expect(untick(registry, ['docs:read', 'docs:write'], 'docs:write')).toEqual(['docs:read'])
  })

  it('keeps the registry order whatever order the boxes are ticked in', () => {
    let chosen: string[] = []
    for (const s of ['calendar:write', 'gmail:draft', 'drive:read'])
      chosen = setScope(registry, chosen, s, true)
    expect(chosen).toEqual([
      'gmail:read',
      'gmail:draft',
      'drive:read',
      'calendar:read',
      'calendar:write',
    ])
  })
})

describe('the delegate checkbox', () => {
  it('disables the connection picker', () => {
    expect(pickerDisabled(false)).toBe(false)
    expect(pickerDisabled(true)).toBe(true)
  })

  it('posts no connection list, because the API refuses one', () => {
    const form = {
      ...emptyForm(),
      name: ' openwebui ',
      client: 'openwebui',
      scopes: ['gmail:read'],
      delegate: true,
      allConnections: false,
      connectionIds: [1, 2],
    }
    expect(tokenInput(registry, form)).toEqual({
      name: 'openwebui',
      client: 'openwebui',
      scopes: ['gmail:read', 'delegate'],
    })
  })

  it('a personal token carries its allowlist', () => {
    const form = {
      ...emptyForm(),
      name: 'claude-code',
      client: 'claude-code',
      scopes: ['drive:read', 'gmail:read'],
      allConnections: false,
      connectionIds: [2],
    }
    expect(tokenInput(registry, form)).toEqual({
      name: 'claude-code',
      client: 'claude-code',
      scopes: ['gmail:read', 'drive:read'],
      all_connections: false,
      connection_ids: [2],
    })
  })

  it('drops the allowlist when every connection is meant', () => {
    const form = {
      ...emptyForm(),
      name: 'all',
      scopes: ['gmail:read'],
      allConnections: true,
      connectionIds: [1],
    }
    expect(tokenInput(registry, form).connection_ids).toEqual([])
  })
})

describe('what stops a token being created', () => {
  it('names the missing piece', () => {
    const form = emptyForm()
    expect(whyNotCreatable(form)).toBe('a token needs a name')
    form.name = 'x'
    expect(whyNotCreatable(form)).toBe('tick at least one capability')
    form.scopes = ['gmail:read']
    form.allConnections = false
    expect(whyNotCreatable(form)).toBe('pick at least one connection')
    form.connectionIds = [1]
    expect(whyNotCreatable(form)).toBeNull()
  })

  it('asks a delegate token for no connection', () => {
    const form = { ...emptyForm(), name: 'g', scopes: ['gmail:read'], delegate: true }
    form.allConnections = false
    expect(whyNotCreatable(form)).toBeNull()
  })
})

describe('copying a token into the form', () => {
  const personal: TokenDto = {
    id: 20,
    name: 'claude-code',
    scopes: ['gmail:read', 'gmail:draft', 'docs:read', 'docs:write'],
    client: 'claude-code',
    user_id: 1,
    all_connections: false,
    connection_ids: [2, 1],
    delegate: false,
    created_at: '2026-08-05T10:00:00Z',
    last_used_at: null,
    revoked_at: null,
  }

  it('copies every field of a personal token with an allowlist', () => {
    const copy = formFromToken(registry, personal, connections)
    expect(copy.form).toEqual({
      name: 'claude-code',
      client: 'claude-code',
      scopes: ['gmail:read', 'gmail:draft', 'docs:read', 'docs:write'],
      delegate: false,
      allConnections: false,
      connectionIds: [2, 1],
    })
    expect(copyNotes(copy)).toEqual([])
  })

  it('copies a token for every connection without a list', () => {
    const copy = formFromToken(registry, tokens[0]!, connections)
    expect(copy.form.allConnections).toBe(true)
    expect(copy.form.connectionIds).toEqual([])
    expect(copy.form.scopes).toEqual(['gmail:read', 'gmail:draft', 'drive:read'])
  })

  it('copies a delegate token as a delegate, and keeps delegate out of the grid', () => {
    const copy = formFromToken(registry, tokens[1]!, connections)
    expect(copy.form).toEqual({
      name: 'openwebui',
      client: 'openwebui',
      scopes: ['gmail:read', 'drive:read'],
      delegate: true,
      allConnections: true,
      connectionIds: [],
    })
    expect(copy.droppedScopes).toEqual([])
    expect(copyNotes(copy)).toEqual([])
    // What the form posts is what the token held.
    expect(tokenInput(registry, copy.form).scopes).toEqual(tokens[1]!.scopes)
  })

  it('ticks the read level along with a copied write level', () => {
    const copy = formFromToken(registry, { ...personal, scopes: ['calendar:write'] }, connections)
    expect(copy.form.scopes).toEqual(['calendar:read', 'calendar:write'])
  })

  it('drops a capability the registry no longer knows, and names it', () => {
    const old = { ...personal, scopes: ['gmail:read', 'tasks:read', 'docs:write', 'gmail:send'] }
    const copy = formFromToken(registry, old, connections)
    expect(copy.form.scopes).toEqual(['gmail:read', 'docs:read', 'docs:write'])
    expect(copy.droppedScopes).toEqual(['tasks:read', 'gmail:send'])
    expect(copyNotes(copy)).toEqual([
      'Not copied: tasks:read, gmail:send. This server no longer knows these capabilities.',
    ])
    expect(copyNotes({ ...copy, droppedScopes: ['tasks:read'] })).toEqual([
      'Not copied: tasks:read. This server no longer knows that capability.',
    ])
  })

  it('drops a connection that no longer exists, and names it', () => {
    const copy = formFromToken(registry, { ...personal, connection_ids: [1, 7] }, connections)
    expect(copy.form.allConnections).toBe(false)
    expect(copy.form.connectionIds).toEqual([1])
    expect(copy.droppedConnections).toEqual([7])
    expect(copyNotes(copy)).toEqual(['Not copied: connection #7. It no longer exists.'])
  })

  it('leaves the picker empty when every listed connection is gone', () => {
    const copy = formFromToken(registry, { ...personal, connection_ids: [7, 8] }, connections)
    expect(copy.form.connectionIds).toEqual([])
    expect(copyNotes(copy)).toEqual([
      'Not copied: connection #7, connection #8. They no longer exist.',
    ])
    // The form says what is missing rather than posting an empty allowlist.
    expect(whyNotCreatable(copy.form)).toBe('pick at least one connection')
  })

  it('falls back to the first client profile when the old one is gone', () => {
    const copy = formFromToken(registry, { ...personal, client: 'emacs' }, connections)
    expect(copy.form.client).toBe('generic')
    expect(copyNotes(copy)).toEqual([
      'Not copied: client profile emacs. This server no longer knows it, so the form uses generic.',
    ])
  })
})
