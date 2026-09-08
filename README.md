# gmcp

A scoped Google portal: one Rust binary that is an HTTP API, a web UI, a CLI
and an MCP server, over SQLite. It holds the Google OAuth grants of a few
people (several Google accounts each), mints bearer tokens whose capabilities
are a matrix of service × level, and exposes curated Gmail, Drive, Docs,
Sheets and Calendar tools to Open WebUI, Claude Code and OpenCode.

**It never sends mail.** Drafts are the deliverable and the person sends them
from Gmail; nothing is trashed; no calendar change ever notifies anyone
(`sendUpdates=none`, always). Google cannot express "drafts but no send", so
this server is the policy layer — which is the reason it exists rather than
the first-party connectors.

`make` lists the targets; `make dev-backend` and `make dev-frontend` run the
two halves for development; `make test` needs no database and no network.

## Surfaces

- `gmcp serve` runs the API, serves the built Vue frontend and exposes MCP at
  `/mcp` (streamable HTTP, bearer only).
- **Connections** are Google accounts, not logins: a person has as many as they
  like, each with a label (`work`, `personal`), its own refresh token sealed at
  rest, its own set of connected services and its own health. Every tool takes
  an `account` argument naming a connection by its label.
- **Tokens** carry `service:level` scopes (`gmail:read`, `gmail:draft`,
  `gmail:modify`, `drive:read`, `docs:read|write`, `sheets:read|write`,
  `calendar:read|write`), a connection allowlist and a client profile
  (`openwebui`, `claude-code`, `opencode`, `generic`) that decides how images
  come back. `tools/list` is filtered per token, so a model never sees a tool
  it may not call. A `delegate` token belongs to nobody and names the acting
  person in `X-Gmcp-User`; it works only for someone who logged in through the
  browser in the last 30 days.
- **Files** leave through short-lived download links (15 minutes, three uses),
  never as MCP payloads; images additionally come back downscaled as image
  content, and text is extracted server-side for PDF, DOCX, CSV and plain text.
- **Every call is logged** — who, which token, which connection, which tool,
  the arguments with secrets stripped, the outcome — and the log is the
  portal's front page.
- `gmcp token`, `gmcp connection`, `gmcp user`, `gmcp prune`,
  `gmcp check-secret` and `gmcp migrate` are the admin CLI. Connecting an
  account is browser-only; `token create` prints the secret once, with the
  snippet each client needs to use it.

## Make targets

```bash
make                  # list targets
make test             # cargo test + vitest
make dev-backend      # GMCP_AUTH=dev on http://localhost:8000
make dev-frontend     # Vite with /api proxied to the dev backend
make build            # install, format, lint, frontend bundle, release binary
```
