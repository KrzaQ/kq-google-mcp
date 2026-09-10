// Instants arrive from the API as RFC 3339 UTC. Everything shown is a
// wall-clock time in one zone, resolved through Intl so there is no offset
// table to keep and DST is the platform's problem. The zone is a parameter
// rather than a global so tests can pin it.

export function systemZone(): string {
  try {
    return Intl.DateTimeFormat().resolvedOptions().timeZone || 'UTC'
  } catch {
    return 'UTC'
  }
}

/**
 * What the zone input offers before the person types. A list of every IANA
 * name would be six hundred rows nobody reads; these are the ones this
 * deployment's people actually live in, and any other name can still be typed
 * by hand.
 */
export const COMMON_ZONES = [
  'Europe/Warsaw',
  'Europe/London',
  'Europe/Berlin',
  'Europe/Lisbon',
  'America/New_York',
  'America/Chicago',
  'America/Los_Angeles',
  'Asia/Tokyo',
  'Australia/Sydney',
  'UTC',
]

const formatters = new Map<string, Intl.DateTimeFormat>()
function formatter(zone: string): Intl.DateTimeFormat {
  let f = formatters.get(zone)
  if (!f) {
    f = new Intl.DateTimeFormat('en-GB', {
      timeZone: zone,
      hourCycle: 'h23',
      year: 'numeric',
      month: '2-digit',
      day: '2-digit',
      hour: '2-digit',
      minute: '2-digit',
      second: '2-digit',
    })
    formatters.set(zone, f)
  }
  return f
}

type Parts = { y: number; m: number; d: number; hh: number; mm: number; ss: number }

function partsIn(date: Date, zone: string): Parts {
  const p = formatter(zone).formatToParts(date)
  const get = (t: Intl.DateTimeFormatPartTypes) => Number(p.find((x) => x.type === t)?.value ?? 0)
  return {
    y: get('year'),
    m: get('month'),
    d: get('day'),
    hh: get('hour'),
    mm: get('minute'),
    ss: get('second'),
  }
}

const pad = (n: number) => String(n).padStart(2, '0')

/** "YYYY-MM-DD HH:MM:SS" on a clock in `zone`. */
export function formatDateTime(iso: string, zone: string = systemZone()): string {
  const p = partsIn(new Date(iso), zone)
  return `${p.y}-${pad(p.m)}-${pad(p.d)} ${pad(p.hh)}:${pad(p.mm)}:${pad(p.ss)}`
}

/** "YYYY-MM-DD HH:MM" — the log is dense enough without seconds everywhere. */
export function formatMinute(iso: string, zone: string = systemZone()): string {
  return formatDateTime(iso, zone).slice(0, 16)
}

/**
 * A date input's value (a plain day) as the instant the API filters on:
 * `from` is the start of that day, `to` the end of it, both on `zone`'s
 * clock. A day typed into a filter means the whole day.
 */
export function dayBoundary(day: string, end: boolean, zone: string = systemZone()): string {
  const [y, m, d] = day.split('-').map(Number)
  if (!y || !m || !d) return ''
  const wall = Date.UTC(y, m - 1, d, end ? 23 : 0, end ? 59 : 0, end ? 59 : 0)
  const wallOf = (t: number) => {
    const p = partsIn(new Date(t), zone)
    return Date.UTC(p.y, p.m - 1, p.d, p.hh, p.mm, p.ss)
  }
  let guess = wall
  for (let i = 0; i < 2; i++) guess += wall - wallOf(guess)
  return new Date(guess).toISOString().slice(0, 19) + 'Z'
}

const MINUTE = 60_000
const HOUR = 60 * MINUTE
const DAY = 24 * HOUR

/**
 * How long ago something happened, for the "last use" line on a connection
 * card. Past a fortnight the date itself says more than the count of days.
 */
export function formatAgo(
  iso: string | null | undefined,
  now: Date = new Date(),
  zone: string = systemZone(),
): string {
  if (!iso) return 'never'
  const delta = now.getTime() - new Date(iso).getTime()
  if (delta < 0) return formatMinute(iso, zone)
  if (delta < MINUTE) return 'just now'
  if (delta < HOUR) return `${Math.floor(delta / MINUTE)} min ago`
  if (delta < DAY) {
    const h = Math.floor(delta / HOUR)
    return `${h} hour${h === 1 ? '' : 's'} ago`
  }
  if (delta < 14 * DAY) {
    const d = Math.floor(delta / DAY)
    return `${d} day${d === 1 ? '' : 's'} ago`
  }
  return formatMinute(iso, zone).slice(0, 10)
}
