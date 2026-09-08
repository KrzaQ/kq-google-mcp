// The connect form's service checkboxes.
//
// Docs and Sheets are read through Drive — search and export both go there —
// so the server adds `drive` to whatever is asked for. The form ticks it
// along and locks it rather than letting a person untick something that comes
// back ticked anyway.

/** The services whose tools cannot work without Drive. */
export const NEED_DRIVE = ['docs', 'sheets'] as const

/** Registry order, so the form always reads the same way. */
export const SERVICE_ORDER = ['gmail', 'drive', 'docs', 'sheets', 'calendar'] as const

export function inServiceOrder(services: Iterable<string>): string[] {
  const want = new Set(services)
  const known = SERVICE_ORDER.filter((s) => want.has(s)) as string[]
  const rest = [...want].filter((s) => !(SERVICE_ORDER as readonly string[]).includes(s)).sort()
  return [...known, ...rest]
}

/** True while Drive is implied and so cannot be unticked. */
export function driveLocked(services: readonly string[]): boolean {
  return NEED_DRIVE.some((s) => services.includes(s))
}

/** What the server will store, given what is ticked. */
export function withImplied(services: readonly string[]): string[] {
  const out = new Set(services)
  if (driveLocked(services)) out.add('drive')
  return inServiceOrder(out)
}

/** Tick or untick one checkbox; a locked Drive stays where it is. */
export function setService(services: readonly string[], service: string, on: boolean): string[] {
  const next = new Set(services)
  if (on) next.add(service)
  else next.delete(service)
  if (service === 'drive' && !on && driveLocked([...next])) next.add('drive')
  return withImplied([...next])
}
