//! Field permission levels in the document operations (M10.2, ADR 0024).
//!
//! The decision is made by [`forgebase_core::permission::field_access`], one pure function;
//! this module only applies it. A field the caller cannot read is **unknown** to the caller:
//!
//! - the **read** paths (`get`, `list`, `summary`, export, saved views, the metadata the HTTP
//!   layer uses to read input and filters) work on [`readable_doctype`], the DocType without
//!   the hidden fields, so a hidden field is neither selected nor accepted in a filter, a sort,
//!   a group-by or an aggregate, and the error is the very one of a name that does not exist;
//! - the **write** paths work on the whole DocType (hooks see every field and the stored
//!   document is merged, so a lower-level user cannot wipe hidden data): [`check_input`] first
//!   refuses the caller's input for a hidden field as unknown and for a readable but
//!   unwritable field as [`ValidationError::NotPermitted`], then [`redact`] removes the hidden
//!   fields from the document that is returned, and [`scrub`] removes their names from errors.
//!
//! Nothing here runs for a DocType without `perm_level` or for the system context and an
//! administrator, so those behave exactly as before.

use std::collections::BTreeMap;

use forgebase_core::document::{Row, ValidationError};
use forgebase_core::meta::DocType;
use forgebase_core::permission::FieldAccess;
use forgebase_core::value::Value;

use crate::document::StoredDocument;
use crate::error::DbError;

/// The caller's field access if it matters for `doctype`: `None` for a DocType without
/// `perm_level` and for a caller that can use every level.
pub(crate) fn restricted(doctype: &DocType, access: FieldAccess) -> Option<FieldAccess> {
    (doctype.has_field_levels() && !access.is_all()).then_some(access)
}

/// Refuses the caller's input (`values` and `tables`, a new document's or an update's changes)
/// for fields the caller may not use, before any hook runs. `stored` is the document being
/// updated (`None` for an insert).
///
/// - A name that is not a field **or a field the caller cannot read** is
///   [`ValidationError::UnknownField`], whatever the value (null too, and even when it equals
///   the stored value): the two cannot be told apart. For a DocType with levels the check of
///   unknown names therefore runs here, early and alone, for every caller it applies to, so the
///   answer for a hidden field is the answer for a missing one.
/// - A field the caller can read but not write is [`ValidationError::NotPermitted`] when the
///   input would change it: a non-null value on an insert, a value that differs from the stored
///   one on an update (sending the stored value back is allowed, as for `read_only`); for a
///   Table field, rows on an insert and rows that differ from the stored ones on an update.
///
/// A field at level 0 is never refused here (the document rights decide), and nothing is
/// refused for a caller without restrictions ([`restricted`]).
pub(crate) fn check_input(
    doctype: &DocType,
    access: Option<FieldAccess>,
    (values, tables): (&BTreeMap<String, Value>, &BTreeMap<String, Vec<Row>>),
    stored: Option<&StoredDocument>,
) -> Result<(), DbError> {
    let Some(access) = access else {
        return Ok(());
    };
    let mut errors = Vec::new();
    let mut classify = |name: &str, changed: &dyn Fn() -> bool| match doctype.field(name) {
        None => errors.push(ValidationError::UnknownField {
            field: name.to_owned(),
        }),
        Some(f) if !access.shows(f.perm_level) => errors.push(ValidationError::UnknownField {
            field: name.to_owned(),
        }),
        Some(f) if f.perm_level > 0 && !access.can_write(f.perm_level) && changed() => {
            errors.push(ValidationError::NotPermitted {
                field: name.to_owned(),
            });
        }
        Some(_) => {}
    };
    let null = Value::Null;
    for (name, value) in values {
        classify(name, &|| match stored {
            Some(s) => s.values.get(name).unwrap_or(&null) != value,
            None => *value != Value::Null,
        });
    }
    for (name, rows) in tables {
        classify(name, &|| match stored {
            Some(s) => s.tables.get(name).map_or(!rows.is_empty(), |r| r != rows),
            None => !rows.is_empty(),
        });
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(DbError::Validation(errors))
    }
}

/// The document as the caller may see it: the values and tables of the fields the caller
/// cannot read are removed. The standard columns are level 0 and stay.
pub(crate) fn redact(
    mut stored: StoredDocument,
    doctype: &DocType,
    access: Option<FieldAccess>,
) -> StoredDocument {
    let Some(access) = access else {
        return stored;
    };
    for field in doctype
        .fields
        .iter()
        .filter(|f| !access.shows(f.perm_level))
    {
        stored.values.remove(&field.name);
        stored.tables.remove(&field.name);
    }
    stored
}

/// The error with every mention of a field the caller cannot read taken out (the leak review of
/// ADR 0024): a framework validation problem about a hidden field (a stored value that is not
/// valid any more, a value a hook set, a Link a hook changed) becomes
/// [`ValidationError::Hidden`], which names nothing, and so does a locked-field error about one
/// (a hook changed it). Other errors do not name fields, or name only what the caller sent.
pub(crate) fn scrub(error: DbError, doctype: &DocType, access: Option<FieldAccess>) -> DbError {
    let Some(access) = access else {
        return error;
    };
    let hidden = |name: &str| {
        doctype
            .field(name)
            .is_some_and(|f| !access.shows(f.perm_level))
    };
    match error {
        DbError::Validation(errors) => {
            let mut scrubbed: Vec<ValidationError> = Vec::with_capacity(errors.len());
            for error in errors {
                // What the caller sent (an unknown name, a field they may not write) is theirs to
                // read back; everything else about a hidden field is not.
                let about_input = matches!(
                    error,
                    ValidationError::UnknownField { .. } | ValidationError::NotPermitted { .. }
                );
                let error = if !about_input && hidden(error.field()) {
                    ValidationError::Hidden
                } else {
                    error
                };
                // One generic entry is enough.
                if !(error == ValidationError::Hidden
                    && scrubbed.contains(&ValidationError::Hidden))
                {
                    scrubbed.push(error);
                }
            }
            DbError::Validation(scrubbed)
        }
        DbError::FieldLocked { field, .. } if hidden(&field) => {
            DbError::Validation(vec![ValidationError::Hidden])
        }
        other => other,
    }
}
