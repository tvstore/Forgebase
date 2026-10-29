//! The `{{name}}` app: DocTypes, hooks and migrations. `main.rs` puts it into a binary.

use forgebase::{App, DocTypeHooks, Document, HookContext, HookError, Value};

/// The app. A plain function that returns data: nothing is registered anywhere else.
pub fn app() -> App {
    App::new("{{name}}")
        .version(env!("CARGO_PKG_VERSION"))
        // To build on another app: `.depends_on(&["crm"])`, and register `crm` before this app
        // in `main.rs`.
        // Every file in doctypes/ must be listed here (a test checks that).
        .doctypes(forgebase::doctypes!["doctypes/{{name}}_note.toml"])
        .hooks("{{name}}_note", NoteRules)
    // Data migrations (SQL, append-only): see migrations/README.md.
    // .migrations(forgebase::migrations![(1, "backfill", "migrations/0001_backfill.sql")])
}

/// Business rules of a note. Hooks are plain functions of the document: no database access.
pub struct NoteRules;

impl DocTypeHooks for NoteRules {
    fn validate(&self, _ctx: &HookContext<'_>, doc: &mut Document) -> Result<(), HookError> {
        // Hooks run before the framework's own validation, so a value may be missing or of
        // another kind: look before you change.
        if let Some(Value::Text(title)) = doc.values.get_mut("title") {
            *title = title.trim().to_owned();
            if title.is_empty() {
                return Err(HookError::new("the title must not be blank"));
            }
        }
        Ok(())
    }
}
