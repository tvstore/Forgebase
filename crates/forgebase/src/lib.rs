//! Forgebase: the crate applications depend on, and the `forgebase` command-line binary.
//!
//! This is the **only public API** of Forgebase (ADR 0012). `forgebase-core`, `forgebase-db` and
//! `forgebase-http` are internal crates with no compatibility promise; this crate re-exports
//! exactly what an app needs. The version is 0.x: the API may still change between minor
//! versions, and every change is listed in `CHANGELOG.md`.
//!
//! # Apps
//!
//! An [`App`] is a named set of DocTypes (TOML files embedded with [`doctypes!`]), lifecycle
//! hooks and SQL data migrations ([`migrations!`]). A binary is built from apps in an explicit
//! order, like this (`main.rs` of an application):
//!
//! ```ignore
//! fn main() -> std::process::ExitCode {
//!     forgebase::Forgebase::new().app(crm::app()).app(sales::app()).run()
//! }
//! ```
//!
//! There is no global registry, no code generation and no runtime plugin: the list above is the
//! whole wiring, and an app is a plain function that returns data.
//!
//! - **Loading order.** The DocTypes of all apps in app order, then the DocTypes of the
//!   `--doctypes DIR` directory (the app `local`, which depends on all the others), then the
//!   reference checks, then the hooks in app order. The app migrations run at `migrate`, after
//!   the schema sync, in app order.
//! - **Ownership.** Every DocType belongs to the app that defines it (the metadata API says
//!   which: `"app"`). A DocType name used by two apps is a startup error. An app cannot change
//!   another app's DocType definition.
//! - **Dependencies.** `depends_on` names apps that must be registered earlier. An app may link
//!   to DocTypes of itself and of its dependencies only, and may register hooks for them.
//! - **Hooks.** For one DocType the hooks run in app order, then in registration order within
//!   an app: an app that extends another app's DocType runs after it, and its hooks can reject
//!   what the first app accepted.
//! - **App migrations.** Versioned SQL, once per database, checksummed, each in its own
//!   transaction (with the `--lock-timeout-ms` lock timeout); a changed or removed applied
//!   migration, or a new one numbered below an applied one, is refused. `fb_app` records which
//!   app and version were migrated. App code is trusted: the SQL runs with the database role
//!   of the binary.
//! - **Startup errors.** All of the above is checked before anything runs; every problem is
//!   reported together ([`StartupError`], exit code 1).
//!
//! # Background jobs
//!
//! An app registers jobs with [`App::job`]; a hook ([`HookContext::enqueue`]), another job
//! ([`JobContext::enqueue`]) or `forgebase job enqueue` enqueues them, and `forgebase worker`
//! runs them. A job runs in one database transaction together with its completion, with a
//! documents API as the system context ([`JobContext::documents`]); delivery is at-least-once.
//! A hook's jobs are written in the same transaction as the document change (the transactional
//! outbox). See [`JobHandler`] for the rules and ADR 0013.
//!
//! # The binary
//!
//! `forgebase [OPTIONS] COMMAND`: `migrate` (framework migrations, DocType schema sync and app
//! migrations), `doc` (`insert`, `get`, `list`, `update`, `delete`, `submit`, `cancel`, `amend`
//! and `transition` of documents; `update`, `delete`, `submit`, `cancel`, `amend` and `transition`
//! need `--version`; `transition` takes the action of a workflow, M8.1),
//! `user` (`create`, `set-password`, `add-role`, `remove-role`, `disable`, `enable`; passwords
//! come from standard input), `token` (`create`, `revoke`), `serve` (the HTTP API; no
//! migrations), `worker` (runs background jobs until Ctrl-C or SIGTERM), `job` (`list`, `show`,
//! `retry` and `enqueue`; the command line is the only interface to jobs) and `new-app` (writes a new app crate that builds and passes its tests; needs no
//! database, see `docs/guide/first-app.md`). Configuration comes from `FORGEBASE_DATABASE_URL` (the database of the
//! one implicit site `default`), and for `serve` from `FORGEBASE_LISTEN`, `FORGEBASE_ALLOWED_HOSTS`,
//! `FORGEBASE_DEV_INSECURE_COOKIES` and `FORGEBASE_DESK_DIR`.
//!
//! # Sites
//!
//! Without a config file there is one site, `default`. With `--config forgebase.toml` (or
//! `FORGEBASE_CONFIG`) there are the sites listed in it, **one database each** (ADR 0014): a
//! name, host names, the name of the environment variable that holds the database URL and,
//! optionally, the `apps` installed on the site (a subset of the binary's apps, closed under
//! `depends_on`; absent means all). `--site NAME` picks the site of a database command (required
//! with several sites, except for `serve`, which serves all of them and routes by `Host`).
//! `forgebase site list` shows the sites and `forgebase site create NAME` creates a site's
//! database and migrates it. The public API of this crate is unchanged: sites are a matter of
//! the command line and the config file.
//!
//! The binary is a trusted local tool: `doc` commands run as the **system context**
//! (`Caller::System` in `forgebase-core`), so no permission rules apply to them, and
//! `--actor` only names who is recorded (in `owner` / `modified_by`; default `cli`). That includes
//! `doc transition`: the roles of a workflow transition are not checked either. It does not
//! authenticate anyone. Real users and their permissions apply to the HTTP API (`serve`). See
//! `docs/blueprint/architecture.md`, ADR 0002, ADR 0011 and ADR 0012.
//!
//! Exit codes: 0 success; 1 runtime, validation, startup or database error (message on standard
//! error, starting with `error:`); 2 usage error (from clap).
//!
//! # Tests of an app
//!
//! With the `testing` cargo feature (enable it in `[dev-dependencies]`), the `testing` module
//! gives a throwaway PostgreSQL database, an in-process HTTP client, `run_jobs` (runs the jobs
//! that are due) and a guard that finds a DocType file missing from the embedded list.

mod app;
mod args;
mod backup;
mod builder;
mod builtin;
mod compose;
mod config;
mod doctypes;
mod documents;
mod error;
mod job_cmd;
mod jobs;
mod new_app;
mod report;
mod reports;
mod run;
mod schedule;
mod worker;

#[cfg(feature = "testing")]
pub mod testing;

pub use app::{App, DocTypeFile, MigrationFile};
pub use builder::Forgebase;
pub use compose::StartupError;
pub use documents::{DocError, Documents, FilterOp, ListQuery, StoredDocument};
pub use jobs::{JobContext, JobError, JobFuture, JobHandler, JobOptions};
pub use reports::{Column, Param, Report};
pub use schedule::Schedule;

// What a hook needs to read and change a document. These are the types of `forgebase-core`.
pub use forgebase_core::document::{Changes, DocStatus, Document, Row};
pub use forgebase_core::hooks::{
    DocTypeHooks, EnqueuedJob, EnqueuedJobs, HookContext, HookError, Operation, TransitionInfo,
};
pub use forgebase_core::value::Value;
// The payloads of `Value`, re-exported so an app uses the same versions.
pub use rust_decimal::Decimal;
pub use time::{Date, OffsetDateTime, Weekday};
// The payload of a job. Re-exported so an app and the framework use the same version.
pub use serde_json;
