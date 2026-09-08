import { describe, expect, it } from 'vitest'
import { snippetsFor } from './snippets'

describe('the client snippets', () => {
  const [claude, opencode, openwebui] = snippetsFor('https://google-mcp.int.krzaq.cc/', 'gg_secret')

  it('register the MCP endpoint with Claude Code in one command', () => {
    expect(claude!.body).toBe(
      'claude mcp add --transport http gmcp https://google-mcp.int.krzaq.cc/mcp ' +
        '--header "Authorization: Bearer gg_secret"',
    )
  })

  it('give OpenCode a remote entry it can paste into opencode.json', () => {
    expect(JSON.parse(opencode!.body)).toEqual({
      mcp: {
        gmcp: {
          type: 'remote',
          url: 'https://google-mcp.int.krzaq.cc/mcp',
          enabled: true,
          headers: { Authorization: 'Bearer gg_secret' },
        },
      },
    })
  })

  it('tell Open WebUI the acting-user header and the Native requirement', () => {
    expect(openwebui!.body).toContain('https://google-mcp.int.krzaq.cc/mcp')
    expect(openwebui!.body).toContain('Bearer')
    expect(openwebui!.body).toContain('X-Gmcp-User: {{USER_EMAIL}}')
    expect(openwebui!.note).toContain('Native')
  })
})
