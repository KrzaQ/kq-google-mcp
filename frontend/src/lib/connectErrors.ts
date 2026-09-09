// The Google callback cannot answer a browser with a JSON envelope: the
// person is looking at a page. It redirects here with `?error=<code>`
// instead, and these are the sentences those codes stand for. The list is the
// one in src/http/handlers/connections.rs; an unknown code still says
// something rather than nothing.

const MESSAGES: Record<string, string> = {
  google_unconfigured:
    'This server has no Google client configured, so nothing can be connected yet.',
  not_logged_in: 'Your session ended while Google had the browser. Log in and try again.',
  access_denied: 'You declined on the Google consent screen. Nothing was connected.',
  refused: 'Google refused the consent. Nothing was connected.',
  flow_expired: 'The connect attempt took too long. Start it again.',
  no_state: 'Google came back without the state this browser started with. Start again.',
  state_mismatch: 'Google came back with a state this browser did not start. Start again.',
  no_code: 'Google came back without an authorisation code. Start again.',
  exchange_failed: 'Google would not exchange the code. Start again; the log has the detail.',
  no_refresh_token:
    'Google issued no refresh token. Remove this app under your Google account’s ' +
    'third-party access, then connect again so the consent screen asks in full.',
  userinfo_failed: 'Google would not say which account consented. Start again.',
  no_email: 'Google reported no email address for that account.',
  no_verified_email:
    'That Google account’s address is not verified, and every label and rule here is keyed on it.',
  gone: 'That connection no longer exists.',
  different_account:
    'The consent screen signed in as a different Google account. Reconnect with the account ' +
    'the connection already names, or remove it and connect the other one.',
  not_stored: 'The grant came back but could not be stored. Try again.',
  already_connected: 'That Google account is already connected here, under another label.',
  label_taken: 'A connection with that label already exists.',
}

// A plain object inherits from Object.prototype, so `?error=constructor` would
// otherwise find a function and render it. Only a code this file wrote counts,
// and everything else gets the same sentence: the query string is the person's
// to set, so nothing out of it is repeated back onto the page.
export function connectErrorMessage(code: string): string {
  return Object.hasOwn(MESSAGES, code)
    ? MESSAGES[code]!
    : 'Connecting failed. Start again; the activity log has the detail.'
}
