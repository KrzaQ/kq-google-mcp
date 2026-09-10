// The clients, each with the one line, block or set of fields a person pastes.
//
// The secret is shown once, so this is the moment the registration has to be
// complete: the URL, the bearer header, and for the gateway the header that
// names the acting person. Everything else about a client belongs in its own
// documentation, not here.
//
// A snippet is either a `body`, one blob that goes somewhere whole, or
// `fields`, when the client asks for the same values in separate inputs and a
// single select-all block would be the wrong shape to copy from.

export type SnippetField = { label: string; value: string }
export type Snippet = {
  id: string
  title: string
  language: string
  body?: string
  fields?: SnippetField[]
  note?: string
}

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

/**
 * The name sub rosa knows the secret by. Named after the service rather than
 * the token, as `KQ_SUPPORT_TOKEN` is, so one machine has one name for it
 * however many tokens get minted over the years.
 */
export const ROSA_SECRET = 'KQ_GMCP_TOKEN'

/**
 * The same registration, with the token held by sub rosa instead of written
 * into `~/.claude.json`. `rosa exec` puts the secret in the environment of the
 * bridge process and nowhere else.
 */
export function subRosaSnippet(origin: string): Snippet {
  const entry = {
    type: 'stdio',
    command: 'rosa',
    args: [
      'exec',
      ROSA_SECRET,
      '--',
      'sh',
      '-c',
      `exec npx -y mcp-remote ${mcpUrl(origin)} --header "Authorization: Bearer $${ROSA_SECRET}"`,
    ],
  }
  return {
    id: 'claude-code-rosa',
    title: 'Claude Code, token held by sub rosa',
    language: 'sh',
    body: [
      `rosa add ${ROSA_SECRET} --policy auto   # paste the token above when it asks`,
      '',
      `claude mcp add-json --scope user gmcp '${JSON.stringify(entry)}'`,
    ].join('\n'),
    note: 'Keeps the token out of the config file. Needs a running rosa serve.',
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
    fields: [
      { label: 'URL', value: mcpUrl(origin) },
      { label: 'Auth', value: 'Bearer' },
      { label: 'Bearer token', value: secret },
      // Open WebUI parses this box as JSON, and expands the template once per
      // request, so the gateway token acts as whoever is chatting.
      { label: 'Extra headers', value: '{"X-Gmcp-User": "{{USER_EMAIL}}"}' },
    ],
    note: "Set the model's Function Calling to Native, or it never sees the images.",
  }
}

export function snippetsFor(origin: string, secret: string): Snippet[] {
  return [
    claudeCodeSnippet(origin, secret),
    subRosaSnippet(origin),
    openCodeSnippet(origin, secret),
    openWebUiSnippet(origin, secret),
  ]
}
