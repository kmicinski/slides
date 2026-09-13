# slides + Caddy + Authelia

`Caddyfile` here is complete. Authelia needs two files in `./authelia/`
that this example does not ship, because they are yours:

- `configuration.yml` — Authelia's main config. The parts that matter for
  slides: a `session.cookies` entry for your domain, an `access_control`
  rule that allows your users on `$SLIDES_HOST`, and the `file` authentication
  backend pointing at `users_database.yml`. Start from
  <https://www.authelia.com/configuration/prologue/introduction/> and the
  Caddy integration guide at
  <https://www.authelia.com/integration/proxies/caddy/> — the `forward_auth`
  block in the Caddyfile is the one from that guide.
- `users_database.yml` — users with argon2id hashes:
  `docker run --rm authelia/authelia:4 authelia crypto hash generate argon2 --password 'pw'`.

Then `cp .env.example .env`, fill in the three secrets and the two hostnames,
and `docker compose up -d --build`. Point DNS for both hostnames at this
machine; Caddy gets the certificates.

Any other `forward_auth`-style provider (Authentik, oauth2-proxy with
`--set-xauthrequest`, Pomerium …) drops in by replacing the `(login)` block:
the only requirement is that the user name ends up in a `Remote-User` header
on the proxied request.
