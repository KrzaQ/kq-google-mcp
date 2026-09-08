// Fixture responses, typed against the generated schema so a change to the
// API breaks the tests here rather than in a browser. Nothing in the app
// imports this file, so it never reaches the bundle; the tests do.

import type { AuditPage, ConnectionDto, Me, ScopeRegistry, TokenDto, UserDto } from '@/api/types'

export const user: UserDto = {
  id: 1,
  email: 'alice@example.com',
  name: 'Alice',
  created_at: '2026-01-04T09:00:00Z',
  last_login_at: '2026-09-09T07:00:00Z',
}

export const me: Me = {
  kind: 'session',
  user,
  connections: 2,
  tokens: 2,
  scopes: [],
  google_configured: true,
}

export const connections: ConnectionDto[] = [
  {
    id: 1,
    label: 'work',
    google_email: 'alice@work.example',
    services: ['gmail', 'drive', 'docs'],
    granted_scopes: [
      'https://www.googleapis.com/auth/gmail.modify',
      'https://www.googleapis.com/auth/drive.readonly',
      'https://www.googleapis.com/auth/drive.file',
      'https://www.googleapis.com/auth/documents',
    ],
    partial: false,
    status: 'ok',
    status_detail: null,
    delegate_ok: true,
    created_at: '2026-08-01T10:00:00Z',
    last_used_at: '2026-09-09T07:30:00Z',
  },
  {
    id: 2,
    label: 'personal',
    google_email: 'alice@gmail.example',
    services: ['gmail', 'calendar'],
    granted_scopes: ['https://www.googleapis.com/auth/gmail.modify'],
    partial: true,
    status: 'needs_reauth',
    status_detail: 'invalid_grant: Token has been expired or revoked.',
    delegate_ok: false,
    created_at: '2026-08-02T10:00:00Z',
    last_used_at: null,
  },
]

export const tokens: TokenDto[] = [
  {
    id: 10,
    name: 'claude-code',
    scopes: ['gmail:read', 'gmail:draft', 'drive:read'],
    client: 'claude-code',
    user_id: 1,
    all_connections: true,
    connection_ids: [],
    delegate: false,
    created_at: '2026-08-05T10:00:00Z',
    last_used_at: '2026-09-09T07:25:00Z',
    revoked_at: null,
  },
  {
    id: 11,
    name: 'openwebui',
    scopes: ['gmail:read', 'drive:read', 'delegate'],
    client: 'openwebui',
    user_id: null,
    all_connections: false,
    connection_ids: [],
    delegate: true,
    created_at: '2026-08-06T10:00:00Z',
    last_used_at: null,
    revoked_at: null,
  },
]

export const registry: ScopeRegistry = {
  services: [
    {
      service: 'gmail',
      google_scopes: ['https://www.googleapis.com/auth/gmail.modify'],
      levels: [
        { scope: 'gmail:read', level: 'read', requires: null, tools: ['gmail_search'] },
        {
          scope: 'gmail:draft',
          level: 'draft',
          requires: 'gmail:read',
          tools: ['gmail_create_draft'],
        },
        {
          scope: 'gmail:modify',
          level: 'modify',
          requires: 'gmail:read',
          tools: ['gmail_modify_labels'],
        },
      ],
    },
    {
      service: 'drive',
      google_scopes: ['https://www.googleapis.com/auth/drive.readonly'],
      levels: [{ scope: 'drive:read', level: 'read', requires: null, tools: ['drive_search'] }],
    },
    {
      service: 'docs',
      google_scopes: ['https://www.googleapis.com/auth/documents'],
      levels: [
        { scope: 'docs:read', level: 'read', requires: null, tools: ['docs_read'] },
        { scope: 'docs:write', level: 'write', requires: 'docs:read', tools: ['docs_append'] },
      ],
    },
    {
      service: 'sheets',
      google_scopes: ['https://www.googleapis.com/auth/spreadsheets'],
      levels: [
        { scope: 'sheets:read', level: 'read', requires: null, tools: ['sheets_read_range'] },
        {
          scope: 'sheets:write',
          level: 'write',
          requires: 'sheets:read',
          tools: ['sheets_append_rows'],
        },
      ],
    },
    {
      service: 'calendar',
      google_scopes: ['https://www.googleapis.com/auth/calendar.events'],
      levels: [
        { scope: 'calendar:read', level: 'read', requires: null, tools: ['calendar_list'] },
        {
          scope: 'calendar:write',
          level: 'write',
          requires: 'calendar:read',
          tools: ['calendar_create_event'],
        },
      ],
    },
  ],
  delegate: { scope: 'delegate', level: null, requires: null, tools: [] },
  clients: ['generic', 'openwebui', 'claude-code', 'opencode'],
}

export const audit: AuditPage = {
  entries: [
    {
      id: 300,
      at: '2026-09-09T07:30:00Z',
      kind: 'tool_call',
      user_id: 1,
      token_id: 10,
      connection_id: 1,
      tool: 'gmail_search',
      args: { account: 'work', query: 'from:bank' },
      outcome: 'ok',
      detail: null,
      duration_ms: 412,
      ip: null,
    },
    {
      id: 299,
      at: '2026-09-09T07:20:00Z',
      kind: 'link_created',
      user_id: 1,
      token_id: 10,
      connection_id: 1,
      tool: 'gmail_attachment_link',
      args: { account: 'work' },
      outcome: 'ok',
      detail: 'invoice.pdf',
      duration_ms: null,
      ip: null,
    },
    {
      id: 298,
      at: '2026-09-08T18:00:00Z',
      kind: 'tool_call',
      user_id: 1,
      token_id: 11,
      connection_id: 2,
      tool: 'calendar_list_events',
      args: null,
      outcome: 'forbidden',
      detail: 'connection "personal" has no calendar service',
      duration_ms: 3,
      ip: '10.0.0.2',
    },
  ],
  next: '2026-09-08T18:00:00Z,298',
}
