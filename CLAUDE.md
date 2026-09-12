# CLAUDE.md

Guidance for Claude Code when working in this repository.

## What this is

`gmcp` holds the Google OAuth grants of a few people, mints scoped bearer
tokens and exposes curated Gmail, Drive, Docs, Sheets and Calendar tools to
Open WebUI, Claude Code and OpenCode. One Rust binary provides an HTTP API,
serves the Vue web UI, offers a CLI and exposes MCP at `/mcp`. Data lives in
one SQLite file. `../koryto` and `../support-ui` are the sibling projects this
one copies its bones from: layout, auth, Makefile, test harness and frontend
stack are theirs, adapted; do not invent a second way of doing something they
already do.

`PLAN.md` is the design and the build sequence. It is a local working document,
listed in `.gitignore`, and **must never be committed**; what the repository has
to say about itself is here and in `README.md`.

## Layout

- `src/main.rs` — clap entry point: `serve | migrate | token | connection | user | prune | check-secret`
- `src/config.rs` — `GMCP_*` environment parsing, dev-auth guard
- `src/db/` — `Db` (SQLite via sqlx, runtime queries) and row types
- `src/domain/` — scope registry, token helpers, refresh-token sealing, links, image policy
- `src/google/` — one `Client` (reqwest) with configurable bases, the OAuth flow, and a module per service; MIME building and text extraction
- `src/http/` — axum router, auth (OIDC session cookie + bearer tokens), the Google callback, the `/dl/{id}` route, OpenAPI
- `src/mcp/` — rmcp server, the tool registry and the per-profile result shaping
- `src/cli/` — the terminal commands
- `migrations/` — sqlx migrations, applied by `gmcp migrate` and at `serve` startup
- `frontend/` — Vue 3 + Vite + TypeScript + Pinia + Tailwind 4; embedded into the binary from `frontend/dist`
- `tests/fixtures/google/` — recorded Google responses for the wiremock tests
- `packaging/` — apache vhost
- `docs/deploy.md` — the host-side runbook

## Rules that matter

- **Nothing is ever sent.** No tool calls `users.messages.send` or
  `users.drafts.send`, ever; a draft is the deliverable and the person sends it
  from Gmail. Nothing is trashed or deleted either (`TRASH` and `SPAM` are
  refused, `messages.trash|delete` is never called); the one exception is
  `gmail_delete_draft`, which is the undo for a draft. Every calendar mutation
  passes `sendUpdates=none`, always, and this release refuses events with
  attendees outright so a draft agenda cannot turn into an invitation.
- The Google grant is broader than what the tools do — `gmail.modify` allows
  sending and Google has no narrower scope. This server is the policy layer.
  That is the whole reason it exists rather than the first-party connectors.
- **Token scopes are `service:level` strings** validated against the registry in
  `domain/scope.rs`: `gmail:read|draft|modify`, `drive:read`,
  `docs:read|write`, `sheets:read|write`, `calendar:read|write`, plus
  `delegate`, the only non-matrix scope. Adding a service is a code change,
  never a migration. Levels are not cumulative in storage; the UI ticks lower
  levels along and the CLI refuses a write level without its read level.
  `domain::token::validate` is the one place those rules live: `POST
  /api/tokens` and `gmcp token create` both call it, so the portal and the
  terminal refuse the same request in the same words.
- `tools/list` is filtered per token: a model that cannot see a tool never
  tries it. `call_tool` re-checks and answers a clean error anyway.
- A token also carries a **client profile**, and images are shaped for it:
  `claude-code` gets `Text` + `Image` at ≤ 1024 px / ≤ 100 KB (the base64
  counts against `MAX_MCP_OUTPUT_TOKENS`), `opencode` and `generic` the same at
  ≤ 1568 px / ≤ 200 KB, `openwebui` additionally an `EmbeddedResource` with an
  `image/*` blob, which is the only thing its model actually sees. Never
  `structuredContent` on the image tools, and never the original bytes.
- **Every id interpolated into a Google URL goes through
  `google::client::urlencode`.** Ids are model-supplied; a `/`, `..`, `?` or
  `#` in one of them would move the call to an endpoint the tools never make.
  `join` refuses a path that does not come out as it went in.
- **Connections, not one login.** A person has any number, each one Google
  account with a label; every tool takes `account` and resolves it against the
  principal's visible connections. Refresh tokens are sealed with AES-256-GCM
  under a key derived from `GMCP_SECRET`; rotating that secret invalidates
  every connection.
- A session sees its own connections; a personal token sees its allowlist (or
  all of the user's); a delegate token sees the acting user's connections
  flagged `delegate_ok`, and only for someone who logged in through the browser
  in the last 30 days.
- Writes to Docs, Sheets and Calendar need `confirmed=true`; Gmail drafts do
  not, because a draft is itself the confirmation step.
- Files leave through short-lived download links (15 min, 3 uses), never as MCP
  payloads. The constants live in `domain/` and are constants, not
  configuration.
- **A file gets into a draft over the wire and no other way.** `gmail_upload_link`
  mints a one-time ticket, the agent POSTs the bytes to `/up/{id}` itself, and
  the upload id goes to a draft tool, which attaches the file and deletes it.
  Nothing reads a path a caller supplies: the caller is on another machine.
  Tickets and staged files are in memory and in `GMCP_UPLOAD_DIR`, never in the
  database, and `serve` empties that directory at startup — nothing survives a
  restart that the database does not know about. A draft with no attachments
  produces byte-identical MIME to what it did before attachments existed and
  still goes to the JSON endpoint; one with files goes to Gmail's upload
  endpoint, which is still a drafts endpoint and still sends nothing.
- Every tool call, link mint and link hit is logged, with the arguments
  stripped of bodies and cut at 4 KB. Tool output is never logged.
- **Tests must not need the network, a file on disk or an existing database.**
  `Db::open_memory()` migrates a fresh in-memory SQLite per test; every Google
  call goes through `google::Client`, whose bases point at wiremock in tests
  with fixtures from `tests/fixtures/google/`.
- No CORS headers; the frontend is same-origin. `/api/*` never redirects on
  401. `/api` is session-only; bearer tokens are for `/mcp` and nothing else.
- After changing an API shape, regenerate `frontend/src/api/schema.d.ts`
  (`make types` with `make dev-backend` running) and commit it.
- `GMCP_AUTH=dev` logs everyone in as one fixed user and is refused unless
  both `GMCP_PUBLIC_URL` and `GMCP_BIND` are loopback.

## Commands

```bash
make                  # list targets
make test             # cargo test + vitest, no database and no network
make dev-backend      # GMCP_AUTH=dev on http://localhost:8000
make dev-frontend     # Vite with /api proxied to the dev backend
make build            # install, format, lint, frontend bundle, release binary
```

Commits: imperative subject with a module prefix (`server:`, `frontend:`,
`mcp:`, `google:`, `docker:`), body explains why, one concern per commit, files
staged by name, never any AI attribution trailers.
