# {{name}}

A [Forgebase](https://github.com/tvstore/Forgebase) app: the library (`src/lib.rs`, the function `app()`) and a binary that runs it (`src/main.rs`). Agents and contributors: read [AGENTS.md](AGENTS.md).

## Build, test, run

Needs Rust {{rust_version}} or newer and, for the tests and for running, a PostgreSQL 16 server.

```sh
cargo build
# Tests: a superuser (or CREATEDB) URL; every test creates and drops its own database.
export FORGEBASE_TEST_DATABASE_URL=postgres://user:password@localhost:5432/postgres
cargo test

# Run: an empty database of your own.
export FORGEBASE_DATABASE_URL=postgres://user:password@localhost:5432/{{name}}
cargo run -- migrate                                        # schema, then data migrations
echo 'a long password' | cargo run -- user create ann --role "{{title}} User"
FORGEBASE_DEV_INSECURE_COOKIES=1 cargo run -- serve         # http://127.0.0.1:8000/api/v1
```

`cargo run -- --help` lists all commands (`doc`, `user`, `token`, ...). `FORGEBASE_DEV_INSECURE_COOKIES=1` is for plain-HTTP development only. `cargo run -- serve` does not migrate: run `migrate` after every change to DocTypes or migrations.

## What is where

| Path | Purpose |
|---|---|
| `src/lib.rs` | `app()`: the app's name, version, DocTypes, hooks and migrations |
| `src/main.rs` | The binary: the list of apps, in dependency order |
| `doctypes/` | One TOML file per DocType, with its permissions |
| `migrations/` | SQL data migrations |
| `tests/app.rs` | Tests, using `forgebase::testing` |

## Extend it

- **A DocType**: add `doctypes/<app>_<thing>.toml` and list it in `doctypes![..]`. Change an existing one additively (new fields); a test fails if a file is not listed.
- **A hook**: implement `DocTypeHooks` for a type and register it with `.hooks("<doctype>", Rules)`. Hooks have no database access.
- **A migration**: see `migrations/README.md`.
- **Build on another app**: add it as a dependency in `Cargo.toml`, name it in `.depends_on(&["other"])`, and register it before this app in `main.rs`. You may then link to its DocTypes and register hooks for them.

## Forgebase dependency

`Cargo.toml` points at a local Forgebase checkout (`path`). To use a git revision instead, replace both `forgebase` lines with `forgebase = { git = "https://github.com/tvstore/Forgebase", rev = "<commit>" }` (and the same with `features = ["testing"]` under `[dev-dependencies]`). `Cargo.lock` was copied from that checkout when this crate was created, so you start with the versions Forgebase was tested with; commit it.

The full guide is `docs/guide/first-app.md` in the Forgebase checkout.
