import { describe, expect, it } from 'vitest'
import { snippetsFor } from './snippets'

describe('the client snippets', () => {
  const [claude, rosa, opencode, openwebui] = snippetsFor(
    'https://google-mcp.int.krzaq.cc/',
    'gg_secret',
  )

  it('register the MCP endpoint with Claude Code in one command', () => {
    expect(claude!.body).toBe(
      'claude mcp add --transport http gmcp https://google-mcp.int.krzaq.cc/mcp ' +
        '--header "Authorization: Bearer gg_secret"',
    )
  })

  it('offer a sub rosa variant that never writes the token to disk', () => {
    expect(rosa!.body).toContain('rosa add KQ_GMCP_TOKEN --policy auto')
    expect(rosa!.body).not.toContain('gg_secret')
    const json = rosa!.body.slice(rosa!.body.indexOf("'") + 1, rosa!.body.lastIndexOf("'"))
    expect(JSON.parse(json)).toEqual({
      type: 'stdio',
      command: 'rosa',
      args: [
        'exec',
        'KQ_GMCP_TOKEN',
        '--',
        'sh',
        '-c',
        'exec npx -y mcp-remote https://google-mcp.int.krzaq.cc/mcp ' +
          '--header "Authorization: Bearer $KQ_GMCP_TOKEN"',
      ],
    })
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
