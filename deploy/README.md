# Deploying slides

The app is one process on port 7100 with no TLS. Put a reverse proxy in front
and pick one of three ways to decide who may edit. Players are always public.

| Mode | Set | Who may edit | When |
|---|---|---|---|
| **Password** | `SLIDES_PASSWORD` | anyone with the shared password (login page; tools send it as a bearer token) | one author, or a small group that shares a secret |
| **Proxy auth** | `TRUST_PROXY_AUTH=true` | any request the proxy tags with a `Remote-User` header | you already run a login layer (basic auth, Authelia, Authentik, oauth2-proxy …) |
| **Read-only** | neither | nobody — players only | publishing decks edited elsewhere |

`/mcp` is separate: it is gated by `SLIDES_MCP_TOKEN` alone (unset ⇒ 503), so
the proxy must pass it through without its own login.

## The contract for proxy auth

With `TRUST_PROXY_AUTH=true` the app trusts one thing: a non-empty
`Remote-User` header means "logged in". So the proxy must

1. **require login** on everything except the public player surface, which is
   `/deck/<name>/` and the files in it (`index.html`, images, `export.zip`),
   `/engine/*` and `/themes/*` — but *not* `/deck/<name>/live`, `…/ws` or
   `…/thumb`, which are editing features;
2. **strip any client-supplied `Remote-User`** (and the other `Remote-*`
   headers) on every path it lets through without login, including `/mcp`;
3. set `Remote-User` to the logged-in user's name on the paths it protects.

Miss (2) and anyone can edit by sending the header themselves. Each example
below implements exactly this list; they differ only in where the login
comes from.

## Examples

Every example is a directory with a `compose.yml` that builds the app from
the repository root plus the proxy config it needs. Run one with

```
cd deploy/<example>
cp .env.example .env   # fill it in
docker compose up -d --build
```

- [`caddy/`](caddy/) — password mode behind Caddy with automatic TLS. The
  smallest real deployment.
- [`caddy-basicauth/`](caddy-basicauth/) — proxy-auth mode with Caddy's
  built-in `basic_auth`. Users and bcrypt hashes live in the Caddyfile; shows
  the header contract without any extra service.
- [`caddy-authelia/`](caddy-authelia/) — proxy-auth mode with
  [Authelia](https://www.authelia.com) via `forward_auth`: named users,
  groups, optional 2FA. The Caddyfile is the complete one; the Authelia side
  is sketched and links to Authelia's own docs.

Anything that speaks `forward_auth` / `auth_request` slots into the Authelia
example unchanged, as long as it copies the user name into `Remote-User`.

## The agent

The ✦ Ask button is optional and off unless the image has a `claude` binary
*and* `SLIDES_MCP_TOKEN` is set. Build with `CLAUDE_CLI=1` and give the
container `ANTHROPIC_API_KEY` (or mount a Claude Code OAuth credentials file
at `/home/slides/.claude/.credentials.json`). Every example's `.env.example`
has the knobs, commented out.
