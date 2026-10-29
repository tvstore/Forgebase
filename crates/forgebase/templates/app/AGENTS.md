# AGENTS.md: {{name}}

Instructions for every agent and human who changes this app. `{{name}}` is a [Forgebase](https://github.com/agilma/Forgebase) app: a Rust crate with DocTypes (TOML), hooks (Rust) and SQL migrations, compiled into one binary. Build for longevity, not complexity: the simplest change that meets the need wins, and nothing is added before it is needed.

## Where things live

- `src/lib.rs`: `app()` returns the `forgebase::App` (name, version, DocTypes, hooks, migrations). All wiring is here.
- `src/main.rs`: the binary, `Forgebase::new().app(..).run()`; apps in dependency order.
- `doctypes/*.toml`: one DocType per file, with its `[[permissions]]`.
- `migrations/*.sql`: data migrations, listed in `src/lib.rs`.
- `tests/app.rs`: the tests. Format of the DocType files: `docs/blueprint/architecture.md` in the Forgebase checkout (the path is in `Cargo.toml`); the guide is `docs/guide/first-app.md` there.

## Rules

1. **Depend on the `forgebase` crate only.** Never on `forgebase-core`, `forgebase-db` or `forgebase-http`: they are internal and change without notice. If something you need is not exported, say so; do not work around it.
2. **List every DocType file** in `doctypes![..]` in `src/lib.rs`. Nothing is scanned; `every_doctype_file_is_listed` fails otherwise.
3. **DocType names start with the app name** (`{{name}}_note`): names are global across apps. Fields and names follow `^[a-z][a-z0-9_]*$`.
4. **Change DocTypes additively**: add fields and DocTypes. A changed type or length is refused by `migrate`; to rename or reshape data, add a migration. Do not edit another app's DocTypes: extend them with hooks.
5. **Migrations are append-only SQL.** Never edit or delete one that was applied (its checksum is verified and a change is refused); add the next version. No `BEGIN` or `COMMIT` in them. They are for data, not for the tables of DocTypes.
6. **Hooks are pure and quick.** They read and change the document they are given; no database access, no network, no clock-dependent surprises. They run before the framework's own validation, so check what you read. Permissions are checked before hooks and are not a hook's job.
7. **Permissions are explicit.** A DocType without `[[permissions]]` is closed to everyone but `Administrator`. Give the least a role needs.
8. **Tests are not skipped.** Tests that need PostgreSQL use `forgebase::testing` and fail when `FORGEBASE_TEST_DATABASE_URL` is not set. Add a test with every change in proportion to its risk.
9. **Other apps:** `.depends_on(&["other"])`, add the crate as a dependency, register it before this app in `main.rs`. Link and hook only to this app and its dependencies.

## Workflow

For every task: **plan** (scope, non-goals, acceptance criteria, affected files, tests; wait for the owner's approval), **do** (only the approved plan; update tests and docs), **review** (a separate agent or session that sees the requirements and the diff, not the implementer's summary), **confirm** (report what passed and what did not; ask before anything outside the plan). Do not claim a check you did not run. Keep unrelated changes out of the commit and never overwrite someone else's work.

## Before every commit

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test        # needs FORGEBASE_TEST_DATABASE_URL, see README.md
```

Commit `Cargo.lock`. Update `README.md` when commands or conventions change.
