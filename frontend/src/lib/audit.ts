// The log, turned into the six columns both tables draw.
//
// The API answers with ids, so the labels have to be looked up here; a row
// whose connection has since been removed still has to render, and it does,
// as the id it names.

import type { AuditDto, ConnectionDto, TokenDto } from '@/api/types'
import { formatMinute, systemZone } from '@/lib/time'

/** The audit kinds, in the order the filter offers them. */
export const AUDIT_KINDS = [
  'tool_call',
  'link_created',
  'link_used',
  'link_refused',
  'connect',
  'reconnect',
  'connection_removed',
  'token_created',
  'token_revoked',
] as const

const KIND_LABELS: Record<string, string> = {
  tool_call: 'tool call',
  link_created: 'link made',
  link_used: 'link used',
  link_refused: 'link refused',
  connect: 'connected',
  reconnect: 'reconnected',
  connection_removed: 'connection removed',
  token_created: 'token created',
  token_revoked: 'token revoked',
}

// Guarded the same way: `KIND_LABELS['constructor']` is a function, not a
// label. A kind this file does not know is still the API's own word for
// something and is shown as words.
export function kindLabel(kind: string): string {
  return Object.hasOwn(KIND_LABELS, kind) ? KIND_LABELS[kind]! : kind.replace(/_/g, ' ')
}

export type AuditRow = {
  id: number
  time: string
  kind: string
  tool: string
  connection: string
  token: string
  outcome: string
  detail: string
  /** The stripped arguments, for the row's title attribute. */
  args: string
}

export type Named = Pick<ConnectionDto, 'id' | 'label'> | Pick<TokenDto, 'id' | 'name'>

function nameOf(named: readonly Named[], id: number | null | undefined): string {
  if (id == null) return '—'
  const hit = named.find((n) => n.id === id)
  if (!hit) return `#${id}`
  return 'label' in hit ? hit.label : hit.name
}

export function auditRow(
  entry: AuditDto,
  connections: readonly Named[] = [],
  tokens: readonly Named[] = [],
  zone: string = systemZone(),
): AuditRow {
  return {
    id: entry.id,
    time: formatMinute(entry.at, zone),
    kind: kindLabel(entry.kind),
    tool: entry.tool ?? '—',
    connection: nameOf(connections, entry.connection_id),
    token: nameOf(tokens, entry.token_id),
    outcome: entry.outcome,
    detail: entry.detail ?? '',
    args: entry.args == null ? '' : JSON.stringify(entry.args),
  }
}

/** How an outcome is coloured: the log is read to find what went wrong. */
export function outcomeClass(outcome: string): string {
  if (outcome === 'ok') return 'text-ok'
  if (outcome === 'forbidden') return 'text-warn'
  return 'text-danger'
}
