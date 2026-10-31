//! The JSON form of stored values and documents: the one place that decides how a
//! [`StoredDocument`] is written as JSON.
//!
//! The HTTP API, the command-line tool (through `forgebase_http::json`) and the webhook payloads
//! of [`crate::webhooks`] all use it, so a receiver of a webhook sees the same document shape as
//! a client of `GET /api/v1/resource/...`. The mapping of each field type is documented in
//! `forgebase_http::json`.

use forgebase_core::value::Value;
use serde_json::{Map, Number, Value as Json};
use time::format_description::well_known::Rfc3339;
use time::format_description::{self, BorrowedFormatItem};
use time::{OffsetDateTime, UtcOffset};

use crate::StoredDocument;

/// A stored value that cannot be written as JSON (for example a year beyond 9999).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot format field `{field}` as JSON: {problem}")]
pub struct OutputError {
    /// The field name.
    pub field: String,
    /// What is wrong.
    pub problem: String,
}

/// The format of a Date: `YYYY-MM-DD`.
pub fn date_format()
-> Result<Vec<BorrowedFormatItem<'static>>, time::error::InvalidFormatDescription> {
    format_description::parse_borrowed::<2>("[year]-[month]-[day]")
}

/// RFC 3339 in UTC. `to_offset` panics when the UTC instant is out of range, so it is checked.
pub fn utc_rfc3339(t: &OffsetDateTime) -> Result<String, String> {
    t.checked_to_offset(UtcOffset::UTC)
        .ok_or_else(|| "out of range once converted to UTC".to_owned())?
        .format(&Rfc3339)
        .map_err(|e| e.to_string())
}

/// One value as JSON. `name` is only used in the error.
pub fn value_to_json(name: &str, value: &Value) -> Result<Json, OutputError> {
    let output = |problem: String| OutputError {
        field: name.to_owned(),
        problem,
    };
    Ok(match value {
        Value::Null => Json::Null,
        Value::Text(s) => Json::String(s.clone()),
        Value::Int(n) => Json::Number(Number::from(*n)),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Bool(b) => Json::Bool(*b),
        Value::Date(d) => {
            let format = date_format().map_err(|e| output(e.to_string()))?;
            Json::String(d.format(&format).map_err(|e| output(e.to_string()))?)
        }
        Value::Datetime(t) => Json::String(utc_rfc3339(t).map_err(output)?),
        // `Value` is `#[non_exhaustive]` for other crates: a new kind gets its JSON form here;
        // until then it is an output error, never a silent drop.
        other => return Err(output(format!("no JSON form for a {} value", other.kind()))),
    })
}

/// A stored document as a JSON object: the standard columns (and, for a submittable DocType,
/// `docstatus` as 0, 1 or 2 and `amended_from` as a name or `null`; for a DocType with a
/// workflow, `workflow_state` as the name of the state, absent while the document has none), the
/// field values and, for each Table field, the array of its rows.
pub fn document_to_json(doc: &StoredDocument) -> Result<Json, OutputError> {
    let rfc3339 = |field: &str, t: &OffsetDateTime| {
        utc_rfc3339(t)
            .map(Json::String)
            .map_err(|problem| OutputError {
                field: field.to_owned(),
                problem,
            })
    };
    let mut object = Map::new();
    object.insert(
        "id".to_owned(),
        Json::String(doc.id.hyphenated().to_string()),
    );
    object.insert("name".to_owned(), Json::String(doc.name.clone()));
    object.insert("owner".to_owned(), Json::String(doc.owner.clone()));
    object.insert(
        "created_at".to_owned(),
        rfc3339("created_at", &doc.created_at)?,
    );
    object.insert(
        "modified_at".to_owned(),
        rfc3339("modified_at", &doc.modified_at)?,
    );
    object.insert(
        "modified_by".to_owned(),
        Json::String(doc.modified_by.clone()),
    );
    object.insert(
        "version".to_owned(),
        Json::Number(Number::from(doc.version)),
    );
    // Only a submittable DocType has a docstatus: 0 draft, 1 submitted, 2 cancelled.
    if let Some(status) = doc.docstatus {
        object.insert(
            "docstatus".to_owned(),
            Json::Number(Number::from(status.as_i16())),
        );
        object.insert(
            "amended_from".to_owned(),
            doc.amended_from.clone().map_or(Json::Null, Json::String),
        );
    }
    // Only a DocType with a workflow has a state (a document that has none yet has no key).
    if let Some(state) = &doc.workflow_state {
        object.insert("workflow_state".to_owned(), Json::String(state.clone()));
    }
    for (name, value) in &doc.values {
        object.insert(name.clone(), value_to_json(name, value)?);
    }
    for (name, rows) in &doc.tables {
        let mut items = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            let mut item = Map::new();
            for (field, value) in row {
                let place = format!("{name} row {}: {field}", i + 1);
                item.insert(field.clone(), value_to_json(&place, value)?);
            }
            items.push(Json::Object(item));
        }
        object.insert(name.clone(), Json::Array(items));
    }
    Ok(Json::Object(object))
}
