//! [`App`]: a named set of DocTypes, hooks and SQL data migrations that is compiled into the
//! binary.

use std::fmt;
use std::sync::Arc;

use forgebase_core::hooks::DocTypeHooks;

use crate::jobs::{JobHandler, JobOptions};

/// A DocType definition file embedded in the binary: its path (for error messages) and its
/// TOML text. Build the list with [`doctypes!`](crate::doctypes); by hand (in a test, for
/// example) with [`DocTypeFile::new`].
///
/// The fields are private and the type is `#[non_exhaustive]`, so that fields can be added in a
/// later version without breaking apps; read them with the getters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DocTypeFile {
    path: &'static str,
    toml: &'static str,
}

impl DocTypeFile {
    /// A file from its path and its TOML text. The path is only used in messages and by
    /// `testing::assert_doctypes_listed`; use the path as you would write it in
    /// [`doctypes!`](crate::doctypes), relative to the app crate's root.
    pub const fn new(path: &'static str, toml: &'static str) -> Self {
        Self { path, toml }
    }

    /// Used by [`doctypes!`](crate::doctypes). Not part of the API: it may change with the macro.
    #[doc(hidden)]
    pub const fn __new(path: &'static str, toml: &'static str) -> Self {
        Self { path, toml }
    }

    /// The path as written in the macro, relative to the app crate's root.
    pub const fn path(&self) -> &'static str {
        self.path
    }

    /// The file's contents.
    pub const fn toml(&self) -> &'static str {
        self.toml
    }
}

/// An app data migration embedded in the binary. Build the list with
/// [`migrations!`](crate::migrations); by hand with [`MigrationFile::new`].
///
/// The fields are private and the type is `#[non_exhaustive]`, like [`DocTypeFile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct MigrationFile {
    version: i32,
    name: &'static str,
    path: &'static str,
    sql: &'static str,
}

impl MigrationFile {
    /// A migration from its version (positive; strictly increasing within the app), a short
    /// name (recorded in `fb_app_migration`), the path (for messages) and the SQL.
    ///
    /// The SQL runs in one transaction together with the row that records it, so it must not
    /// contain `BEGIN` or `COMMIT`. Never edit an applied migration: its checksum is verified,
    /// and a changed one is refused.
    pub const fn new(
        version: i32,
        name: &'static str,
        path: &'static str,
        sql: &'static str,
    ) -> Self {
        Self {
            version,
            name,
            path,
            sql,
        }
    }

    /// Used by [`migrations!`](crate::migrations). Not part of the API: it may change with the
    /// macro.
    #[doc(hidden)]
    pub const fn __new(
        version: i32,
        name: &'static str,
        path: &'static str,
        sql: &'static str,
    ) -> Self {
        Self {
            version,
            name,
            path,
            sql,
        }
    }

    /// The version.
    pub const fn version(&self) -> i32 {
        self.version
    }

    /// The name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The path as written in the macro, for messages.
    pub const fn path(&self) -> &'static str {
        self.path
    }

    /// The SQL.
    pub const fn sql(&self) -> &'static str {
        self.sql
    }
}

/// Embeds DocType definition files, with paths relative to **the root of the crate that calls
/// the macro** (its `CARGO_MANIFEST_DIR`). Gives a `&'static [DocTypeFile]` for
/// [`App::doctypes`].
///
/// ```ignore
/// App::new("sales").doctypes(forgebase::doctypes![
///     "doctypes/customer.toml",
///     "doctypes/sales_order.toml",
/// ])
/// ```
///
/// The list is explicit on purpose: there is no directory scanning and no registration magic.
/// A file you forget to list is caught by
/// `testing::assert_doctypes_listed` (feature `testing`).
#[macro_export]
macro_rules! doctypes {
    ($($path:literal),* $(,)?) => {
        const {
            &[$($crate::DocTypeFile::__new(
                $path,
                ::core::include_str!(::core::concat!(::core::env!("CARGO_MANIFEST_DIR"), "/", $path)),
            )),*]
        }
    };
}

/// Embeds app data migrations (SQL files), with paths relative to **the root of the crate that
/// calls the macro**. Each entry is `(version, name, path)`. Gives a
/// `&'static [MigrationFile]` for [`App::migrations`].
///
/// ```ignore
/// App::new("sales").migrations(forgebase::migrations![
///     (1, "backfill_status", "migrations/0001_backfill_status.sql"),
/// ])
/// ```
#[macro_export]
macro_rules! migrations {
    ($(($version:expr, $name:expr, $path:literal)),* $(,)?) => {
        const {
            &[$($crate::MigrationFile::__new(
                $version,
                $name,
                $path,
                ::core::include_str!(::core::concat!(::core::env!("CARGO_MANIFEST_DIR"), "/", $path)),
            )),*]
        }
    };
}

/// A named unit of an application: DocTypes, lifecycle hooks and SQL data migrations, compiled
/// into the binary. Several apps make one binary; they are registered with
/// [`Forgebase::app`](crate::Forgebase::app) **in dependency order**, and nothing is sorted
/// for you. See ADR 0012.
///
/// ```ignore
/// pub fn app() -> forgebase::App {
///     forgebase::App::new("sales")
///         .version(env!("CARGO_PKG_VERSION"))
///         .depends_on(&["crm"])
///         .doctypes(forgebase::doctypes!["doctypes/sales_order.toml"])
///         .hooks("sales_order", OrderHooks)
///         .migrations(forgebase::migrations![(1, "backfill", "migrations/0001_backfill.sql")])
/// }
/// ```
///
/// An `App` is only data: building one cannot fail. Everything wrong with it (an invalid name,
/// a missing dependency, a hook on an unknown DocType, ...) is reported together with the
/// other problems when the binary starts ([`StartupError`](crate::StartupError)).
///
/// What an app may do:
///
/// - own DocTypes (a DocType name belongs to exactly one app);
/// - link to DocTypes of itself and of the apps it depends on (directly or through other
///   apps), and nothing else;
/// - register hooks ([`DocTypeHooks`]) for its own DocTypes and for DocTypes of the apps it
///   depends on. Hooks run in app order, then in registration order within the app: the hooks
///   of an app that extends another run after the hooks of the app it extends;
/// - declare SQL data migrations;
/// - register background jobs ([`App::job`]), which hooks, other jobs and the command line can
///   enqueue.
///
/// What it may not do: change the definition of another app's DocType (add fields to it, for
/// example), run code outside the binary, or reach the database from a hook (a hook may
/// enqueue a job, and the job reaches the database).
#[derive(Clone)]
pub struct App {
    pub(crate) name: String,
    pub(crate) version: Option<&'static str>,
    pub(crate) depends_on: Vec<String>,
    pub(crate) doctypes: Vec<DocTypeFile>,
    pub(crate) hooks: Vec<(String, Arc<dyn DocTypeHooks>)>,
    pub(crate) migrations: Vec<MigrationFile>,
    pub(crate) jobs: Vec<(String, Arc<dyn JobHandler>, JobOptions)>,
}

impl fmt::Debug for App {
    /// Shows the name, version, dependencies, DocType files and migrations; hooks are shown by
    /// DocType only (a hook set has no `Debug`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hooked: Vec<&str> = self
            .hooks
            .iter()
            .map(|(doctype, _)| doctype.as_str())
            .collect();
        f.debug_struct("App")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("depends_on", &self.depends_on)
            .field("doctypes", &self.doctypes)
            .field("hooks", &hooked)
            .field("migrations", &self.migrations)
            .field(
                "jobs",
                &self
                    .jobs
                    .iter()
                    .map(|(name, _, _)| name)
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl App {
    /// Starts an app. The name follows the rules of a DocType name (lower-case ASCII letters,
    /// digits and underscores, starting with a letter) and must be unique in the binary. It
    /// is recorded in `fb_app` and names the app in messages and in the metadata API.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            version: None,
            depends_on: Vec::new(),
            doctypes: Vec::new(),
            hooks: Vec::new(),
            migrations: Vec::new(),
            jobs: Vec::new(),
        }
    }

    /// The version of the app, recorded in `fb_app` at every `migrate`. Usually
    /// `env!("CARGO_PKG_VERSION")`. It has no other effect.
    pub fn version(mut self, version: &'static str) -> Self {
        self.version = Some(version);
        self
    }

    /// Names apps this app depends on. They must be registered **before** this app. Adds to
    /// the dependencies already named.
    pub fn depends_on(mut self, apps: &[&str]) -> Self {
        self.depends_on.extend(apps.iter().map(|a| (*a).to_owned()));
        self
    }

    /// Adds DocType definition files, from [`doctypes!`](crate::doctypes) (or built by hand).
    /// Calling it again adds more files.
    pub fn doctypes(mut self, files: &'static [DocTypeFile]) -> Self {
        self.doctypes.extend_from_slice(files);
        self
    }

    /// Registers a hook set for a DocType of this app or of an app it depends on (directly or
    /// through other apps). Several hook sets may be registered for one DocType; they run in
    /// registration order, after the hooks of the apps before this one. A child DocType has no
    /// hooks (the hooks of its parent see its rows). See [`DocTypeHooks`] for the pipeline.
    ///
    /// The hook set is taken by value and kept behind an `Arc` internally. There is one hook
    /// object per call: to use the same logic for several DocTypes, build one value per DocType
    /// (they can share state through an `Arc` field of their own).
    pub fn hooks(mut self, doctype: &str, hooks: impl DocTypeHooks + 'static) -> Self {
        self.hooks.push((doctype.to_owned(), Arc::new(hooks)));
        self
    }

    /// Adds SQL data migrations, from [`migrations!`](crate::migrations) (or built by hand).
    /// Versions must be positive and strictly increasing over everything added. Calling it
    /// again adds more.
    ///
    /// `forgebase migrate` applies them after the schema sync, once per database, each in its
    /// own transaction. The SQL runs with the database role of the binary: app code is trusted.
    pub fn migrations(mut self, files: &'static [MigrationFile]) -> Self {
        self.migrations.extend_from_slice(files);
        self
    }

    /// Registers a background job with the default options (5 attempts, a 30 second delay that
    /// doubles up to 1 hour, a 5 minute timeout). `name` follows the rules of a DocType name and
    /// is unique within the app; the job is known as `<app>.<name>`. See
    /// [`JobHandler`](crate::JobHandler) for what a handler does and how delivery works.
    ///
    /// An invalid or duplicate name is a startup error, like the other mistakes of an app.
    pub fn job(self, name: &str, handler: impl JobHandler + 'static) -> Self {
        self.job_with(name, handler, JobOptions::new())
    }

    /// Like [`App::job`], with [`JobOptions`]. Invalid options are a startup error.
    pub fn job_with(
        mut self,
        name: &str,
        handler: impl JobHandler + 'static,
        options: JobOptions,
    ) -> Self {
        self.jobs
            .push((name.to_owned(), Arc::new(handler), options));
        self
    }

    /// The name given to [`App::new`].
    pub fn name(&self) -> &str {
        &self.name
    }
}
