# Security model

Status: **Proposed** unless stated; nothing is implemented. Architecture context: [architecture](architecture.md). Frappe behavior referenced is labelled in the [feature matrix](feature-matrix.md). Changes to authentication, authorization or multi-site isolation are high-impact: they need Stage 1 planning and independent Stage 3 review per [AGENTS.md](../../AGENTS.md).

## Principles

- **Deny by default everywhere.** A DocType with no permission rules is inaccessible except to System Manager/admin.
- **One enforcement point.** A single permission evaluator in `forgebase-core`; no endpoint bypasses it.
- **Framework owns the dangerous parts.** Identifiers, SQL, transactions and sessions are handled by the framework, not by app code.
- **Secrets stay out of the data plane** (see Secrets).

## Authentication (milestone M3)

| Mechanism | Design |
|---|---|
| Browser sessions | Opaque random token in an `HttpOnly`, `Secure`, `SameSite=Lax` cookie. Only a hash is stored in Postgres. Expiry and revocation supported. CSRF protection for cookie-authenticated state-changing requests. |
| API tokens (machine clients) | Key + secret; the secret is stored hashed. |
| Passwords | Argon2id (`argon2` crate; Proposed, see [dependency-evaluation](dependency-evaluation.md#8-authentication)). |
| Deferred | OAuth/SSO, 2FA. |

JWT for first-party browser sessions is avoided (revocation complexity).

## Authorization

- **Phase 1 (M3):** role permissions per DocType (read, write, create, delete, submit, cancel, amend) plus an "owner only" condition.
- **Phase 2 (Later, separate decision):** field-level permission levels, user permissions (record filters by linked value), sharing.
- Frappe's phase-2 matching rules are not fully verified (feature matrix, open questions 3-4); do not copy semantics until they are read from the official pages.

## Query enforcement

- All document reads go through a permission-filtered query path in `forgebase-db`. List queries receive `WHERE` predicates; there is no "fetch then filter".
- Custom endpoints (`/api/v1/method/{name}`) must declare the required permission explicitly. This contrasts with Frappe, where whitelisted methods do not check DocType permissions automatically (Source-indexed, see feature matrix).
- Pagination has a default limit and a hard maximum.

## Injection defense

- SQL identifiers come only from validated metadata, are matched against a strict pattern and are always quoted.
- Values are always bound parameters.
- List filters are parsed into a typed AST, never string-concatenated.
- Reports (later) run with a read-only DB role.

## Multi-site isolation (when it arrives; Undecided)

Options, lean (database-per-site) and the day-one site/pool context constraint are in [architecture, section 9](architecture.md#9-multi-site-undecided). Security-relevant points: no cross-site queries; the site is resolved from the `Host` header against a configured list; a row-level tenant column is rejected because permission bugs would leak data.

## Secrets

Config file and environment variables only; never in DB metadata or logs. Session tokens and API secrets are stored hashed.

## Audit

`owner`, `modified_by`, `modified_at` from day one. Full version history is deferred.

## Deferred items

OAuth/SSO, 2FA, field-level permissions, user permissions, sharing, full audit/version history, workflow-based edit restrictions, Postgres row-level security as defense in depth (idea only, not planned).

## Open questions for M3

Not decided; each needs an answer in the M3 plan:

- Login rate limiting / brute-force protection.
- Session rotation: issue a new session token on login, and invalidate sessions on password change and logout.
- CORS policy when cookies are used (CORS middleware is planned in [ADR 0005](../adr/0005-http-stack.md)).
- `Host` header validation in single-site mode (today it appears only under multi-site).
- Argon2id parameters (see [dependency-evaluation, open question 9](dependency-evaluation.md#open-questions)).
- Permission-aware Link name resolution: the API resolves a target `name` to `id` internally, so foreign key or lookup errors must not reveal whether a document the user cannot read exists.

## Required negative tests

Per the testing strategy in [maintenance-policy](maintenance-policy.md#testing-strategy), M3 is not complete without explicit negative tests:

- every permission rule: a user without the role is denied (read, write, create, delete, submit, cancel, amend), and a DocType with no rules is denied to non-admins;
- "owner only": another user's document is neither listed nor fetchable;
- list queries never return rows the user cannot read (including via filters, sorting and paging);
- injection attempts in filters, field names, sort keys and DocType names are rejected or inert;
- expired, revoked and tampered session tokens and wrong API secrets are rejected;
- state-changing cookie-authenticated requests without a valid CSRF token are rejected;
- custom endpoints without a declared permission are refused;
- (when multi-site exists) a request for site A can never read site B data.
