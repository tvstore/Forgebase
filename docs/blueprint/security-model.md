# Security model

Status: **Proposed** unless stated; of the sections below identifier and value handling in `forgebase-db` (M1.2, M1.3, M2.1, M2.2), the trust note for the command-line tool and the storage side of authentication (M3.1, [ADR 0010](../adr/0010-authentication-and-sessions.md), Accepted) are implemented. Cookies and HTTP handling (M3.3), authorization (M3.2) and multi-site isolation are not. Architecture context: [architecture](architecture.md). Frappe behavior referenced is labelled in the [feature matrix](feature-matrix.md). Changes to authentication, authorization or multi-site isolation are high-impact: they need Stage 1 planning and independent Stage 3 review per [AGENTS.md](../../AGENTS.md).

## Principles

- **Deny by default everywhere.** A DocType with no permission rules is inaccessible except to System Manager/admin.
- **One enforcement point.** A single permission evaluator in `forgebase-core`; no endpoint bypasses it.
- **Framework owns the dangerous parts.** Identifiers, SQL, transactions and sessions are handled by the framework, not by app code.
- **Secrets stay out of the data plane** (see Secrets).

## Authentication (milestone M3; storage side implemented in M3.1, [ADR 0010](../adr/0010-authentication-and-sessions.md))

| Mechanism | Design |
|---|---|
| Browser sessions | Opaque random token (32 bytes from the operating system, 64 hex characters). **Implemented (M3.1):** only the SHA-256 digest is stored in `fb_session`, with a CSRF token digest, the user, an absolute expiry (7 days) and an idle expiry (24 hours since last use); every login creates a new session; logout, password change and disabling the user delete sessions. **M3.3:** carried in an `HttpOnly`, `Secure`, `SameSite=Lax` cookie; the CSRF check for cookie-authenticated state-changing requests (`check_csrf` exists since M3.1). |
| API tokens (machine clients) | **Implemented (M3.1):** public key (32 hex) and secret (64 hex); only the secret's SHA-256 digest is stored; the secret is shown once; tokens do not expire and can be revoked; a disabled user's tokens stop working. |
| Passwords | **Implemented (M3.1):** Argon2id (`argon2` 0.6) with m = 19456 KiB, t = 2, p = 1, random 16-byte salt, PHC string; verified with the parameters inside the string; 8 to 1024 bytes; hashing runs on the blocking thread pool. |
| Login lockout | **Implemented (M3.1):** per submitted username (also unknown ones), 5 failures lock it for 15 minutes; counting is committed even when the login fails; concurrent attempts are serialized by a row lock. Unknown, disabled, wrong-password and locked all give one uniform error and spend one Argon2 verification (a dummy hash when there is no real one). |
| Deferred | OAuth/SSO, 2FA, password reset by email. |

JWT for first-party browser sessions is avoided (revocation complexity).

**Secrets rule.** No password, session token, CSRF token or API secret is stored in plaintext, logged, or part of an error, `Debug` or `Display` output. The only exceptions are the one-time return values of `login` and `create_api_token` and the one-time output of `forgebase token create`. Types that carry a secret implement `Debug` by hand and redact it; tests assert this.

**Known limits (M3.1).** The lockout can be used to lock a known user out (denial of service); `fb_login_failure` has one small row per failed username that is not cleaned up automatically; a caller that runs `login` inside its own transaction loses the failure counting if that transaction rolls back (call it on a pooled client); digests are compared by PostgreSQL, not in constant time (acceptable for 256-bit random values); there is no password complexity or breach check; Argon2 memory use (about 19 MiB per verification) needs concurrency limits in the HTTP layer.

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
- Values are always bound parameters. Generated DDL (schema sync) contains no values at all: only validated, quoted identifiers and numeric type parameters (`varchar(n)`, `numeric(p,s)`) taken from validated metadata; settings such as `lock_timeout` are passed as bound parameters.
- Identifiers are validated twice: by `forgebase-core` when metadata is loaded, and quoted by `forgebase_db::quote_ident` (doubling any `"`) at every use in generated SQL (implemented in M1.2).
- Document values and the actor are bound parameters, with an explicit cast per field type; a document write never interpolates a value into SQL. Document names are bound too (implemented in M1.3, tests store injection-like strings literally).
- Link fields (M2.1): the target `name` is a bound parameter of the lookup (`SELECT "id" FROM "doc_<target>" WHERE "name" = $1::text FOR KEY SHARE`) and the resolved `id` is bound as `uuid`; the target table name comes from validated metadata (`Link { target }` is checked as a DocType name when the definition is loaded) and is quoted. Generated DDL for Link fields (foreign keys, indexes) contains identifiers only. Tests store injection-like names literally. The foreign key (`ON DELETE RESTRICT`) is the integrity guarantee; the `FOR KEY SHARE` lock narrows the race between the lookup and the write. Conflicting versions (`expected_version`) are checked by the locking read that starts each `update` and `delete` (since M2.3), and again in the `WHERE` clause of the write.
- Child tables (M2.2): rows are written with one `INSERT` per row whose values are bound parameters with explicit casts (`$n::bigint`, ...), like the document itself; the child table name and column names come from validated metadata and are quoted; Link names inside rows are resolved by the same bound lookup. The parent id and the row position are bound too. The DDL for a child table (`parent_id` foreign key, indexes) contains identifiers only. Row values from the command-line tool go through the same validation as document values. Calling `forgebase-db` with a child DocType as the document type is refused (`DbError::ChildDocType`), so rows cannot be addressed without their parent; M3 must apply the parent's permissions to the rows.
- Naming series and docstatus (M2.4): the series prefix (rendered from the pattern and the database date) and the number are bound parameters of the `fb_series` statement; the pattern is validated when the DocType is loaded and the table name `fb_series` is fixed, so no document value reaches SQL text. The `docstatus` value written by submit and cancel is a constant of the `DocStatus` enum, not an input. Users cannot set `docstatus` or `amended_from`: they are not fields, so any value for them in a document or in changes is rejected as an unknown field, and both are reserved field names. A submitted document can only change `allow_on_submit` fields, also when a hook tries to change another (`DbError::FieldLocked`). Submit, cancel and amend need permissions of their own once M3 arrives (the Submit, Cancel and Amend rights of the permission model); until then they are as unauthenticated as every other operation.
- Database connections do not use TLS yet (M1.2); connect over a local or private network until rustls support is added.
- List filters are parsed into a typed AST, never string-concatenated.
- Reports (later) run with a read-only DB role.

## Multi-site isolation (when it arrives; Undecided)

Options, lean (database-per-site) and the day-one site/pool context constraint are in [architecture, section 9](architecture.md#9-multi-site-undecided). Security-relevant points: no cross-site queries; the site is resolved from the `Host` header against a configured list; a row-level tenant column is rejected because permission bugs would leak data.

## Secrets

Config file and environment variables only; never in DB metadata or logs. Session tokens and API secrets are stored hashed.

## The command-line tool (M1.3)

`forgebase` is a **trusted local tool**: whoever can run it with a database URL has full access to that database, including creating users, setting passwords and creating API tokens (`forgebase user ...` and `forgebase token ...`, M3.1; passwords are read from standard input, never from an argument or environment variable, and `token create` is the only output of a secret). It has no authentication and no permission checks (those arrive with M3 in the HTTP layer), and `--actor` is only recorded in `owner` / `modified_by` on insert and update (on delete it is given to hooks and not stored); it is not verified and must not be treated as an identity. The database URL (with its password) is read from the `FORGEBASE_DATABASE_URL` environment variable, never from a command-line argument, and is not printed. Input is checked before any SQL runs (JSON mapping, then core validation), and a datetime whose UTC instant is outside the supported range is an error, not a panic.

## Audit

`owner`, `modified_by`, `modified_at` from day one. Full version history is deferred.

## Deferred items

OAuth/SSO, 2FA, field-level permissions, user permissions, sharing, full audit/version history, workflow-based edit restrictions, Postgres row-level security as defense in depth (idea only, not planned).

## Open questions for M3

Not decided; each needs an answer in the M3 plan:

- ~~Login rate limiting / brute-force protection.~~ Answered in M3.1 (ADR 0010): per-username lockout (5 failures, 15 minutes). Throttling by client address and bounded login concurrency belong to M3.3.
- ~~Session rotation: issue a new session token on login, and invalidate sessions on password change and logout.~~ Answered in M3.1 (ADR 0010): a new session on every login; password change and disabling delete all of the user's sessions; logout deletes the session.
- CORS policy when cookies are used (CORS middleware is planned in [ADR 0005](../adr/0005-http-stack.md)).
- `Host` header validation in single-site mode (today it appears only under multi-site).
- ~~Argon2id parameters~~ Answered in M3.1 (ADR 0010): m = 19456 KiB, t = 2, p = 1 (see [dependency-evaluation, open question 9](dependency-evaluation.md#open-questions)).
- Permission-aware Link name resolution: the API resolves a target `name` to `id` internally, so foreign key or lookup errors must not reveal whether a document the user cannot read exists. M2.1 resolves names without any permission check (the command-line tool is trusted); its `LinkNotFound` and `LinkedDocumentExists` errors confirm that a document does or does not exist, and the referencing DocType is named. M3 must decide what an unprivileged caller may learn from them.
- Link lookups take `FOR KEY SHARE` on the target row, which PostgreSQL allows only to a role with `UPDATE` privilege on the target table. A read-only database role (planned for M3) therefore cannot write documents that have Link fields unless it also gets that privilege or the lookup changes.

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
