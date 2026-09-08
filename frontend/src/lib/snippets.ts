// The three clients, each with the one line or block a person pastes.
//
// The secret is shown once, so this is the moment the registration has to be
// complete: the URL, the bearer header, and for the gateway the header that
// names the acting person. Everything else about a client belongs in its own
// documentation, not here.

export type Snippet = { id: string; title: string; language: string; body: string; note?: string }

/** The server this portal is served from, which is the one the clients call. */
export function publicOrigin(): string {
  return typeof window !== 'undefined' ? window.location.origin : ''
}

export function mcpUrl(origin: string): string {
  return `${origin.replace(/\/+$/, '')}/mcp`
}

export function claudeCodeSnippet(origin: string, secret: string): Snippet {
  return {
    id: 'claude-code',
    title: 'Claude Code',
    language: 'sh',
    body: `claude mcp add --transport http gmcp ${mcpUrl(origin)} --header "Authorization: Bearer ${secret}"`,
    note: 'Large images count against MAX_MCP_OUTPUT_TOKENS; raise it if a picture comes back cut.',
  }
}

export function openCodeSnippet(origin: string, secret: string): Snippet {
  const config = {
    mcp: {
      gmcp: {
        type: 'remote',
        url: mcpUrl(origin),
        enabled: true,
        headers: { Authorization: `Bearer ${secret}` },
      },
    },
  }
  return {
    id: 'opencode',
    title: 'OpenCode',
    language: 'json',
    body: JSON.stringify(config, null, 2),
    note: 'Merge into opencode.json, in the project or in ~/.config/opencode.',
  }
}

export function openWebUiSnippet(origin: string, secret: string): Snippet {
  return {
    id: 'openwebui',
    title: 'Open WebUI',
    language: 'text',
    body: [
      `URL          ${mcpUrl(origin)}`,
      `Auth         Bearer`,
      `Bearer token ${secret}`,
      `Extra header X-Gmcp-User: {{USER_EMAIL}}`,
    ].join('\n'),
    note: "Set the model's Function Calling to Native, or it never sees the images.",
  }
}

export function snippetsFor(origin: string, secret: string): Snippet[] {
  return [
    claudeCodeSnippet(origin, secret),
    openCodeSnippet(origin, secret),
    openWebUiSnippet(origin, secret),
  ]
}
