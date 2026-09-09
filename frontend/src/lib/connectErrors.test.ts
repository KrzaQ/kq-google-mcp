import { describe, expect, it } from 'vitest'
import { connectErrorMessage } from './connectErrors'

describe('a callback error code', () => {
  it('is spelled out when it is one of ours', () => {
    expect(connectErrorMessage('different_account')).toContain('different Google account')
    expect(connectErrorMessage('label_taken')).toContain('label already exists')
  })

  it('never reaches Object.prototype, and never comes back on the page', () => {
    // The code is whatever is in the query string. A plain-object lookup would
    // find these on the prototype and render a function.
    for (const code of ['constructor', '__proto__', 'toString', 'hasOwnProperty']) {
      expect(connectErrorMessage(code)).toBe(
        'Connecting failed. Start again; the activity log has the detail.',
      )
    }
    // An unknown code is not repeated back either.
    expect(connectErrorMessage('<img onerror=x>')).not.toContain('img')
  })
})
