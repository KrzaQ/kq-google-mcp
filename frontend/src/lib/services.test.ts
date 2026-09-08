import { describe, expect, it } from 'vitest'
import { driveLocked, setService, withImplied } from './services'

describe('the connect form’s services', () => {
  it('adds drive to docs and to sheets', () => {
    expect(withImplied(['docs'])).toEqual(['drive', 'docs'])
    expect(withImplied(['sheets'])).toEqual(['drive', 'sheets'])
    expect(withImplied(['gmail'])).toEqual(['gmail'])
  })

  it('locks drive while docs or sheets is ticked', () => {
    expect(driveLocked(['gmail', 'drive'])).toBe(false)
    expect(driveLocked(['docs'])).toBe(true)
    expect(driveLocked(['sheets'])).toBe(true)
  })

  it('ticks drive along and refuses to let it go', () => {
    const withDocs = setService(['gmail'], 'docs', true)
    expect(withDocs).toEqual(['gmail', 'drive', 'docs'])
    expect(setService(withDocs, 'drive', false)).toEqual(withDocs)
  })

  it('lets drive go once nothing implies it', () => {
    const noDocs = setService(['gmail', 'drive', 'docs'], 'docs', false)
    expect(noDocs).toEqual(['gmail', 'drive'])
    expect(setService(noDocs, 'drive', false)).toEqual(['gmail'])
  })

  it('keeps the registry order', () => {
    expect(setService(['calendar', 'gmail'], 'sheets', true)).toEqual([
      'gmail',
      'drive',
      'sheets',
      'calendar',
    ])
  })
})
