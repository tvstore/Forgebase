//! Putting the apps together: validating them, loading their DocTypes into one registry,
//! registering their hooks, and migrating them.
//!
//! Order of work (documented in ADR 0012): the apps' own consistency (names, dependencies,
//! order, migration versions); the DocTypes of every app in app order, then the DocTypes of
//! the `--doctypes` directory (the app `local`); the references between DocTypes (existing
//! rules, plus: a Link or Table may only target a DocType of its own app or of a dependency);
//! the hooks in app order. Every problem found is collected and reported together. The
//! reference and hook checks only run when every DocType file loaded, because a file that did
//! not load would make them report follow-up errors.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use forgebase_core::ident::validate_doctype_name;
use forgebase_core::meta::{DefinitionError, DocType, FieldType, Registry};
use forgebase_db::{AppMigration, AppSpec, DbError, MigrateReport, Pool, SyncOptions};

use crate::app::{App, MigrationFile};
use crate::doctypes::read_dir;
use crate::error::bullets;

/// The name of the app that the `--doctypes` directory forms.
pub(crate) const LOCAL_APP: &str = "local";

/// Everything wrong with the apps of a binary, found at startup. All problems are collected, so
/// one run shows them all.
///
/// Printed (`Display`) as up to two lists: "invalid app setup" (names, dependencies, order,
/// ownership, hooks, migrations, links between apps) and "cannot load DocType files" (a file
/// that cannot be read or parsed, an invalid DocType, a Link to a DocType nobody defines, ...,
/// each naming its file).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StartupError {
    apps: Vec<String>,
    files: Vec<String>,
}

impl StartupError {
    /// Every problem, one message each: first the app setup problems, then the DocType file
    /// problems.
    pub fn problems(&self) -> impl Iterator<Item = &str> {
        self.apps.iter().chain(&self.files).map(String::as_str)
    }

    fn is_empty(&self) -> bool {
        self.apps.is_empty() && self.files.is_empty()
    }
}

impl fmt::Display for StartupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.apps.is_empty() {
            write!(f, "invalid app setup:\n{}", bullets(&self.apps))?;
        }
        if !self.files.is_empty() {
            if !self.apps.is_empty() {
                f.write_str("\n")?;
            }
            write!(f, "cannot load DocType files:\n{}", bullets(&self.files))?;
        }
        Ok(())
    }
}

impl std::error::Error for StartupError {}

/// The apps of a binary, composed: one registry and what `migrate` needs.
pub(crate) struct Composed {
    pub registry: Registry,
    /// The registered apps (not `local`) in app order.
    apps: Vec<AppInfo>,
}

struct AppInfo {
    name: String,
    version: Option<&'static str>,
    migrations: Vec<MigrationFile>,
}

impl Composed {
    /// How many apps the binary has (the `--doctypes` directory is not counted).
    pub fn app_count(&self) -> usize {
        self.apps.len()
    }

    /// Framework migrations, schema sync, then the app migrations in app order
    /// ([`forgebase_db::migrate_apps`]).
    pub async fn migrate(
        &self,
        pool: &Pool,
        options: &SyncOptions,
    ) -> Result<MigrateReport, DbError> {
        let migrations: Vec<Vec<AppMigration<'_>>> = self
            .apps
            .iter()
            .map(|app| {
                app.migrations
                    .iter()
                    .map(|m| AppMigration {
                        version: m.version,
                        name: m.name,
                        sql: m.sql,
                    })
                    .collect()
            })
            .collect();
        let specs: Vec<AppSpec<'_>> = self
            .apps
            .iter()
            .zip(&migrations)
            .map(|(app, migrations)| AppSpec {
                name: &app.name,
                version: app.version,
                migrations,
            })
            .collect();
        forgebase_db::migrate_apps(pool, &self.registry, &specs, options).await
    }
}

/// One DocType file: where it came from and its text.
struct File<'a> {
    path: &'a str,
    text: &'a str,
}

/// Composes `apps` (in this order) and, when given, the DocType files of the directory `local`
/// as the last app, which depends on all the others.
pub(crate) fn compose(apps: &[App], local: Option<&Path>) -> Result<Composed, StartupError> {
    let mut errors = StartupError::default();
    let reach = check_apps(apps, &mut errors);

    let mut registry = Registry::new();
    // DocType name -> (app, file) that defined it.
    let mut defined_in: BTreeMap<String, (String, String)> = BTreeMap::new();
    for app in apps {
        let files: Vec<File<'_>> = app
            .doctypes
            .iter()
            .map(|f| File {
                path: f.path,
                text: f.toml,
            })
            .collect();
        load(
            &app.name,
            &files,
            &mut registry,
            &mut defined_in,
            &mut errors,
        );
    }
    // The directory is read after the apps. Its problems are reported like any other file
    // problem, without an app prefix (it is not an app of the binary).
    let mut local_reach: BTreeSet<String> = BTreeSet::new();
    if let Some(dir) = local {
        let (sources, problems) = read_dir(dir);
        errors.files.extend(problems);
        let files: Vec<File<'_>> = sources
            .iter()
            .map(|s| File {
                path: &s.path,
                text: &s.text,
            })
            .collect();
        load(
            LOCAL_APP,
            &files,
            &mut registry,
            &mut defined_in,
            &mut errors,
        );
        local_reach = apps.iter().map(|a| a.name.clone()).collect();
    }

    // Only when everything loaded: a Link to a DocType from a broken file would be reported
    // twice, and a hook on one as an unknown DocType.
    if errors.files.is_empty() {
        check_references(&registry, &defined_in, &reach, &local_reach, &mut errors);
        check_hooks(apps, &reach, &mut registry, &mut errors);
    }

    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(Composed {
        registry,
        apps: apps
            .iter()
            .map(|a| AppInfo {
                name: a.name.clone(),
                version: a.version,
                migrations: a.migrations.clone(),
            })
            .collect(),
    })
}

/// Checks names, dependencies, order and migration versions. Returns, per app name, the names
/// of the apps it may use: its dependencies and theirs.
fn check_apps(apps: &[App], errors: &mut StartupError) -> BTreeMap<String, BTreeSet<String>> {
    let mut reach: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for app in apps {
        let name = app.name.as_str();
        if let Err(reason) = validate_doctype_name(name) {
            errors.apps.push(format!("app name {name:?} {reason}"));
        }
        if name == LOCAL_APP {
            errors.apps.push(format!(
                "app name `{LOCAL_APP}` is reserved for the DocTypes of the --doctypes directory"
            ));
        }
        if reach.contains_key(name) {
            errors
                .apps
                .push(format!("app `{name}` is registered more than once"));
        }
        let mut uses = BTreeSet::new();
        for dependency in &app.depends_on {
            if dependency == name {
                errors.apps.push(format!("app `{name}` depends on itself"));
            } else if let Some(theirs) = reach.get(dependency) {
                uses.insert(dependency.clone());
                uses.extend(theirs.iter().cloned());
            } else if apps.iter().any(|a| a.name == *dependency) {
                errors.apps.push(format!(
                    "app `{name}` depends on `{dependency}`, which is registered after it; \
                     register `{dependency}` first (apps are never sorted)"
                ));
            } else {
                errors.apps.push(format!(
                    "app `{name}` depends on `{dependency}`, which is not registered"
                ));
            }
        }
        reach.entry(name.to_owned()).or_insert(uses);

        let mut previous: Option<&MigrationFile> = None;
        for migration in &app.migrations {
            if migration.version < 1 {
                errors.apps.push(format!(
                    "app `{name}`: migration version {} ({}, {}) must be positive",
                    migration.version, migration.name, migration.path
                ));
            } else if let Some(before) = previous.filter(|p| p.version >= migration.version) {
                errors.apps.push(format!(
                    "app `{name}`: migration versions must increase: {} ({}, {}) comes after {} ({})",
                    migration.version, migration.name, migration.path, before.version, before.name
                ));
            }
            previous = Some(migration);
        }
    }
    reach
}

/// Parses `files` and adds their DocTypes to the registry as owned by `app`.
fn load(
    app: &str,
    files: &[File<'_>],
    registry: &mut Registry,
    defined_in: &mut BTreeMap<String, (String, String)>,
    errors: &mut StartupError,
) {
    let prefix = if app == LOCAL_APP {
        String::new()
    } else {
        format!("app `{app}`: ")
    };
    for file in files {
        let doctype = match DocType::from_toml_str(file.text) {
            Ok(doctype) => doctype,
            Err(e) => {
                errors.files.push(format!("{prefix}{}: {e}", file.path));
                continue;
            }
        };
        let name = doctype.name.clone();
        if let Some((first_app, first_path)) = defined_in.get(&name) {
            errors.files.push(if first_app == app {
                format!(
                    "{prefix}{}: doctype `{name}` is already defined in {first_path}",
                    file.path
                )
            } else {
                format!(
                    "{prefix}{}: doctype `{name}` is already defined by app `{first_app}` ({first_path}); \
                     a DocType belongs to one app",
                    file.path
                )
            });
            continue;
        }
        match registry.add_owned(app, doctype) {
            Ok(()) => {
                defined_in.insert(name, (app.to_owned(), file.path.to_owned()));
            }
            Err(problems) => {
                for problem in problems {
                    errors
                        .files
                        .push(format!("{prefix}{}: {problem}", file.path));
                }
            }
        }
    }
}

/// The reference rules of the registry, and the app rule: a Link or Table field may only
/// target a DocType of its own app or of an app it depends on.
fn check_references(
    registry: &Registry,
    defined_in: &BTreeMap<String, (String, String)>,
    reach: &BTreeMap<String, BTreeSet<String>>,
    local_reach: &BTreeSet<String>,
    errors: &mut StartupError,
) {
    if let Err(problems) = registry.check_references() {
        for problem in problems {
            let doctype = match &problem {
                DefinitionError::UnknownLinkTarget { doctype, .. }
                | DefinitionError::LinkToChild { doctype, .. }
                | DefinitionError::UnknownTableTarget { doctype, .. }
                | DefinitionError::TableTargetNotChild { doctype, .. }
                | DefinitionError::UnusedChild { doctype } => Some(doctype),
                DefinitionError::ChildSharedByTables { child, .. } => Some(child),
                _ => None,
            };
            errors
                .files
                .push(match doctype.and_then(|d| defined_in.get(d)) {
                    Some((app, path)) if app == LOCAL_APP => format!("{path}: {problem}"),
                    Some((app, path)) => format!("app `{app}`: {path}: {problem}"),
                    None => problem.to_string(),
                });
        }
        return;
    }
    for doctype in registry.iter() {
        let Some((app, path)) = defined_in.get(&doctype.name) else {
            continue;
        };
        let uses = if app == LOCAL_APP {
            local_reach
        } else {
            match reach.get(app) {
                Some(uses) => uses,
                None => continue,
            }
        };
        for field in &doctype.fields {
            let (FieldType::Link { target } | FieldType::Table { target }) = &field.field_type
            else {
                continue;
            };
            let Some(owner) = registry.owner(target) else {
                continue;
            };
            if owner != app && !uses.contains(owner) {
                errors.apps.push(format!(
                    "app `{app}`: {path}: field `{}` of doctype `{}` refers to `{target}`, which belongs to app `{owner}`; \
                     app `{app}` does not depend on `{owner}` (add .depends_on(&[\"{owner}\"]), registered before `{app}`)",
                    field.name, doctype.name
                ));
            }
        }
    }
}

/// Registers the hooks in app order, then registration order. An app may hook DocTypes of its
/// own and of its dependencies.
fn check_hooks(
    apps: &[App],
    reach: &BTreeMap<String, BTreeSet<String>>,
    registry: &mut Registry,
    errors: &mut StartupError,
) {
    for app in apps {
        let uses = reach.get(&app.name);
        for (doctype, hooks) in &app.hooks {
            let name = &app.name;
            let Some(owner) = registry.owner(doctype).map(str::to_owned) else {
                errors.apps.push(format!(
                    "app `{name}`: hooks registered for unknown DocType `{doctype}`"
                ));
                continue;
            };
            if owner != *name && !uses.is_some_and(|u| u.contains(&owner)) {
                errors.apps.push(format!(
                    "app `{name}`: hooks registered for DocType `{doctype}` of app `{owner}`, \
                     which `{name}` does not depend on"
                ));
                continue;
            }
            if let Err(problem) = registry.add_hooks(doctype, hooks.clone()) {
                errors
                    .apps
                    .push(format!("app `{name}`: hooks for `{doctype}`: {problem}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::app::DocTypeFile;

    const CUSTOMER: &str = "name = \"customer\"\nlabel = \"C\"\n";
    const LINKING: &str = "name = \"{n}\"\nlabel = \"L\"\n[[fields]]\nname = \"f\"\ntype = \"link\"\ntarget = \"{t}\"\n";

    fn crm() -> App {
        App::new("crm").doctypes(&[DocTypeFile {
            path: "doctypes/customer.toml",
            toml: CUSTOMER,
        }])
    }

    fn scratch(tag: &str, files: &[(&str, String)]) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("forgebase-compose-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch dir");
        for (name, text) in files {
            fs::write(dir.join(name), text).expect("write");
        }
        dir
    }

    fn linking(name: &str, target: &str) -> String {
        LINKING.replace("{n}", name).replace("{t}", target)
    }

    #[test]
    fn the_doctypes_directory_is_the_app_local_and_may_link_to_every_app() {
        let dir = scratch("link", &[("order.toml", linking("order", "customer"))]);
        let composed = compose(&[crm()], Some(&dir)).expect("composes");
        assert_eq!(composed.registry.owner("customer"), Some("crm"));
        assert_eq!(composed.registry.owner("order"), Some(LOCAL_APP));
        assert_eq!(composed.app_count(), 1);
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn an_app_cannot_link_to_the_doctypes_directory() {
        // crm is registered first and cannot depend on `local`; a link to it is an error.
        let dir = scratch(
            "backlink",
            &[("extra.toml", "name = \"extra\"\nlabel = \"E\"\n".to_owned())],
        );
        let crm = App::new("crm").doctypes(&[DocTypeFile {
            path: "doctypes/order.toml",
            toml: "name = \"order\"\nlabel = \"O\"\n[[fields]]\nname = \"f\"\ntype = \"link\"\ntarget = \"extra\"\n",
        }]);
        let error = compose(&[crm], Some(&dir)).err().expect("refused");
        let text = error.to_string();
        assert!(
            text.contains("belongs to app `local`") && text.contains("does not depend on `local`"),
            "{text}"
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_doctype_of_the_directory_cannot_reuse_the_name_of_an_app_doctype() {
        let dir = scratch("dup", &[("customer.toml", CUSTOMER.to_owned())]);
        let error = compose(&[crm()], Some(&dir)).err().expect("refused");
        let text = error.to_string();
        assert!(
            text.starts_with("cannot load DocType files:\n")
                && text.contains("customer.toml: doctype `customer` is already defined by app `crm` (doctypes/customer.toml)"),
            "{text}"
        );
        fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn startup_errors_print_the_app_list_before_the_file_list() {
        let error = StartupError {
            apps: vec!["a".to_owned()],
            files: vec!["b".to_owned(), "c".to_owned()],
        };
        assert_eq!(
            error.to_string(),
            "invalid app setup:\n  - a\ncannot load DocType files:\n  - b\n  - c"
        );
        assert_eq!(error.problems().collect::<Vec<_>>(), ["a", "b", "c"]);
        let only_files = StartupError {
            apps: vec![],
            files: vec!["b".to_owned()],
        };
        assert_eq!(only_files.to_string(), "cannot load DocType files:\n  - b");
    }
}
