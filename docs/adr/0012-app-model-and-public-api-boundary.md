# 0012 App model and public API boundary

- Status: Accepted (owner decisions 1 to 4 and 6 and the API boundary of the M5 plan v2, 2026-10-29; implementation details of M5.1 separated below)
- Date: 2026-10-29

## Context

[ADR 0006](0006-extension-model.md) decided that a Forgebase app is a Rust crate compiled into the user's binary and registered through an explicit list. Until M4 the repository had no way to write such an app: the binary (`forgebase-cli`) was an executable only, the DocTypes were loaded from a directory at runtime, and hooks could only be registered by code that depends on the internal crates. The owner wants Forgebase to be open to extension the way ERPNext and custom apps stack on Frappe: a framework, apps on top of it, and apps on top of apps, with low long-term maintenance cost. M5 builds that. M5.1 (this ADR) is the framework part; M5.2 adds `forgebase new-app` and the guide, M5.3 adds example apps.

## Decision

The decisions below are the owner's (M5 plan v2, 2026-10-29).

1. **One entry crate, `forgebase`.** `forgebase-cli` is renamed to `forgebase` and is a library and a binary (the binary is still called `forgebase`). The workspace still has four crates. [ADR 0002](0002-modular-monolith-and-workspace-layout.md) is amended (its status says so; its decision text is unchanged).
2. **DocTypes are embedded in the binary** through an explicit list (`forgebase::doctypes!`) of files that are read at compile time, plus a guard test helper that fails when a file in the app's directory is not in the list. No directory scanning at runtime, no code generation.
3. **Composition.** A binary has several apps, registered in an **explicit order**; `depends_on` names apps that must come earlier and is checked. An app may register hooks for the DocTypes of the apps it depends on. An app cannot change another app's DocType definition (custom fields on another app's DocType are a later milestone, M10: the model leaves room for an overlay applied after the app's own definitions).
4. **App data migrations.** An app declares versioned SQL migrations that `forgebase migrate` applies once per database, and `fb_app` records which apps and versions were migrated.
5. (Unchanged) No runtime plugins ([ADR 0006](0006-extension-model.md)); hooks have no database access ([ADR 0009](0009-lifecycle-pipeline-and-hooks.md)).
6. **`forgebase::testing`**, behind the cargo feature `testing`, gives apps the helpers for their own tests.

**Public API boundary.** Apps depend on the `forgebase` crate **only**. `forgebase-core`, `forgebase-db` and `forgebase-http` are internal: no compatibility promise, their crate documentation says so. `forgebase` re-exports exactly what an app needs (list below). The version is 0.x: the public API may change between minor versions, and every change goes in the changelog.

### The public surface (crate root of `forgebase`)

| Item | Purpose |
|---|---|
| `App` (`new`, `version`, `depends_on`, `doctypes`, `hooks`, `migrations`, `name`) | An app, built with a small builder. Only data: building it cannot fail. |
| `DocTypeFile`, `MigrationFile`, macros `doctypes!`, `migrations!` | The embedded files: path and contents, resolved relative to the calling crate. |
| `Forgebase` (`new`, `app`, `check`, `run`) | A binary: its apps in order, and the command line. |
| `StartupError` (`problems`, `Display`) | Every problem of the app setup, together. |
| `DocTypeHooks`, `HookContext`, `HookError`, `Operation`, `Document`, `Row`, `Value`, `Decimal`, `Date`, `OffsetDateTime` | What a hook implements, receives and changes. `Decimal`, `Date` and `OffsetDateTime` are the payloads of `Value`, re-exported so that an app uses the same versions. |
| `testing` (feature `testing`): `TestDb`, `HttpClient`, `Reply`, `Migrated`, `AppliedMigration`, `PASSWORD`, `assert_doctypes_listed`, `Json`, `json!`, `serde_json` | Helpers for tests of an app. |

Nothing else is public. The stock `forgebase` binary is `Forgebase::new().run()`: no apps, DocTypes from `--doctypes DIR`.

## Implementation details (M5.1)

These were chosen while implementing M5.1; they are to be reviewed with that change, not owner decisions.

**Loading order.** (1) Check the apps themselves (names, dependencies, order, migration versions). (2) The DocTypes of all apps, in app order, each file parsed and added to one registry with its owner. (3) The `--doctypes DIR` DocTypes as one more app, named `local`, that depends on every app of the binary (so the stock binary and any binary can still load a directory). (4) The reference checks: the existing ones (`Registry::check_references`) and the app rule below. (5) The hooks, in app order, then registration order within an app. (6) At `migrate`, the app migrations in app order. All problems found are collected and reported together (`StartupError`, exit code 1); the reference and hook checks only run when every DocType file loaded, because a broken file would make them report follow-up errors. Every command checks this before it does anything else.

**Startup errors.** Invalid app name (the rules of a DocType name; `local` is reserved); a duplicate app name; a dependency that is not registered; a dependency registered **after** its dependent (there is no sorting: the order in `main` is the order); an app that depends on itself; a DocType defined by two apps (the message names both apps and both files); a DocType file that cannot be parsed or is invalid (names the app and the file); hooks for an unknown DocType, for a child DocType, or for a DocType of an app that is not the app itself or a (transitive) dependency; a Link or Table field that targets a DocType of an app that is not the app itself or a (transitive) dependency; migration versions that are not positive and strictly increasing.

**Ownership.** `forgebase_core::meta::Registry` records an owner tag (a string) per DocType (`add_owned`, `owner`); it attaches no meaning to it, so `forgebase-core` has no app concept beyond that string. The metadata API returns `"app"` for each DocType in `GET /api/v1/meta` and `GET /api/v1/meta/{doctype}` (`null` when the registry records no owner; a field addition, backward compatible). The desk does not use it yet.

**Hook order.** For one DocType: app order, then registration order within the app. An app that extends another runs after it, and its hook can reject what the first app accepted; the first app's changes to the document are visible to the second.

**App migrations.** Framework migration 0004 creates `fb_app` (`name`, `version`, `migrated_at`) and `fb_app_migration` (`app`, `version`, `name`, `checksum`, `applied_at`). `migrate` runs: framework migrations, verification of the applied app migrations, schema sync of all apps' DocTypes, then per app (in app order) its pending migrations by version, then the upsert of its `fb_app` row. The advisory lock of the framework migrations is taken as a **session-level** lock for the whole `migrate` (the framework migrations take the same key per transaction on the same connection, which does not conflict), so two `migrate` runs never interleave; the wait is bounded by `--lock-timeout-ms`. Each app migration is one transaction together with its `fb_app_migration` row; one that fails is rolled back and stops the run, and the migrations before it stay applied (so do the framework migrations and the schema sync). An applied migration whose SQL changed (SHA-256 computed by PostgreSQL, like the framework migrations) or that the app does not declare any more is an error; this is checked **before** the schema sync, so a refused run applies nothing but possibly the framework migrations. A pending migration with a lower version than an applied one is applied (in version order), like the framework's. Apps that are not in the binary are left as they are (nothing is uninstalled; their rows stay). `migrate --dry-run` lists the pending app migrations without running them; while framework migrations are pending it lists all declared ones, because no app migration can be applied before migration 0004. `fb_app.version` is whatever the app declares (`NULL` if it declares none); `migrated_at` is the time of the last `migrate` that saw the app.

**SQL and trust.** App migrations are SQL only (no Rust-code migrations). They run with the same database role as everything else: app code is trusted and compiled in (see the [security model](../blueprint/security-model.md)). They must not contain `BEGIN` or `COMMIT`.

**Embedded files.** The macros expand to `include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", path))`, evaluated in the crate that calls the macro, so paths are relative to the app crate's root. The expansion is a `&'static [DocTypeFile]` or `&'static [MigrationFile]`; the fields are public, so a test can also build one by hand. The guard `testing::assert_doctypes_listed(&app, dir)` compares file names (the app's DocType files are assumed to be in the one directory).

**`testing`.** `TestDb` creates a database `fb_test_app_*` on the server of `FORGEBASE_TEST_DATABASE_URL`, runs `migrate` for a `Forgebase`, creates users, runs SQL and hands out an `HttpClient` (the router called in process through `tower`, session cookie and CSRF handled). It adds `axum` and `tower` as optional dependencies of `forgebase` (both are already in the tree through `forgebase-http`). The crate's own integration tests turn the feature on with a dev-dependency on itself, so a plain `cargo test` runs them.

**The `--doctypes` option.** It is optional in a binary with apps. The stock binary has no apps, so there it is still required (a usage error, exit code 2, as before).

## Alternatives considered

- **Several public crates** (`forgebase-core`, `-db`, `-http` all public): a larger compatibility surface to keep stable for no benefit; rejected by the owner in favour of one entry crate.
- **Runtime plugins, `inventory`/`linkme` registration, proc macros**: against [ADR 0006](0006-extension-model.md) and the maintenance goal. An explicit function that returns data (`App`) and an explicit list in `main` is the whole mechanism.
- **Sorting apps by their dependencies**: hides the real order, which matters for hooks. The order is explicit and a wrong one is an error.
- **Scanning the `doctypes/` directory at compile time (build script or proc macro)**: magic and a build dependency; the explicit list plus a guard test costs one line per DocType.
- **Rust-code migrations**: more API surface and trust questions; SQL covers data backfills. Revisit with a real need.
- **Letting an app add fields to another app's DocType**: the Frappe "custom field" feature. Left to M10 (an overlay on top of the app's definitions) so the DocType definition stays owned by exactly one app.

## Consequences

- An app is a normal crate with a function `app() -> forgebase::App`; a binary is a normal crate with a `main` that lists the apps. Nothing is discovered at runtime, so an agent or a human finds all the wiring by reading `main`.
- Adding or changing what `forgebase` exports is an API change: update this list and the changelog.
- The internal crates can be refactored freely as long as the public surface above holds.
- A DocType can only be extended by hooks and, later (M10), by an overlay; it cannot be redefined by another app.
- The app model adds one migration (0004) and two small tables; uninstalling an app is out of scope.
