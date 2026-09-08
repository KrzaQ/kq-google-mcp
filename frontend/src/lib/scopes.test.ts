import { describe, expect, it } from 'vitest'
import { registry } from '@/api/fixtures'
import {
  emptyForm,
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
