# Deploying gmcp on the home server

Everything here runs on the host, not in a sandbox. The steps are in order and
each one can be checked before the next. This is the whole runbook: nothing in
it needs another document.

Names and ports used throughout:

| Thing | Value |
|---|---|
| Public name | `https://google-mcp.int.krzaq.cc` |
| Container port on the host | `127.0.0.1:13386` (support-ui has 13382/13383, koryto 13384/13385) |
| Run directory | `/storage/encrypted/gmcp` — `docker-compose.yml`, `.env`, `data/` |
| Container uid | `10001`, the image's `gmcp` user; `data/` must be owned by it |
| OIDC redirect URI | `https://google-mcp.int.krzaq.cc/api/auth/callback` |
| Google redirect URI | `https://google-mcp.int.krzaq.cc/api/google/callback` |

## 1. Google Cloud console

In the project that owns the `gmail-sender` client (`~/code/scripts/gmail-sender`).
The existing credentials there are a Desktop ("installed") client, and Google
allows only loopback and out-of-band redirects on that type, so a server-side
callback needs a new client. Everything else in the project — consent screen,
enabled APIs — carries over.

**1.1 Enable the five APIs.** APIs & Services → Library, enable each:

- Gmail API
- Google Drive API
- Google Docs API
- Google Sheets API
- Google Calendar API

**1.2 Consent screen scopes.** APIs & Services → OAuth consent screen → Data
access → Add or remove scopes. Add exactly these; they are what the service
bundles ask for:

| Service | Scopes |
|---|---|
| `gmail` | `https://www.googleapis.com/auth/gmail.modify` |
| `drive` | `https://www.googleapis.com/auth/drive.readonly`, `https://www.googleapis.com/auth/drive.file` |
| `docs` | `https://www.googleapis.com/auth/documents` |
| `sheets` | `https://www.googleapis.com/auth/spreadsheets` |
| `calendar` | `https://www.googleapis.com/auth/calendar.readonly`, `https://www.googleapis.com/auth/calendar.events` |

`openid` and `email` are always requested as well, so the callback learns which
Google account was connected; they are on the consent screen by default.

`gmail.modify` allows sending, and Google has no narrower scope that still
permits drafts — this server is the policy layer, and no tool of it ever calls
a send, trash or delete endpoint.

**1.3 Publishing status: In production, not verified.** Audience → Publish app.
Confirm the dialog; do **not** submit for verification.

Why: in *Testing* every refresh token dies after seven days and each Google
account has to be pre-listed as a test user — that is the actual cause of the
weekly re-auth in `~/misc/mailing`. Unverified *In production* shows a "Google
hasn't verified this app" screen once per account (click **Advanced** → **Go to
gmcp (unsafe)**), caps the app at 100 users for life, and issues refresh tokens
that live until six months of disuse or an explicit revocation. Verified status
would need a paid CASA audit because of the Gmail scope, and is not pursued.

**1.4 The Web application client.** APIs & Services → Credentials → Create
credentials → OAuth client ID → Application type **Web application**, name
`gmcp`. Authorised redirect URIs, both of them:

```
https://google-mcp.int.krzaq.cc/api/google/callback
http://localhost:8000/api/google/callback
```

The second is for `make dev-backend`, which serves on `http://localhost:8000`.
There are no authorised JavaScript origins; the flow is server-side.

Put the client id and secret into `/storage/encrypted/gmcp/.env` as
`GMCP_GOOGLE_CLIENT_ID` and `GMCP_GOOGLE_CLIENT_SECRET`, replacing the
`CHANGE_ME` values.

**1.5 Afterwards, re-run the mailing script once:**

```sh
ruby ~/misc/mailing/auth.rb
```

It is bound to the old Desktop client, which is untouched, but its refresh
token was dying weekly because the consent screen was in Testing. One more
consent under the now-published screen and it should stop expiring.

## 2. authentik

1. *Optional:* Directory → Groups → create `gmcp` and add whoever may log in.
   Either bind it to the application as a policy (step 3) or set
   `GMCP_OIDC_GROUP=gmcp` in `.env`. Without either, any authentik account may
   log in; everyone still sees only their own connections, tokens and log.
2. Applications → Providers → Create → **OAuth2/OpenID Provider**:
   - Authorization flow: the default explicit-consent (or implicit) flow
   - Client type: **Confidential**
   - Redirect URIs: `https://google-mcp.int.krzaq.cc/api/auth/callback` (strict)
   - Signing key: any RSA key
   - Scopes: `openid`, `email`, `profile`
3. Applications → Create: name `gmcp`, slug `gmcp`, bind the provider. To gate
   the login in authentik rather than in `.env`, add the group policy binding
   here.
4. Put the three values into `/storage/encrypted/gmcp/.env`, replacing the
   `CHANGE_ME`s:

   ```
   GMCP_OIDC_ISSUER=https://authentik.krzaq.cc/application/o/gmcp/
   GMCP_OIDC_CLIENT_ID=...
   GMCP_OIDC_CLIENT_SECRET=...
   ```

   The issuer is the application's slug URL and ends with a slash.

## 3. DNS and Apache — already done

`google-mcp.int.krzaq.cc` exists in both DNS views as a CNAME to `krzaq.cc`,
and the vhost is installed and enabled. Nothing to do; verify:

```sh
dig +short google-mcp.int.krzaq.cc
curl -sk -o /dev/null -w '%{http_code}\n' https://google-mcp.int.krzaq.cc/api/health
```

Until the container runs, that second command prints `503` — apache is up and
nothing is listening on 13386 yet. That is the expected state at this point,
and it becomes `200` in step 4.

Had it not been done, it would have been:

```sh
sudo install -m 644 packaging/httpd/google-mcp.int.krzaq.cc /etc/httpd/conf/vhosts/
sudo /root/scripts/vhost add google-mcp.int.krzaq.cc
sudo apachectl configtest && sudo systemctl reload httpd
```

The wildcard certificate for `int.krzaq.cc` already covers the name. The vhost
proxies everything to `127.0.0.1:13386`; its header comment says why it has no
CORS header, no `/api` bypass, no websocket rewrite and no special handling of
`/dl/`.

## 4. Deploy

The run directory is its own ZFS dataset, `storage/encrypted/gmcp` at
`/storage/encrypted/gmcp`, holding only `docker-compose.yml`, `.env` and
`data/`. It is not a git checkout: the image is built in the development
checkout and started there.

**Already done:** the dataset exists with an empty `data/`, and `.env` is in
place with a generated `GMCP_SECRET` and `CHANGE_ME` placeholders for the
values from steps 1 and 2. Had it not been:

```sh
sudo zfs create -o compression=zstd storage/encrypted/gmcp
sudo chown krzaq:krzaq /storage/encrypted/gmcp
mkdir /storage/encrypted/gmcp/data
install -m 600 .env.example /storage/encrypted/gmcp/.env   # then fill it in
```

Confirm no `CHANGE_ME` is left before starting:

```sh
grep -c CHANGE_ME /storage/encrypted/gmcp/.env   # want 0
```

**The container runs as uid 10001** (the image's `gmcp` user), and `data/` is a
bind mount, so the directory has to belong to that uid or SQLite cannot create
the file:

```sh
sudo chown -R 10001:10001 /storage/encrypted/gmcp/data
```

Then, from the development checkout, on the commit to ship:

```sh
cp Makefile.local.example Makefile.local     # RUN_DIR is already that path
make deploy                                  # builds gmcp:local here, copies the compose file, starts the stack there
cd /storage/encrypted/gmcp && docker compose logs -f gmcp
```

Wait for `listening on http://0.0.0.0:8000` in the log. The lines before it are
the configuration summary (no secret is ever printed), whether `pdftotext` was
found, and the migrations: on a first start sqlx applies `0001_...` and the
file appears:

```sh
ls -l /storage/encrypted/gmcp/data/        # gmcp.db, gmcp.db-wal, gmcp.db-shm, owned by 10001
curl -s https://google-mcp.int.krzaq.cc/api/health
```

Health should report the database ok, `pdftotext` present and the Google client
configured. `make image` also tags the build with the commit (`gmcp:<sha>`), so
an older image is one `docker tag gmcp:<sha> gmcp:local` plus a `docker compose
up -d` away if a deploy goes wrong.

Updating later is the same two commands: `make deploy`, then watch the log.
Migrations apply at startup.

**Anywhere else** a clone is enough: the compose file carries `build: .`, so
`docker compose up -d --build` in a checkout builds and runs the container in
place, with `.env` and `data/` next to it.

## 5. First login and connecting accounts

Open `https://google-mcp.int.krzaq.cc` and log in through authentik. The first
login creates the user row that `X-Gmcp-User` later names, with the house time
zone from `GMCP_TIMEZONE` (`Europe/Warsaw` unless `.env` says otherwise). The
home page shows that zone and lets the person change it; every time the portal
and the MCP tools show is on that clock, and a time given to a tool without an
offset is read on it.

On **Connections** → *Connect account*:

1. Label the connection `work`, tick every service (`drive` is ticked along
   with `docs` and `sheets` — search and export go through Drive), and submit.
2. Google shows **"Google hasn't verified this app"**, because the consent
   screen is published but unverified. Click **Advanced** → **Go to gmcp
   (unsafe)**. This appears once per Google account.
3. Tick the permissions on the consent screen. Google lets individual ones be
   unticked; the callback stores what was actually granted, and a connection
   whose grant is narrower than its services is shown with a **partial**
   badge. If you see one, hit **Reconnect** and leave everything ticked.
4. Back on the page the connection should be green (`ok`) with the full grant.

Then connect a second account, label `personal`, with **gmail only**. Two
connections with different service sets are what proves the reach checks: a
tool that needs `calendar` on `personal` refuses with "connection `personal`
has no `calendar` service" rather than failing at Google.

Tick **gateway** on the connections that Open WebUI may reach (step 6's
delegate token sees only those). Leave it off for anything the model has no
business in.

## 6. Tokens

Two tokens, made on the **Tokens** page or from the host. The secret is shown
once; the page prints the ready-made client snippet next to it.

```sh
cd /storage/encrypted/gmcp

# Personal token for Claude Code: acts as you, every connection you have.
docker compose exec gmcp gmcp token create claude-code \
  --scopes gmail:read,gmail:draft,drive:read,docs:read,sheets:read,calendar:read \
  --client claude-code --user you@example.com --all-connections

# Gateway token for Open WebUI: belongs to nobody, acts for whoever
# X-Gmcp-User names, and only reaches connections flagged for the gateway.
docker compose exec gmcp gmcp token create openwebui \
  --scopes gmail:read,gmail:draft,drive:read,docs:read,sheets:read,calendar:read \
  --client openwebui --delegate
```

Both print a `gg_...` secret once. Add write levels (`docs:write`,
`sheets:write`, `calendar:write`, `gmail:modify`) only when they are wanted;
the CLI refuses a write level without its read level, and `tools/list` is
filtered per token, so a model never sees a tool the token cannot call.

**Claude Code:**

```sh
claude mcp add --transport http gmcp https://google-mcp.int.krzaq.cc/mcp \
  --header "Authorization: Bearer gg_..."
```

Images come back at ≤ 1024 px / ≤ 100 KB for this profile because the base64
counts against `MAX_MCP_OUTPUT_TOKENS` (25 000 by default). If a picture is
refused for size, raise it in the environment Claude Code runs in:

```sh
export MAX_MCP_OUTPUT_TOKENS=50000
```

**Open WebUI:** register `https://google-mcp.int.krzaq.cc/mcp` as an MCP
(streamable HTTP) tool server, the same way the support server's
`X-Support-User` and koryto's `X-Koryto-User` are wired. Its form takes one
value per field, and the header box is parsed as JSON:

```
URL             https://google-mcp.int.krzaq.cc/mcp
Auth            Bearer
Bearer token    gg_...
Extra headers   {"X-Gmcp-User": "{{USER_EMAIL}}"}
```

Open WebUI expands the template once per request, so the one gateway token acts
as whoever is chatting. The token page prints these four values in the same
shape when the token is created.

The delegate token acts only for someone who logged into the portal through the
browser in the last 30 days, so removing a person in authentik ends their Open
WebUI access within that time on its own.

> **Set the model's Function Calling to Native.** Open WebUI 0.11 uploads MCP
> image content to its file store and shows it under the message, but the model
> never receives it; the embedded image resource that gmcp also returns is fed
> to the model as vision input *only* in Native function-calling mode (since
> 0.10.0). In Default mode `gmail_view_image` and `drive_view_image` will look
> like they work — the picture appears in the chat — and the model will not be
> able to see it. It is per model: Workspace → Models → the model → Advanced
> Params → **Function Calling: Native**.

A picture is visible only in the turn it was fetched; a follow-up question
about it needs the tool called again. The tool descriptions say so.

## 7. Backups

The dataset snapshot already covers `/storage/encrypted/gmcp`, including
`data/`. A snapshot of a live WAL database is crash-consistent but not a clean
backup, so add a proper one to the existing backup job:

```sh
sqlite3 /storage/encrypted/gmcp/data/gmcp.db ".backup /backups/gmcp-$(date +%F).db"
```

`.backup` is safe against a running server. The job runs as root, which is what
lets it write the WAL sidecar files under the uid-10001 directory; as another
user, take it through the container instead:

```sh
cd /storage/encrypted/gmcp && docker compose exec -T gmcp \
  sqlite3 /data/gmcp.db ".backup /data/backup.db"
```

The backup contains sealed refresh tokens, useless without `GMCP_SECRET` — so
`.env` (mode 600, on the encrypted dataset) has to be in the backup too, or a
restore connects nothing.

## 8. Operations

Everything below runs from `/storage/encrypted/gmcp`.

```sh
docker compose logs -f gmcp                    # the log; tracing on stderr, RUST_LOG in .env tunes it
docker compose ps                              # health from the container's own /api/health probe
docker compose exec gmcp gmcp token list       # tokens, their scopes and last use
docker compose exec gmcp gmcp connection list  # every connection, its services and health
docker compose exec gmcp gmcp user list        # who has logged in, when last, and in which zone
docker compose exec gmcp gmcp user set-timezone someone@example.com Europe/London
```

**Pruning.** Expired download links and old audit rows:

```sh
docker compose exec gmcp gmcp prune --audit-older-than 180d
```

Nothing runs it automatically. The audit log is the portal's front page and its
answer to "why did the model draft that", so keep a generous window.

**After rotating `GMCP_SECRET`:** every refresh token was sealed under a key
derived from the old one, so every connection is dead and has to be made again
through the browser. To see exactly which:

```sh
docker compose exec gmcp gmcp check-secret
```

It opens each stored refresh token and reports the failures. Do not rotate the
secret casually.

**Migrations** apply at startup (`GMCP_AUTO_MIGRATE=1`). With it set to `0`:

```sh
docker compose exec gmcp gmcp migrate --status
docker compose exec gmcp gmcp migrate
```

**A connection gone amber** (`needs_reauth`) means Google answered a refresh
with `invalid_grant` — the grant was revoked, or the password changed. The fix
is the **Reconnect** button on the Connections page; nothing on the host helps.

## 9. Acceptance

- [ ] From Open WebUI, search the work account for a mail with a photo
      attached, ask what is in the picture and get an answer.
- [ ] Ask for a reply draft and find it in Gmail, unsent.
- [ ] From Claude Code, download an attachment through its link and read a
      Sheet range.
- [ ] Confirm every one of those shows up on the Activity page.
- [ ] Revoke the Google grant for one account in the Google account settings
      and confirm the next tool call says to reconnect and the Home page shows
      amber.
