# Dependency evaluation

Condensed from research done on 2026-10-28; sources are given inline. **Versions, licenses and MSRVs are as of 2026-10-28**; re-run `cargo info` at decision time. Recommendations are **Proposed** unless marked Accepted (the `time` and `rust_decimal` choices below were Accepted by the owner on 2026-10-28): the axum, tokio and tokio-postgres + deadpool-postgres (with the custom migration runner) directions are Accepted by the owner (ADRs 0004 and 0005, 2026-10-28; ADR 0004 confirmed by the M1 spike, see below), as are React + TypeScript for the desk (ADR 0002) and TLS termination at a reverse proxy (ADR 0005); everything not marked Accepted is not decided. Admission rules: [maintenance-policy](maintenance-policy.md). Related decisions: [ADR 0004](../adr/0004-database-access-layer.md), [ADR 0005](../adr/0005-http-stack.md).

Selection criteria, in priority order: long-term maintenance cost, simplicity, small dependency count, then fit with runtime-metadata DocTypes (tables and columns created from metadata at runtime; documents cross the boundary as validated maps/JSON). Popularity is not a criterion.

## Method, evidence labels, limitations

| Label | Meaning |
|---|---|
| `Verified (cargo info)` | Read from crates.io metadata with `cargo info` (cargo 1.97.0): version, license field, `rust-version`, features. |
| `Verified (local measurement)` | Measured in throwaway crates with `cargo add` + `cargo tree -e normal` (unique packages, root included, host target) and `cargo metadata` (licenses of all resolved packages, all targets). Nothing was compiled. |
| `Source-indexed` | Backed by a web-search summary of the page at the URL given inline next to the claim. Summaries are second-hand; confirm on the live page. |
| `Unverified` | General knowledge; confirm before relying on it. |

Limitations:

- crates.io, docs.rs, GitHub and rustsec.org were not directly reachable. Release dates, activity and advisories come from search summaries or are Unverified.
- `cargo info` reports the license *field*, not license files; crates with bundled code (ring, aws-lc-sys) may carry extra terms.
- Package counts are a rough proxy for weight (not compile time, `unsafe` usage or C build needs) and depend on the features chosen.

## Summary table

All rows `Verified (cargo info)`. "n/d" = no `rust-version` declared.

| Crate | Version | License | MSRV |
|---|---|---|---|
| axum | 0.8.9 | MIT | 1.80 |
| actix-web | 4.18.1 | MIT OR Apache-2.0 | 1.89 |
| poem | 3.1.12 | MIT OR Apache-2.0 | 1.85 |
| rocket | 0.5.2 | MIT OR Apache-2.0 | 1.89 |
| tower | 0.5.3 | MIT | 1.64 |
| tower-http | 0.7.1 | MIT | 1.65 |
| tokio | 1.54.1 | MIT | 1.85 |
| clap | 4.6.7 | MIT OR Apache-2.0 | 1.85 |
| argh | 0.1.19 | BSD-3-Clause | n/d |
| lexopt | 0.3.2 | MIT | n/d |
| sqlx | 0.9.0 | MIT OR Apache-2.0 | 1.94.0 |
| sea-orm | 2.0.4 | MIT OR Apache-2.0 | 1.94.0 |
| sea-query | 1.0.2 | MIT OR Apache-2.0 | 1.88.0 |
| diesel | 2.3.14 | MIT OR Apache-2.0 | 1.86.0 |
| diesel-async | 0.9.2 | MIT OR Apache-2.0 | 1.84.0 |
| tokio-postgres | 0.7.18 | MIT OR Apache-2.0 | 1.85 |
| postgres-types | 0.2.14 | MIT OR Apache-2.0 | 1.85 |
| deadpool-postgres | 0.14.2 | MIT OR Apache-2.0 | 1.85 |
| refinery | 0.10.0 | MIT | 1.92 |
| serde | 1.0.229 | MIT OR Apache-2.0 | 1.56 |
| serde_json | 1.0.154 | MIT OR Apache-2.0 | 1.71 |
| validator | 0.21.0 | MIT | 1.88 |
| garde | 0.23.0 | MIT OR Apache-2.0 | 1.87 |
| thiserror | 2.0.21 | MIT OR Apache-2.0 | 1.77 |
| anyhow | 1.0.104 | MIT OR Apache-2.0 | 1.68 |
| tracing | 0.1.44 | MIT | 1.65.0 |
| tracing-subscriber | 0.3.23 | MIT | 1.65.0 |
| argon2 | 0.6.0 | MIT OR Apache-2.0 | 1.85 |
| password-hash | 0.6.1 | MIT OR Apache-2.0 | 1.85 |
| tower-sessions | 0.15.0 | MIT | n/d |
| tower-sessions-sqlx-store | 0.15.0 | MIT | n/d |
| axum-login | 0.18.0 | MIT | n/d |
| apalis | 1.0.0 | MIT OR Apache-2.0 | n/d |
| apalis-postgres | 1.0.0 | **unknown (no license field)** | n/d |
| figment | 0.10.19 | MIT OR Apache-2.0 | n/d |
| config (config-rs) | 0.15.27 | MIT OR Apache-2.0 | 1.88.0 |
| toml | 1.1.8+spec-1.1.0 | MIT OR Apache-2.0 | 1.85 |
| cargo-deny | 0.20.2 | MIT OR Apache-2.0 | 1.88.0 |
| cargo-audit | 0.22.2 | Apache-2.0 OR MIT | 1.88 |
| rustls | 0.23.46 (0.24.0-dev.2 is pre-release) | Apache-2.0 OR ISC OR MIT | 1.71 |
| tokio-postgres-rustls | 0.14.0 | MIT | n/d |
| postgres-native-tls | 0.5.3 | MIT OR Apache-2.0 | n/d |
| native-tls | 0.2.18 | MIT OR Apache-2.0 | 1.80 |
| ring | 0.17.14 | Apache-2.0 AND ISC | 1.66.0 |
| aws-lc-rs | 1.18.2 | ISC AND (Apache-2.0 OR ISC) | 1.71.0 |
| uuid | 1.29.0 | Apache-2.0 OR MIT | **1.89.0** |
| chrono | 0.4.45 | MIT OR Apache-2.0 | 1.62.0 |
| time | 0.3.55 | MIT OR Apache-2.0 | 1.88.0 |
| jiff | 0.2.38 | Unlicense OR MIT | 1.70 |
| rust_decimal | 1.43.0 | MIT | 1.67.1 |

Frontend (not `cargo info`): React MIT (Unverified); TypeScript Apache-2.0 (Source-indexed, https://licenses.dev/npm/typescript/2.4.1); Vite MIT, its published artifact also bundles Apache-2.0, BSD-2-Clause, CC0-1.0, ISC and MIT code (Source-indexed, https://arches.lcc.ufmg.br/static/vitest/node_modules/vite/LICENSE.md). Both frontend sources are non-authoritative (the TypeScript page is for the old 2.4.1 version; the Vite page is a third-party mirror): re-verify against the official repositories before relying on them.

### Transitive weight (`Verified (local measurement)`, unique packages)

| Option (features) | Packages |
|---|---|
| axum (default) + tokio (rt-multi-thread, macros, net) | 48 |
| actix-web (default) | 139 |
| axum + tower-sessions | 82 |
| sqlx (no defaults; postgres, runtime-tokio, tls-rustls, migrate, json, uuid, chrono) | 132 |
| tokio-postgres (serde_json, uuid, chrono) + deadpool-postgres | 74 |
| diesel-async (postgres, deadpool) | 78 |
| sea-orm (sqlx-postgres, runtime-tokio-rustls, macros) | 152 |
| sea-query (no defaults; backend-postgres) | 2 |
| refinery (tokio-postgres) | 110 |
| apalis + apalis-postgres (defaults) | 155 |
| clap (derive) / argh / lexopt | 18 / 12 / 2 |
| figment (toml, env) / config (default) / toml + serde(derive) | 23 / 51 / 14 |
| validator (derive) / garde (derive) | 53 / 15 |
| argon2 (default) | 17 |
| tracing + tracing-subscriber (env-filter, json) | 29 |
| **Candidate stack A** (axum, tokio, tokio-postgres, deadpool-postgres, serde, serde_json, thiserror, anyhow, tracing, tracing-subscriber, clap, argon2, toml, uuid, chrono) | **136** |
| **Candidate stack B** (stack A with sqlx instead of tokio-postgres + deadpool; no TLS) | **179** |

## Per-category comparison

### 1. HTTP server

| | axum | actix-web |
|---|---|---|
| Version / MSRV | 0.8.9 / 1.80 | 4.18.1 / 1.89 |
| Weight | 48 | 139 |
| Versioning | 0.x; 0.6 to 0.8 had breaking steps (expect a migration every 1-2 years) | Stable 4.x line (date Unverified) |
| Middleware | tower / tower-http ecosystem (same tokio-rs org) | Own model, separate from tower (Unverified) |

poem (3.1.12) and rocket (0.5.2) offer nothing Forgebase needs that axum lacks.

**Recommendation (Accepted, [ADR 0005](../adr/0005-http-stack.md)): axum**, plus tower-http only as needed.
Trade-off: 0.x breaking upgrades; mitigate by keeping axum types inside `forgebase-http`. Alternative: actix-web if a 1.0+-style stability promise outweighs about 90 extra packages.

#### Admission notes for M3.3 (2026-10-29)

Approved by the owner with the M3 plan ([ADR 0005](../adr/0005-http-stack.md)); licenses checked with `cargo deny --locked check` (licenses ok, no advisory); every new package has an MSRV at or below 1.85 (`hyper-util` 1.85 is the highest; project MSRV 1.97; `cargo +1.97 check --workspace --all-targets --locked` passes).

| Dependency | Where | Need | Alternatives considered | Maintenance, weight |
|---|---|---|---|---|
| `axum` 0.8.9 (MIT, MSRV 1.80), `default-features = false`, features `http1`, `json`, `query`, `tokio` | `forgebase-http` | Routing, extractors, `DefaultBodyLimit`, `axum::serve` with graceful shutdown. `http2`, `form`, `matched-path`, `original-uri`, `tracing`, `macros`, `multipart`, `ws` and `tower-log` are off. | actix-web, poem, rocket (see above). | tokio-rs organisation; 0.x (expect a breaking step every 1-2 years; axum types stay inside the crate). |
| `tower` 0.5.3 (MIT, MSRV 1.64), feature `util` only | `forgebase-http` **dev-dependency** | `ServiceExt::oneshot` to call the router in tests without a socket. | `axum::serve` on a port in every test: slower, port handling. | Already in the tree as an axum dependency (default-off features there). `deadpool-postgres` (a workspace dependency of `forgebase-db`) is also a `forgebase-http` dev-dependency, for a pool that never connects in one unit test; it adds no package. |
| `tokio` features `net`, `sync` | `forgebase-http` | `TcpListener` (`serve`), `Semaphore` (login bound). | none. | Already in the tree. |
| `tokio` features `net`, `signal`, `sync`, `time` | `forgebase` (named `forgebase-cli` until M5.1) | Bind the listener, Ctrl-C and SIGTERM, the stop channels and the grace period for `serve`. `signal` adds `signal-hook-registry` and `errno`. | none. | Already in the tree. |

New packages in `Cargo.lock` (22): `axum` 0.8.9, `axum-core` 0.5.6, `tower` 0.5.3, `tower-layer` 0.3.3, `tower-service` 0.3.3, `http` 1.5.0, `http-body` 1.1.0, `http-body-util` 0.1.5, `hyper` 1.12.0 (MIT, MSRV 1.63), `hyper-util` 0.1.21 (MIT, MSRV 1.85), `httparse` 1.10.1, `httpdate` 1.0.3, `matchit` 0.8.4 (MIT AND BSD-3-Clause), `mime` 0.3.17, `form_urlencoded` 1.2.2, `serde_urlencoded` 0.7.1, `serde_path_to_error` 0.1.20, `sync_wrapper` 1.0.2 (Apache-2.0), `atomic-waker` 1.1.2, `ryu` 1.0.23 (Apache-2.0 OR BSL-1.0), `signal-hook-registry` 1.4.8 and `errno` 0.3.14 (the last two from tokio's `signal`). All licenses are in the `deny.toml` allow list. `forgebase-http` has 144 packages in its normal dependency tree (includes `forgebase-db` and its dependencies). **No `tower-http` and no `cookie` crate**: the body limit is axum's own, CORS is deliberately absent (owner decision 8), and the session cookie is one formatted string and a small parser (the token is lowercase hex, so no escaping is needed; resolves open question 12). `http-body-util` is not a dependency of the crate: tests read bodies with `axum::body::to_bytes`.

### 2. Async runtime

tokio 1.54.1 (MIT, MSRV 1.85). axum, tokio-postgres, deadpool, sqlx and tower-sessions all target tokio; async-std is deprecated (Source-indexed, https://github.com/launchbadge/sqlx/discussions/4271: sqlx 0.9 added smol / async-global-executor support as successors to it); smol/glommio would force their own stacks.
**Recommendation (Accepted, [ADR 0005](../adr/0005-http-stack.md): tokio with explicit features, not `full`; the exact feature list `rt-multi-thread, macros, net, signal, time` is Proposed):** tokio.
Trade-off: the whole stack is tied to the tokio ecosystem and its release cadence; there is no realistic alternative among the chosen libraries, so the cost is accepted rather than mitigated.

### 3. CLI

| Crate | Weight | Notes |
|---|---|---|
| clap (derive) | 18 | Subcommands, help, env fallback; stable 4.x. |
| argh | 12 | BSD-3-Clause; 0.1.x for years (Unverified). |
| lexopt | 2 | Minimal; every subcommand and help text is hand-written. |

**Recommendation (Accepted, owner decision 2026-10-28): clap (derive).** Hand-written parsing would cost more than 17 maintained transitive crates for a many-subcommand CLI. Alternative: lexopt if the CLI stays under about 5 commands.

### 4. Database access

DocType tables exist only at runtime, so compile-time-schema approaches (diesel `table!`, sea-orm entities, sqlx `query!`) fit only Forgebase's own fixed tables. Document CRUD must build SQL from metadata with quoted identifiers and bound parameters.

| Concern | sqlx (no macros) | tokio-postgres + deadpool | diesel(-async) | sea-orm |
|---|---|---|---|---|
| Dynamic DDL / columns | Raw SQL: OK | Raw SQL: OK | Off main path | Off main path |
| Rows as JSON maps | Dynamic row API | Natural (column metadata + `FromSql`) | Awkward | Awkward |
| Compile-time checks | Optional; need DB or `.sqlx` cache (recurring CI cost) | None | Schema file kept in sync | Entities kept in sync |
| Built-in migrations | `sqlx::migrate!` | No | diesel_migrations | sea-orm-migration |
| MSRV | 1.94 | 1.85 | 1.86 / 1.84 | 1.94 |
| Weight | 132 | 74 | 78 | 152 |

Other facts:

- sqlx 0.9.0 (2026-05-06 release notes) states it has not been maintained by LaunchBadge for a few years and is moving to the `transact-rs` GitHub org (Source-indexed, https://github.com/launchbadge/sqlx/discussions/4271): watch the governance change. RUSTSEC-2024-0363 (Postgres binary protocol truncating casts) was fixed in 0.8.1 (Source-indexed, https://rustsec.org/advisories/RUSTSEC-2024-0363).
- tokio-postgres development is concentrated in one maintainer (Unverified bus-factor assessment).
- sea-query 1.0.2 as a query/DDL builder only (backend-postgres) adds 1 package; optional later behind an internal trait.

**Recommendation (Accepted direction, [ADR 0004](../adr/0004-database-access-layer.md)): tokio-postgres + deadpool-postgres + small in-house SQL/DDL layer** (strict identifier pattern such as `^[a-z][a-z0-9_]{0,58}$` for the DocType part (the 63-byte PostgreSQL limit applies to the full name including the 4-byte `doc_` prefix, so at most 59 bytes remain; derived index and constraint names such as `doc_<name>_parent_id_idx` can exceed 63 bytes and need their own bounded naming scheme, decided in M1) and always double-quoted; values only as bound parameters; Postgres-to-field-type mapping).
Close alternative: sqlx with `default-features = false`, features `postgres, runtime-tokio, migrate, json`, macros off: better if built-in migrations and pooling in one crate are worth about 40-60 extra packages, MSRV 1.94 and the governance transition. Not recommended: diesel, sea-orm. The M1 spike (the required checkpoint for [ADR 0004](../adr/0004-database-access-layer.md)) confirmed this direction; see the spike result below.

#### M1 spike result (ADR 0004 checkpoint)

Evidence from a throwaway spike that is not kept in the repository (single run on PostgreSQL 16.15, 4 cores). Both stacks implemented the same scenario end to end: create a table from metadata, insert/get/update/delete with a dynamic `Value` enum, and an injection string stored literally.

| Metric | A: tokio-postgres 0.7.18 + deadpool-postgres 0.14.2 | B: sqlx 0.9.0 (no macros, no TLS) |
|---|---|---|
| Unique packages (`cargo tree -e normal`, incl. spike crate) | 93 | 154 |
| Clean release build | ~30 s | ~54 s |
| Clean debug build | ~19 s | ~33 s |
| Spike LOC (excl. metadata) | 169 | 167 |
| Builds on MSRV 1.97 | yes | yes |
| cargo-deny licenses | ok | ok |

- Dynamic parameters: A implements `ToSql for Value` once and passes `&[&(dyn ToSql + Sync)]`; NULL needs no type. B needs a by-value bind chain, a typed `None` per field type, and `sqlx::AssertSqlSafe` around every runtime query.
- Transactions: A `let mut c = pool.get().await?; let tx = c.transaction().await?; ... tx.commit().await?` (needs a `mut` pooled client; dropping `tx` without commit rolls back). B `pool.begin().await?` then `.execute(&mut *tx)` and `tx.commit()`. Both are acceptable.
- Decoding and type mappings (numeric to `rust_decimal`, date to `time::Date`, timestamptz to `OffsetDateTime`) worked in both. A's error `Display` is only "db error" (the server message is in `as_db_error()`); B includes the server message.

**Decision: the ADR 0004 direction is confirmed; no superseding ADR.** B's advantages (error text, built-in pool and migrations) do not outweigh +61 packages, about 1.8x build time, per-query `AssertSqlSafe`, typed-NULL binding, MSRV 1.94 and the governance transition.

Lessons carried into M1:

- Map Int to PostgreSQL `bigint` (decode as `i64`); an `int` column decodes as `i32`.
- Generated SQL uses explicit parameter casts (`$n::bigint`, `$n::numeric`, ...).
- Wrap database errors in a Forgebase error type that carries the server message and SQLSTATE code.
- `impl ToSql for Value` needs the `bytes` crate (`BytesMut` in the trait signature).

Limitations: one timing run; runtime performance, statement caching and pool contention were not measured.

#### Decimal and date/time types (Accepted, owner decision 2026-10-28)

- **Decimal:** `rust_decimal` (1.43.0, MIT), mapped to PostgreSQL `numeric`. In `forgebase-core` it is used without database features. Precision is limited to 28 digits, which bounds the Decimal field `precision`.
- **Date/time:** `time` (0.3.55, MIT OR Apache-2.0, MSRV 1.88) with only the features needed. `Date` maps to `date`, `OffsetDateTime` to `timestamptz`. chrono and jiff were not chosen.

#### Admission notes for M1.3 (2026-10-28)

Added in the M1.3 PR; licenses were checked with `cargo deny` (all MIT or Apache-2.0).

| Dependency | Where | Need | Alternatives considered | Maintenance, weight |
|---|---|---|---|---|
| `uuid` 1.29 (`std`, `v7`), plus tokio-postgres `with-uuid-1` | `forgebase-db` | UUID v7 `id` generated in the application (owner decision 2026-10-28) and the `uuid` column type. MSRV 1.89 (below the project MSRV 1.97). | Hand-written UUID v7: more code to own, no gain. | Widely used, rust-random organisation. Adds `uuid` to `Cargo.lock`; its random source (`getrandom`) was already in the tree. |
| `bytes` 1 | `forgebase-db` | The signature of `ToSql::to_sql` (the `SqlValue` newtype) names `BytesMut`. Already in the tree through tokio-postgres. | None (the trait requires it). | Already a transitive dependency; no new package. |
| `serde_json` 1.0.154 (`std`) | `forgebase` (named `forgebase-cli` until M5.1) | Parsing `--data` and printing documents as JSON. The evaluation above already recommended serde + serde_json. | Hand-written JSON: not reasonable. | Very widely used, maintained by the serde authors. Already in `Cargo.lock`; now also a direct dependency of the CLI. |
| `clap` 4.6.7 (`std`, `derive`, `help`, `usage`, `error-context`) | `forgebase` (named `forgebase-cli` until M5.1) | Subcommands, help and usage errors (exit code 2). Accepted by the owner in the M1 plan (clap derive). | `lexopt` (hand-written help and subcommands), `argh`; see section 3. | Stable 4.x. Default features such as colour and suggestions are off. Adds `clap`, `clap_builder`, `clap_derive`, `clap_lex`, `anstyle` and `heck` to `Cargo.lock`. |
| `tokio` (CLI) | `forgebase` (named `forgebase-cli` until M5.1) | `#[tokio::main]` for the async database calls; features `rt-multi-thread`, `macros` only. | A current-thread runtime would need fewer features but the owner plan names these. | Already used by `forgebase-db` tests and tokio-postgres. |

Feature changes without new packages: `tokio-postgres` `with-time-0_3` (`time` values as `date` and `timestamptz`), `rust_decimal` `db-tokio-postgres` (in `forgebase-db` only; `forgebase-core` still has no database features), and `time` `parsing` and `formatting` in the CLI manifest only (RFC 3339 and `YYYY-MM-DD`). Packages new to `Cargo.lock` in this PR: `anstyle`, `clap`, `clap_builder`, `clap_derive`, `clap_lex`, `heck`, `uuid` and `time-macros` (a lock entry only, not built). `cargo tree -p forgebase-cli -e normal` (the crate is now `forgebase`) listed 98 packages (94 external, 4 workspace crates).

### 5. Migrations

Two problems: (1) framework-internal ordered SQL migrations; (2) DocType schema sync, a declarative diff of metadata vs `pg_catalog`, which no migration tool solves.

| Option | Notes |
|---|---|
| sqlx migrate | Only if sqlx is chosen. |
| refinery 0.10.0 | MSRV 1.92; 110 packages; only 0.8.x from 2024 seen in search, so 0.10 history unconfirmed. |
| Custom runner | `forgebase_migrations(id, checksum, applied_at)`, `pg_advisory_xact_lock`, embedded SQL via `include_str!`, checksum verification; about 100-200 lines (Unverified estimate). |

**Recommendation (custom runner Accepted with [ADR 0004](../adr/0004-database-access-layer.md), confirmed by the M1 spike, see the spike result above; the rest Proposed):** custom runner if tokio-postgres; `sqlx::migrate!` if sqlx. Schema sync is always framework code, transactional, destructive changes explicit. Avoid refinery.

### 6. Serialization and validation

- serde 1.0.229 and serde_json 1.0.154: no real alternative. Decide `preserve_order` and `arbitrary_precision` features early; they affect the whole workspace (semantics Unverified).
- validator (53 packages) and garde (15) validate static structs; DocType rules are runtime metadata, so neither helps for documents.

**Recommendation (Proposed):** serde + serde_json, one in-house runtime validator returning structured per-field errors; no validation crate for now.

### 7. Errors and logging

**Recommendation (Proposed):** thiserror in libraries, anyhow only in binaries; tracing + tracing-subscriber (`fmt`, `env-filter`; 29 packages); defer OpenTelemetry.

Alternatives (not evaluated in the research; all Unverified, from general knowledge): `snafu` or hand-written error enums instead of thiserror; `eyre`/`color-eyre` instead of anyhow; `log` + `env_logger` instead of tracing. Trade-off: tracing adds weight (29 packages, measured) and an extra concept (spans), but fits the tokio/axum ecosystem and structured logs; `log` + `env_logger` would likely be lighter (weight not measured) with no spans. Revisit only if tracing's weight or complexity proves a problem.

### 8. Authentication

- argon2 0.6.0 (RustCrypto, pure Rust, 17 packages). Argon2id as OWASP default and absence of an audit are Unverified; 0.5 to 0.6 implies API churn.
- tower-sessions 0.15.0: +34 packages over axum; its Postgres store (`tower-sessions-sqlx-store`) pulls sqlx; 0.x with frequent breaking releases and a past session-ID cycling fix (Source-indexed, https://github.com/maxcountryman/tower-sessions/releases); single maintainer (Unverified). axum-login adds more abstraction than needed.
- Custom opaque sessions: 256-bit random token, SHA-256 hash stored with expiry/user/CSRF secret, `HttpOnly; Secure; SameSite=Lax` cookie; about 200-300 lines (Unverified estimate). `sha2` was not evaluated. JWT is avoided for first-party sessions (revocation complexity).

**Recommendation (Proposed):** argon2 (Argon2id) + custom DB-backed opaque session tokens and hashed API tokens. Alternative: tower-sessions with a self-written store.

#### Admission notes for M3.1 (2026-10-29)

Approved by the owner with the M3 plan; recorded in [ADR 0010](../adr/0010-authentication-and-sessions.md). Licenses checked with `cargo deny --locked check` (licenses ok); MSRV from `cargo metadata`, all at or below 1.85 (project MSRV 1.97; `cargo +1.97 check` passes).

| Dependency | Where | Need | Alternatives considered | Maintenance, weight |
|---|---|---|---|---|
| `argon2` 0.6.0 (MIT OR Apache-2.0, MSRV 1.85), features `alloc` and `password-hash` only | `forgebase-db` | Argon2id password hashing and verification of PHC strings. | bcrypt (72-byte input limit, not memory-hard), scrypt, hand-written: no. Argon2id is the default recommendation (Unverified, see below). | RustCrypto, pure Rust. New packages in `Cargo.lock`: `argon2`, `password-hash` 0.6.1, `phc` 0.6.1, `base64ct` 1.8.3, `blake2` 0.11.0 (all MIT or Apache-2.0, MSRV 1.85); `cpufeatures` was already there. The `getrandom` and `parallel` features are off: the salt comes from our own `getrandom` call and hashing is single-lane. `password-hash` comes only through `argon2`'s re-exports, not as a direct dependency. |
| `getrandom` 0.4.3 (MIT OR Apache-2.0, MSRV 1.85), default features | `forgebase-db` | 32-byte tokens and the 16-byte salt from the operating system's random source (`getrandom::fill`). | `uuid`'s internal use (not an API), `rand` (more weight, not needed). | rust-random organisation. Version 0.4.3 was already in the tree (`cargo tree -i getrandom@0.4.3 -e normal` shows `rand` 0.10, which `postgres-protocol` and so tokio-postgres use, and `uuid`); it is now also a direct dependency. No new package. |
| `tokio` (normal dependency of `forgebase-db`; feature `rt` only; the workspace entry has no default features, `macros` and `rt-multi-thread` are enabled in `forgebase` (formerly `forgebase-cli`) and in the dev-dependencies of `forgebase-db`) | `forgebase-db` | `tokio::task::spawn_blocking` so Argon2 does not block the async executor. | A dedicated thread pool: more code. | Already in the tree (tokio-postgres, and a dev-dependency of this crate before). |

Hashing of tokens needs no crate: SHA-256 is computed by PostgreSQL (`sha256(convert_to($1::text, 'UTF8'))`), as for migration checksums. Parameters: m = 19456 KiB, t = 2, p = 1; they match the OWASP Password Storage Cheat Sheet's minimum Argon2id configuration as far as known (Unverified: the cheat sheet was not re-read for this change). The crate's API change between 0.5 and 0.6 does not matter here: no 0.5 code existed. No independent audit of the crate was checked (Unverified).

### 9. Background jobs

- apalis 1.0.0 is very new (still at 1.0.0-rc.4 in Feb 2026; Source-indexed, https://lib.rs/crates/apalis), apalis-postgres has **no license field**, 155 packages, and is built on sqlx.
- Redis queues add a second stateful service, against the PostgreSQL-only direction.
- Custom queue: atomic claim with `FOR UPDATE SKIP LOCKED`; optional `LISTEN/NOTIFY` wake-up; enqueue in the same transaction as the document write gives a transactional outbox.

**Recommendation (Proposed):** custom Postgres `SKIP LOCKED` queue (few hundred lines, no new dependencies) with retries, backoff and a lease timeout.

### 10. Configuration

| Option | Weight | Notes |
|---|---|---|
| figment 0.10.19 | 23 | Good layering; upstream activity unconfirmed. |
| config 0.15.27 | 51 | Many formats; heavier. |
| toml + serde | 14 | Env overrides written by hand. |

**Recommendation (Proposed):** toml + serde with a small overlay: defaults, then `forgebase.toml`, then explicitly listed `FORGEBASE_*` env vars; secrets only from env or a file path. Alternative: figment.

### 11. Supply-chain tooling and MSRV

- cargo-deny (licenses, advisories, bans, sources) in CI; cargo-audit mostly redundant. Both are CI tools, not workspace dependencies. Details: [maintenance-policy](maintenance-policy.md).
- Measured floors: stack A **1.89** (set by `uuid` 1.29.0); stack B **1.94** (set by sqlx).

### 12. Frontend

React + TypeScript for `desk/` is Accepted ([ADR 0002](../adr/0002-modular-monolith-and-workspace-layout.md)). Alternatives, listed briefly and not evaluated (Unverified): Vue, Svelte or Solid as the UI framework; server-rendered HTML (for example with htmx) instead of a SPA.

#### Owner decisions for M4 (2026-10-29, Accepted)

- **No UI component library, no router library, no data-fetching library.** Native inputs and a small own stylesheet; an own hash router (`desk/src/router.ts`); plain `fetch` (`desk/src/api.ts`). Reason: every runtime dependency is a long-term maintenance cost; the desk needs a login, a list and (M4.2) a form. Revisit only if a real need appears (an ADR then).
- **Toolchain: Vite + TypeScript + Vitest**, with ESLint (`@eslint/js`, `typescript-eslint`, `eslint-plugin-react-hooks`: the smallest set that lints TypeScript and the React hooks rules; the project's own rules forbid browser storage, `dangerouslySetInnerHTML` and inline `style`). Tests are pure-module tests in Vitest's `node` environment with a stubbed `fetch`; no `jsdom`, no testing library (not needed in M4.1 or M4.2: the form view's logic is in pure modules, and the browser behavior is covered by the one Playwright smoke test).
- **Only `react` and `react-dom` are runtime dependencies.** `npm run licenses` fails otherwise.

#### Admission notes for M4.1 (2026-10-29)

Versions are pinned exactly in `desk/package.json` (no `^` or `~`) and the lockfile (`desk/package-lock.json`, lockfile version 3) is committed. Licenses are the `license` field of the lockfile (`Verified (local measurement)`, npm 10.9.4, Node 22.22.0); versions from `npm view` on 2026-10-29.

| Package | Version | License | Use |
|---|---|---|---|
| `react`, `react-dom` | 19.3.1 | MIT | runtime (shipped) |
| `vite` | 8.3.5 | MIT | build and dev server |
| `@vitejs/plugin-react` | 6.1.2 | MIT | JSX transform for Vite |
| `typescript` | 6.0.3 | Apache-2.0 | `tsc --noEmit` (type check only; Vite transpiles) |
| `vitest` | 5.0.5 | MIT | tests |
| `eslint`, `@eslint/js` | 10.13.0, 10.0.1 | MIT | lint |
| `typescript-eslint` | 8.72.1 | MIT | lint rules for TypeScript |
| `eslint-plugin-react-hooks` | 7.1.1 | MIT | hooks rules |
| `@types/react`, `@types/react-dom` | 19.3.0 | MIT | types |

**TypeScript 6.0.3, not 7.0.2.** TypeScript 7.0.2 exists and the pre-research named it, but `typescript-eslint` 8.72.1 declares the peer range `typescript >=4.8.4 <6.1.0`, so `npm install` refuses the combination (`ERESOLVE`; `--legacy-peer-deps` was not used). The newest TypeScript inside that range, 6.0.3, is pinned. Move to 7 when `typescript-eslint` supports it.

**Package count (`Verified (local measurement)`).** The lockfile lists **197** packages (172 are installed on Linux x64; the rest are optional platform binaries of `rolldown`, `lightningcss` and similar that other platforms install). Only **3** are shipped in the built desk (`react`, `react-dom`, `scheduler`, all MIT); the other 194 are development tooling. By license: MIT 149 (3 shipped), Apache-2.0 16, ISC 10, BSD-2-Clause 6, BSD-3-Clause 2, **MPL-2.0 12** (all dev only: `lightningcss` 1.33.0 and its 11 platform packages, used by Vite's CSS tooling), BlueOak-1.0.0 1 (`minimatch`), CC-BY-4.0 1 (`caniuse-lite`, browser data).

**Frontend license allow-list (Accepted).** Enforced by `desk/scripts/check-licenses.mjs` (`npm run licenses`, plain Node, run in CI):

- every package in `package-lock.json` needs a `license` that is exactly one of these strings (a missing license, an SPDX expression or anything else fails);
- packages that are **shipped** (not `dev` in the lockfile): MIT, ISC, Apache-2.0, BSD-2-Clause, BSD-3-Clause;
- packages that are **development only**: the same plus MPL-2.0, BlueOak-1.0.0 and CC-BY-4.0.

**MPL-2.0 is accepted for development tooling only.** MPL-2.0 is file-level copyleft; nothing of `lightningcss` is part of the built desk, so it places no obligation on the MIT-licensed result. If an MPL-2.0 package ever becomes a shipped dependency (not `dev` in the lockfile), the check fails and the owner decides. The same check keeps the runtime dependency list to `react` and `react-dom`. **Limit of the "dev" flag.** The lockfile's `dev` flag says whether a package is reachable from `dependencies` only; it is as good as the imports of `desk/src/`. A development-only package (for example an MPL-2.0 one) that is `import`ed from `src/` would be bundled into the shipped desk although the lockfile calls it dev. Reviewers must check imports in `src/` for that; there is deliberately no scanner of `desk/dist`. Install scripts of dependencies are off: `desk/.npmrc` sets `ignore-scripts=true`, so `npm ci` (locally and in CI) runs none. Node: `^20.19.0 || >=22.12.0` (the range Vite 8 needs). The CI job also runs `npm audit --audit-level=high` (high and critical advisories fail the job; the daily schedule runs it too). `typescript` at 6.x is Apache-2.0 (NOTICE files are kept in `node_modules`, which is not shipped).

#### Admission note for M4.2 (2026-10-29): Playwright

One development dependency is added for the smoke test of owner decision 5 (one Playwright test, Chromium only):

| Package | Version | License | Use |
|---|---|---|---|
| `@playwright/test` | 1.64.1 | Apache-2.0 | test runner and API for `desk/e2e/smoke.spec.ts` (dev only) |
| `playwright`, `playwright-core` | 1.64.1 | Apache-2.0 | pulled in by `@playwright/test` (dev only; no other dependencies; `fsevents` is an optional macOS dependency that was already in the tree) |

`Verified (local measurement)`: licenses are the `license` field of the lockfile (`npm run licenses` passes: the lockfile now lists 200 packages, 3 of them shipped as before); the version is exact-pinned and 1.64.1 is the `latest` dist-tag of `npm view @playwright/test` on 2026-10-29. Nothing of Playwright is imported from `desk/src`; it is used by `desk/e2e` and `desk/playwright.config.ts` only (reviewers check that, like for any dev package). The Chromium that Playwright drives is **not** an npm package: CI downloads it with `npx playwright install --with-deps chromium` (an explicit command; `ignore-scripts=true` still holds for `npm ci`), it is never stored in the repository or shipped, and a developer who already has a Chromium points the test at it with `PLAYWRIGHT_CHROMIUM_EXECUTABLE`. Cost to keep in mind: the browser download is a CI-time network dependency and Playwright releases often (a new Chromium per release); the single test keeps the surface small, and the version is bumped by hand like any other pin. `@types/node` was **not** added: the two e2e files declare the one global they use (`desk/e2e/env.d.ts`).

The earlier remark that "UI library, router, data-fetching and form libraries are deferred" is answered by the decisions above.

### Admission note for M5.1 (2026-10-29): no new package

M5.1 adds **no external package** and no new version to `Cargo.lock`. The crate `forgebase-cli` is renamed `forgebase` and gets a library target; `axum` and `tower` (already direct dependencies of `forgebase-http`, same version and features) become **optional normal dependencies** of `forgebase`, enabled only by its cargo feature `testing` (the in-process HTTP client of `forgebase::testing` needs `axum::Router` and `tower::ServiceExt::oneshot`, as the `forgebase-http` tests do). The `testing` feature is off by default, so the stock binary and an app's release build do not carry the helpers; an app enables it in its `[dev-dependencies]`. The crate depends on itself as a dev-dependency with that feature, so that its own integration tests (which use the helpers) run in a plain `cargo test`. `tokio` also gets the `rt-multi-thread` runtime built by hand in the library (`Forgebase::run`) instead of `#[tokio::main]`; the features are the ones it already had. The public API re-exports `rust_decimal::Decimal` and `time::{Date, OffsetDateTime}` (the payloads of `Value`) from the versions already in the tree. Licenses: unchanged.

## Recommended baseline

Rows or parts marked (Accepted) are owner-accepted directions (ADRs 0002, 0004, 0005); everything else is Proposed.

| Concern | Recommendation | Main alternative |
|---|---|---|
| HTTP | axum (+ tower-http as needed) (Accepted); admitted in M3.3 without tower-http | actix-web |
| Runtime | tokio (explicit features) (Accepted) | none realistic (ecosystem tie-in accepted) |
| CLI | clap (derive) (Accepted 2026-10-28) | lexopt |
| DB | tokio-postgres + deadpool-postgres + in-house SQL/DDL builder (Accepted; confirmed by the M1 spike, see the spike result subsection) | sqlx (macros off); sea-query as builder |
| Framework migrations | custom embedded runner (Accepted with the DB direction) | sqlx migrate (if sqlx) |
| Serialization / validation | serde + serde_json; runtime metadata validator | garde for fixed structs |
| Metadata file parsing | toml + serde (parse only, in `forgebase-core`) (format Accepted 2026-10-28) | JSON |
| Decimal, date/time | `rust_decimal`, `time` (Accepted 2026-10-28) | chrono, jiff |
| Errors / logging | thiserror, anyhow, tracing | snafu, eyre, log + env_logger (not evaluated) |
| Auth | argon2 + opaque DB-backed sessions | tower-sessions with own store |
| Jobs | custom Postgres SKIP LOCKED queue | apalis (if sqlx, after maturing) |
| Config | toml + serde + env overlay | figment |
| TLS | none in app, TLS terminated at a reverse proxy (Accepted, ADR 0005); rustls+ring for Postgres behind a feature (Proposed) | native-tls |
| Frontend | React + TypeScript (Accepted, ADR 0002) + Vite + Vitest, no UI, router or data library (Accepted 2026-10-29, M4.1) | Vue/Svelte/Solid, htmx (not evaluated) |

Approximate core-stack weight: 136 packages (A) vs 179 (B); excludes TLS, sessions and sha2.

## License compatibility with MIT

From `cargo metadata` on stacks A and B (all targets), `Verified (local measurement)`:

- Dominant: `MIT OR Apache-2.0` and variants. Fine.
- Permissive, list in `deny.toml`: `Unlicense OR MIT` (choose MIT), `Zlib`, `Apache-2.0 OR BSL-1.0`, `MIT AND BSD-3-Clause` (matchit, axum's router; keep both notices), `(MIT OR Apache-2.0) AND Unicode-3.0`, `Unicode-3.0` alone (18 ICU4X crates, only in the sqlx stack; needs attribution), `BSD-3-Clause` (argh, if chosen).
- Copyleft seen: only `r-efi` (`MIT OR Apache-2.0 OR LGPL-2.1-or-later`), an OR expression so MIT applies. No MPL, GPL or AGPL-only crates appeared.
- Missing license metadata: apalis-postgres 1.0.0.
- ring 0.17.14: field `Apache-2.0 AND ISC`. Older releases carried OpenSSL/BoringSSL-derived text and other projects needed `clarify` entries (Source-indexed, https://gitlab.torproject.org/tpo/core/arti/-/issues/493 and https://apache.googlesource.com/opendal/+/HEAD/deny.toml). Whether 0.17.14's LICENSE file still has such text is Unverified; inspect before release. The "ring is unmaintained" advisory (RUSTSEC-2025-0007) was withdrawn (Source-indexed, https://rustsec.org/advisories/RUSTSEC-2025-0007).
- aws-lc-rs 1.18.2: `ISC AND (Apache-2.0 OR ISC)`; its C library likely carries OpenSSL-derived notices (Unverified).

Proposed `deny.toml` allow-list is in [maintenance-policy](maintenance-policy.md#cargo-deny).

## TLS note

- **HTTP:** terminate TLS at a reverse proxy; the app then needs no TLS crate.
- **Postgres:** optional on private networks, required for most managed databases.
  - rustls 0.23.46: pure Rust, no system OpenSSL. Default provider aws-lc-rs needs a C compiler and CMake (NASM on Windows); the `ring` provider avoids CMake. sqlx `tls-rustls` currently resolves to ring (verified locally).
  - native-tls 0.2.18: uses system OpenSSL on Linux; adds a system dependency and version drift.
- **Recommendation:** reverse proxy for HTTPS (Accepted, [ADR 0005](../adr/0005-http-stack.md)); Postgres TLS (Proposed) behind a cargo feature using rustls with ring via `tokio-postgres-rustls` 0.14.0 (third-party, MIT), or sqlx `tls-rustls-ring` if sqlx is chosen (feature name Unverified). Root certificates via `webpki-roots` or `rustls-native-certs` (not evaluated).

## Open questions

1. ~~DB layer: is sqlx convenience worth +43 packages, MSRV 1.94 and the governance transition?~~ Resolved by the M1 spike (2026-10-28): no; ADR 0004 direction confirmed.
2. tokio-postgres bus factor: contributor distribution and PR latency.
3. sqlx under transact-rs: maintainer list and post-move cadence.
4. ring 0.17.14 license file: need for a `deny.toml` clarify entry.
5. refinery: actual 0.10.0 release date and activity.
6. figment: upstream activity.
7. apalis-postgres: missing license metadata and maturity.
8. axum: any announced 0.9 plans; tokio's current LTS line.
9. ~~argon2 0.6: API changes, audit status, current OWASP Argon2id parameters.~~ Partly resolved in M3.1: admitted at 0.6.0 (features `alloc`, `password-hash`), parameters m = 19456 KiB, t = 2, p = 1 (owner decision, ADR 0010). Audit status and a re-check of the current OWASP numbers stay Unverified.
10. ~~Decimal and date/time types~~ Resolved (Accepted, owner decision 2026-10-28): `rust_decimal` and `time`, see the section in 4. Database access.
11. serde_json `preserve_order` / `arbitrary_precision` workspace-wide implications.
12. ~~Session token hashing: `sha2` weight and license; cookie handling via `cookie`/`axum-extra` vs hand-written.~~ Resolved: hashing in PostgreSQL (M3.1), cookie hand-written (M3.3, see the admission notes).
13. MSRV-aware resolver (resolver "3") behavior with the pinned toolchain.
14. ~~React license confirmation; frontend audit tooling.~~ Resolved in M4.1: React 19.3.1 is MIT; `npm audit` and the license allow-list run in CI (see section 12). Still open: bus factor and release cadence of the Vite and Vitest toolchain, and the move to TypeScript 7 once `typescript-eslint` supports it.
15. Re-run every `cargo info` check at decision time.
