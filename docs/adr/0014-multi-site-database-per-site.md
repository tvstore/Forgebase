# 0014 Multi-site: database per site

- Status: Accepted (owner decisions M7-1 to M7-7 of the M7 plan, 2026-10-30, "setuju semua, lanjutkan"); the implementation details of M7.1 are separated below and are not owner decisions. Replaces the "lean" of [architecture, section 9](../blueprint/architecture.md#9-multi-site-accepted-adr-0014); extends [ADR 0010](0010-authentication-and-sessions.md) (sessions and tokens live in the site's own database) without changing it.
- Date: 2026-10-30

## Context

One deployment of Forgebase should be able to serve several customers (sites), each with its own data and users. The risk to design against is a **cross-site leak**: one request, command or job that runs with the wrong site's data. [Architecture, section 9](../blueprint/architecture.md#9-multi-site-accepted-adr-0014) had a day-one constraint, "every request or command carries an explicit site context; there is no global database handle", and a lean towards a database per site. M7 is split into M7.1 (the config file and the site context everywhere, this ADR), M7.2 (per-site app install, `site list`, `site create`, the worker per site with its apps) and M7.3 (backup and restore, the operations guide, a CI example).

## Decision

The decisions below are the owner's (M7 plan, 2026-10-30).

- **M7-1. One database per site.** Architecture section 9 moves from "lean" to Accepted.
- **M7-2. A config file, `forgebase.toml`, holds the sites.** Database URLs come from environment variables, never from the file. Without a config file, Forgebase behaves exactly as before.
- **M7-3. The site is chosen by the `Host` header.** An unknown host is refused.
- **M7-4. The apps of a site are a subset of the binary's apps** (M7.2). Removing an app keeps its data.
- **M7-5. One worker process per site** in M7.
- **M7-6. Backup and restore use `pg_dump` and `pg_restore`** (M7.3).
- **M7-7. The work is split into M7.1, M7.2 and M7.3.**

## Alternatives considered

- **Schema per site in one database.** Cheaper in connections, but one `search_path` mistake or one missing schema qualifier reads another site's tables; the roles, sequences and extensions are still shared; a per-site backup needs `pg_dump --schema`. Rejected: the isolation is weaker for no gain that matters at the scale of M7.
- **A tenant column on every row.** The weakest isolation: every query, every hook and every future feature must remember the filter, and one permission bug leaks data. Rejected (as in architecture section 9 before).
- **A shared worker for all sites.** One process polling every site's database. Possible later; M7 runs one worker process per site (M7-5), which keeps a site's jobs, pool and failures separate and needs no new code path.

## Consequences

- **More pools and connections.** Each site has its own pool; a deployment with N sites can hold N times the connections. The config has an optional per-site `pool_size`, so a small site can have a small pool. PostgreSQL's `max_connections` must cover the sum (the operations guide of M7.3 will say so).
- **`CREATE DATABASE` is not transactional** (architecture section 5): creating a site is a separate step from migrating it (M7.2).
- **Backup and restore are per site**, with the standard PostgreSQL tools (M7.3).
- **A missed site context is the main risk.** The design answer is one explicit `Site` type, a site context passed down (no global pool, no global URL), and isolation tests at every layer.

## Implementation details (M7.1)

These choices were made while implementing M7.1 and are reviewed with that change; they are not owner decisions.

**The config file.** `forgebase.toml` is found only through `--config PATH` or the environment variable `FORGEBASE_CONFIG` (the option wins). **There is no discovery in the current directory**, so a stray file never changes behaviour. Format:

```toml
[[sites]]
name = "acme"                                  # [a-z][a-z0-9_]*, at most 63 bytes
hosts = ["acme.example.com", "acme.localhost"] # host names without a port, lower-cased on load
database_url_env = "ACME_DATABASE_URL"         # the environment variable that holds the URL
# pool_size = 16                               # optional, 1 to 256 (default: the pool default)
```

Unknown keys are an error (`deny_unknown_fields`; `apps` is refused until M7.2 adds it). Validation errors are collected and reported together with exit code 1: no sites, an invalid or duplicate site name, a site with no hosts or an invalid host (the rules of `FORGEBASE_ALLOWED_HOSTS`: no port, IPv6 in brackets), a host listed by two sites, an invalid variable name, a `pool_size` out of range. A site's variable that is unset or empty is an error **when that site is used**; the message names the site and the variable, and **the value is never printed**. With a config file, `FORGEBASE_ALLOWED_HOSTS` is ignored, and `serve` warns once if it is set.

**The `Site` type and the site context.** `Site` (in `forgebase`, `config.rs`) holds the name, the hosts, the name of the URL variable and the pool size. `Site::context` reads the URL, opens the site's pool and returns a `SiteContext { site, pool, registry }`; **that is the only way a command gets a database connection**. The registry is one `Arc<Registry>` shared by all sites in M7.1; M7.2 builds one per site, changing only how the context is built. There is no process-global pool or URL: only the builder of the implicit site names `FORGEBASE_DATABASE_URL` (a unit test checks the sources). `forgebase-http` has no notion of a configuration: it is given one `AppState` (pool, registry, login limit) per site.

**The implicit site.** Without a config file there is one site, `default`: its URL is `FORGEBASE_DATABASE_URL`, its hosts are `FORGEBASE_ALLOWED_HOSTS` (default `localhost,127.0.0.1`). `--site default` is accepted; any other name is a usage error. Every existing command, test and document stays valid.

**`--site`.** A global option. With a config file of several sites it is required on every database command except `serve` (exit 2, with the site names); with one site it is optional; `new-app` ignores it. `serve` without `--site` serves every site (one pool per site, routing by `Host`); with `--site` it serves that site only. `worker` runs for one site. `migrate` migrates one site: **there is no "all sites" mode** (a possible later convenience). A worker's pool is sized by its concurrency (`concurrency + 2`), not by the site's `pool_size`.

**Host to site, and the status codes.** The `Host` is still checked first, on every route and every error. A missing, repeated or malformed `Host` is `400 invalid_host` (unchanged). Then:

- **With a config file** a well-formed host that no site owns is `404 not_found` (`"no such site"`, the generic envelope): the host names no site, and the 404 does not say whether the name is a typo, a site that is not served here or no site at all. The match ignores case and port.
- **Without a config file** (the implicit site) a host outside the allow-list stays `400 invalid_host`, so the protection against DNS rebinding and every existing test are unchanged.

`X-Forwarded-Host` and `Forwarded` are never read: a reverse proxy must pass the original `Host`. The dispatcher is the outermost layer: it picks the site's router (each site has a complete router with its own state) and adds the common response headers. Desk serving, security headers, CSRF, the session cookie and the body limit are the same for all sites.

**Cookies and sessions.** The session cookie has no `Domain` attribute, so a browser sends it only to the host that set it. Sessions, API tokens and the login-failure counters live in the site's own database, so a cookie or token of site A cannot be validated by site B even if a request were routed to it by mistake (tested). The login semaphore and the pool are per site: `max_concurrent_logins` applies per site.

**Tests.** Config parsing and validation (units); the CLI with two real databases (`--site` required, optional, unknown, `migrate`, `doc`, `user`, `token` and `job enqueue` touch one database); HTTP isolation with one router over two databases (passwords, sessions, tokens, documents, meta, unknown and malformed hosts, forwarding headers); `serve` end to end with a config file; `worker --site a` claims only from database A.
