import { describe, expect, it } from 'vitest'
import { audit, connections, tokens } from '@/api/fixtures'
import { auditRow, kindLabel, outcomeClass } from './audit'
import { dayBoundary, formatAgo, formatMinute } from './time'

const UTC = 'UTC'

describe('an audit row', () => {
  it('names the connection and the token behind the ids', () => {
    const row = auditRow(audit.entries[0]!, connections, tokens, UTC)
    expect(row).toMatchObject({
      time: '2026-09-09 07:30',
      kind: 'tool call',
      tool: 'gmail_search',
      connection: 'work',
      token: 'claude-code',
      outcome: 'ok',
      detail: '',
    })
    expect(row.args).toBe('{"account":"work","query":"from:bank"}')
  })

  it('keeps rendering when the connection is gone', () => {
    const row = auditRow(audit.entries[0]!, [], [], UTC)
    expect(row.connection).toBe('#1')
    expect(row.token).toBe('#10')
  })

  it('shows an em dash where the log has nothing', () => {
    const row = auditRow({ ...audit.entries[2]!, connection_id: null, tool: null }, [], [], UTC)
    expect(row.connection).toBe('—')
    expect(row.tool).toBe('—')
    expect(row.args).toBe('')
  })

  it('spells the kinds out', () => {
    expect(kindLabel('link_refused')).toBe('link refused')
    expect(kindLabel('connection_removed')).toBe('connection removed')
    expect(kindLabel('something_new')).toBe('something new')
  })

  it('colours by outcome, because the log is read to find failures', () => {
    expect(outcomeClass('ok')).toBe('text-ok')
    expect(outcomeClass('forbidden')).toBe('text-warn')
    expect(outcomeClass('error')).toBe('text-danger')
  })
})

describe('instants', () => {
  it('are shown to the minute on the reader’s clock', () => {
    expect(formatMinute('2026-09-09T07:30:00Z', UTC)).toBe('2026-09-09 07:30')
    expect(formatMinute('2026-09-09T07:30:00Z', 'Europe/Warsaw')).toBe('2026-09-09 09:30')
  })

  it('say how long ago on a card', () => {
    const now = new Date('2026-09-09T12:00:00Z')
    expect(formatAgo(null, now, UTC)).toBe('never')
    expect(formatAgo('2026-09-09T11:59:30Z', now, UTC)).toBe('just now')
    expect(formatAgo('2026-09-09T11:40:00Z', now, UTC)).toBe('20 min ago')
    expect(formatAgo('2026-09-09T11:00:00Z', now, UTC)).toBe('1 hour ago')
    expect(formatAgo('2026-09-08T11:00:00Z', now, UTC)).toBe('1 day ago')
    expect(formatAgo('2026-08-01T11:00:00Z', now, UTC)).toBe('2026-08-01')
  })

  it('turn a filter’s day into the whole of that day', () => {
    expect(dayBoundary('2026-09-09', false, UTC)).toBe('2026-09-09T00:00:00Z')
    expect(dayBoundary('2026-09-09', true, UTC)).toBe('2026-09-09T23:59:59Z')
    // Warsaw is two hours ahead in September, so its day starts earlier in UTC.
    expect(dayBoundary('2026-09-09', false, 'Europe/Warsaw')).toBe('2026-09-08T22:00:00Z')
  })
})
