//! JSON <-> [`Value`] mapping, shared by the HTTP API and the command-line tool.
//!
//! | Field type | JSON |
//! |---|---|
//! | Data, Text, Select | string |
//! | Link | string: the `name` of the target document (the database stores its id) |
//! | Table | array of row objects in order, each mapped like a document (no nested Table); `null` means no rows; in an update the array replaces the rows |
//! | Int | integer (a float, or an integer outside the `i64` range, is rejected) |
//! | Decimal | **string**, for example `"1234.50"` (a JSON number is rejected so no precision is lost to floats); every form `rust_decimal` parses is accepted with its exact value, for example `"1e3"`, `"+12.5"`, `".5"` |
//! | Check | `true` / `false` |
//! | Date | string `"YYYY-MM-DD"` |
//! | Datetime | RFC 3339 string; output is always UTC with `Z`. Microsecond precision (PostgreSQL `timestamptz`): finer digits are truncated, not rounded or rejected |
//! | any | `null` for no value |
//!
//! The input must be a JSON object. Output of a Table field is an array of row objects in order
//! (a row has one key per field of the child DocType, `null` when empty); row ids are not output.
//! A duplicate key is accepted and the last one wins (serde_json behavior). Output keys are
//! sorted alphabetically. Values stored by other tools outside the supported ranges (years beyond
//! ±9999, `infinity`, `NaN`) cannot be read; see [`forgebase_db::DbError::Decode`]. Keys that are
//! not fields of the DocType are passed on (with a null value) so that core validation reports
//! them together with the other problems.
//! The CLI reuses this module (`forgebase_http::json`) so both interfaces map identically.

use std::collections::BTreeMap;

use forgebase_core::document::Row;
use forgebase_core::meta::{DocType, FieldType, Registry};
use forgebase_core::value::Value;
use forgebase_db::{Filter, FilterOp, Sort, StoredDocument};
use rust_decimal::Decimal;
use serde_json::{Map, Number, Value as Json};
use time::format_description::well_known::Rfc3339;
use time::format_description::{self, BorrowedFormatItem};
use time::{Date, OffsetDateTime, UtcOffset};

/// Why JSON input or output could not be mapped.
#[derive(Debug, thiserror::Error)]
pub enum JsonError {
    /// The input text is not valid JSON.
    #[error("invalid JSON: {0}")]
    Syntax(#[from] serde_json::Error),
    /// The input is valid JSON but not an object.
    #[error("the data must be a JSON object")]
    NotAnObject,
    /// A value does not fit its field.
    #[error("field `{field}`: {problem}")]
    Field {
        /// The field name.
        field: String,
        /// What is wrong.
        problem: String,
    },
    /// A filter or sort value cannot be used.
    #[error("{0}")]
    Query(String),
    /// The output document could not be serialized to JSON text (not expected to happen).
    #[error("cannot serialize the document as JSON: {0}")]
    Serialize(serde_json::Error),
    /// A stored value cannot be written as JSON (for example a year beyond 9999).
    #[error("cannot format field `{field}` as JSON: {problem}")]
    Output {
        /// The field name.
        field: String,
        /// What is wrong.
        problem: String,
    },
}

fn bad(field: &str, problem: impl Into<String>) -> JsonError {
    JsonError::Field {
        field: field.to_owned(),
        problem: problem.into(),
    }
}

fn date_format() -> Result<Vec<BorrowedFormatItem<'static>>, time::error::InvalidFormatDescription>
{
    format_description::parse_borrowed::<2>("[year]-[month]-[day]")
}

/// The values and table rows of a JSON object.
#[derive(Debug, Default)]
pub struct Input {
    /// Field values (every field except Table fields).
    pub values: BTreeMap<String, Value>,
    /// The rows of each Table field that the input mentions.
    pub tables: BTreeMap<String, Vec<Row>>,
}

/// Parses JSON text into field values and table rows for `doctype`. `registry` provides the
/// child DocTypes of its Table fields.
pub fn parse_input(registry: &Registry, doctype: &DocType, text: &str) -> Result<Input, JsonError> {
    input_from_json(registry, doctype, serde_json::from_str::<Json>(text)?)
}

/// Like [`parse_input`] for JSON that is already parsed.
pub fn input_from_json(
    registry: &Registry,
    doctype: &DocType,
    json: Json,
) -> Result<Input, JsonError> {
    let Json::Object(object) = json else {
        return Err(JsonError::NotAnObject);
    };
    let mut input = Input::default();
    for (name, json) in &object {
        match doctype.field(name).map(|f| &f.field_type) {
            Some(FieldType::Table { target }) => {
                input
                    .tables
                    .insert(name.clone(), to_rows(registry, name, target, json)?);
            }
            Some(field_type) => {
                input
                    .values
                    .insert(name.clone(), to_value(name, field_type, json)?);
            }
            // Reported by validation as an unknown field.
            None => {
                input.values.insert(name.clone(), Value::Null);
            }
        }
    }
    Ok(input)
}

/// The rows of a Table field: an array of objects (`null` is no rows).
fn to_rows(
    registry: &Registry,
    name: &str,
    target: &str,
    json: &Json,
) -> Result<Vec<Row>, JsonError> {
    let items = match json {
        Json::Null => return Ok(Vec::new()),
        Json::Array(items) => items,
        other => {
            return Err(bad(
                name,
                format!("expected an array of rows, found {}", kind(other)),
            ));
        }
    };
    let child = registry
        .get(target)
        .ok_or_else(|| bad(name, format!("unknown child doctype `{target}`")))?;
    let mut rows = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let in_row = |problem: String| bad(name, format!("row {}: {problem}", i + 1));
        let Json::Object(object) = item else {
            return Err(in_row(format!("expected an object, found {}", kind(item))));
        };
        let mut row = Row::new();
        for (field, json) in object {
            let value = match child.field(field) {
                Some(f) => {
                    to_value(field, &f.field_type, json).map_err(|e| in_row(e.to_string()))?
                }
                // Reported by validation as an unknown field of the row.
                None => Value::Null,
            };
            row.insert(field.clone(), value);
        }
        rows.push(row);
    }
    Ok(rows)
}

fn to_value(name: &str, field_type: &FieldType, json: &Json) -> Result<Value, JsonError> {
    if json.is_null() {
        return Ok(Value::Null);
    }
    let expected = |what: &str| bad(name, format!("expected {what}, found {}", kind(json)));
    match field_type {
        FieldType::Data { .. }
        | FieldType::Text
        | FieldType::Select { .. }
        | FieldType::Link { .. } => json
            .as_str()
            .map(|s| Value::Text(s.to_owned()))
            .ok_or_else(|| expected("a string")),
        FieldType::Int => match json {
            Json::Number(n) => n.as_i64().map(Value::Int).ok_or_else(|| {
                bad(
                    name,
                    format!("{n} is not an integer in the range of a 64-bit signed integer"),
                )
            }),
            _ => Err(expected("an integer")),
        },
        FieldType::Decimal { .. } => match json {
            Json::String(s) => s
                .parse::<Decimal>()
                .map(Value::Decimal)
                .map_err(|e| bad(name, format!("`{s}` is not a decimal number: {e}"))),
            Json::Number(_) => Err(bad(
                name,
                "a decimal must be a JSON string such as \"12.50\", not a number",
            )),
            _ => Err(expected("a decimal string")),
        },
        FieldType::Check => json
            .as_bool()
            .map(Value::Bool)
            .ok_or_else(|| expected("true or false")),
        FieldType::Date => {
            let s = json.as_str().ok_or_else(|| expected("a date string"))?;
            let format = date_format().map_err(|e| bad(name, e.to_string()))?;
            Date::parse(s, &format)
                .map(Value::Date)
                .map_err(|_| bad(name, format!("`{s}` is not a date in the form YYYY-MM-DD")))
        }
        FieldType::Table { .. } => Err(bad(
            name,
            "a table field takes a list of rows, not a single value",
        )),
        FieldType::Datetime => {
            let s = json
                .as_str()
                .ok_or_else(|| expected("an RFC 3339 string"))?;
            OffsetDateTime::parse(s, &Rfc3339)
                .map(Value::Datetime)
                .map_err(|_| {
                    bad(
                        name,
                        format!(
                            "`{s}` is not an RFC 3339 date and time, e.g. 2024-03-15T10:20:30Z"
                        ),
                    )
                })
        }
    }
}

fn kind(json: &Json) -> &'static str {
    match json {
        Json::Null => "null",
        Json::Bool(_) => "a boolean",
        Json::Number(_) => "a number",
        Json::String(_) => "a string",
        Json::Array(_) => "an array",
        Json::Object(_) => "an object",
    }
}

/// RFC 3339 in UTC. `to_offset` panics when the UTC instant is out of range, so it is checked.
fn utc_rfc3339(t: &OffsetDateTime) -> Result<String, String> {
    t.checked_to_offset(UtcOffset::UTC)
        .ok_or_else(|| "out of range once converted to UTC".to_owned())?
        .format(&Rfc3339)
        .map_err(|e| e.to_string())
}

fn from_value(name: &str, value: &Value) -> Result<Json, JsonError> {
    let output = |problem: String| JsonError::Output {
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
    })
}

/// A stored document as a JSON object: the standard columns (and, for a submittable DocType,
/// `docstatus` as 0, 1 or 2 and `amended_from` as a name or `null`), the field values and, for
/// each Table field, the array of its rows.
pub fn document_to_json(doc: &StoredDocument) -> Result<Json, JsonError> {
    let rfc3339 = |field: &str, t: &OffsetDateTime| {
        utc_rfc3339(t)
            .map(Json::String)
            .map_err(|problem| JsonError::Output {
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
    for (name, value) in &doc.values {
        object.insert(name.clone(), from_value(name, value)?);
    }
    for (name, rows) in &doc.tables {
        let mut items = Vec::with_capacity(rows.len());
        for (i, row) in rows.iter().enumerate() {
            let mut item = Map::new();
            for (field, value) in row {
                let place = format!("{name} row {}: {field}", i + 1);
                item.insert(field.clone(), from_value(&place, value)?);
            }
            items.push(Json::Object(item));
        }
        object.insert(name.clone(), Json::Array(items));
    }
    Ok(Json::Object(object))
}

/// Parses the filter JSON (`doc list --filter`, the `filters` query parameter): an array of `{"field", "op", "value"}` objects.
/// `value` is mapped by the field's type like in `--data` (a Link field takes the target's name;
/// `docstatus` and `version` are integers, `created_at` and `modified_at` RFC 3339 strings);
/// `in` takes an array of them, `like` a string, `is null` and `is not null` no value. Whether the
/// field exists and the operator fits it is checked by `forgebase-db` before anything is run.
pub fn parse_filters(doctype: &DocType, text: &str) -> Result<Vec<Filter>, JsonError> {
    let query = |problem: String| JsonError::Query(format!("invalid filter: {problem}"));
    let Json::Array(items) = serde_json::from_str::<Json>(text)? else {
        return Err(query("expected a JSON array of filters".to_owned()));
    };
    let mut filters = Vec::with_capacity(items.len());
    for (i, item) in items.iter().enumerate() {
        let place = |problem: String| query(format!("filter {}: {problem}", i + 1));
        let Json::Object(object) = item else {
            return Err(place(format!("expected an object, found {}", kind(item))));
        };
        if let Some(key) = object
            .keys()
            .find(|k| !matches!(k.as_str(), "field" | "op" | "value"))
        {
            return Err(place(format!("unknown key `{key}` (use field, op, value)")));
        }
        let field = object
            .get("field")
            .and_then(Json::as_str)
            .ok_or_else(|| place("`field` must be a string".to_owned()))?;
        let op = object
            .get("op")
            .and_then(Json::as_str)
            .ok_or_else(|| place("`op` must be a string".to_owned()))?;
        let value = object.get("value");
        let field_type = forgebase_db::column_type(doctype, field).ok_or_else(|| {
            place(format!(
                "unknown field {field:?} (table fields cannot be filtered)"
            ))
        })?;
        let one = || -> Result<Value, JsonError> {
            let json = value.ok_or_else(|| place(format!("operator `{op}` needs a `value`")))?;
            to_value(field, &field_type, json)
        };
        let op = match op {
            "=" => FilterOp::Eq(one()?),
            "!=" => FilterOp::Ne(one()?),
            "<" => FilterOp::Lt(one()?),
            "<=" => FilterOp::Le(one()?),
            ">" => FilterOp::Gt(one()?),
            ">=" => FilterOp::Ge(one()?),
            "in" => {
                let Some(Json::Array(values)) = value else {
                    return Err(place("operator `in` needs a `value` array".to_owned()));
                };
                FilterOp::In(
                    values
                        .iter()
                        .map(|json| to_value(field, &field_type, json))
                        .collect::<Result<_, _>>()?,
                )
            }
            "like" => match value {
                Some(Json::String(pattern)) => FilterOp::Like(pattern.clone()),
                _ => return Err(place("operator `like` needs a string `value`".to_owned())),
            },
            "is null" | "is not null" => {
                if value.is_some() {
                    return Err(place(format!("operator `{op}` takes no `value`")));
                }
                FilterOp::IsNull(op == "is null")
            }
            other => {
                return Err(place(format!(
                    "unknown operator `{other}` (use =, !=, <, <=, >, >=, in, like, is null, is not null)"
                )));
            }
        };
        filters.push(Filter::new(field, op));
    }
    Ok(filters)
}

/// Parses the sort values (`doc list --sort`, the comma separated `sort` query parameter): `FIELD`, `FIELD:asc` or `FIELD:desc`.
pub fn parse_sort(specs: &[String]) -> Result<Vec<Sort>, JsonError> {
    specs
        .iter()
        .map(|spec| match spec.split_once(':') {
            None => Ok(Sort::asc(spec.as_str())),
            Some((field, "asc")) => Ok(Sort::asc(field)),
            Some((field, "desc")) => Ok(Sort::desc(field)),
            Some(_) => Err(JsonError::Query(format!(
                "invalid sort {spec:?}: use FIELD, FIELD:asc or FIELD:desc"
            ))),
        })
        .collect()
}

/// A list of stored documents as a JSON array of [`document_to_json`] objects.
pub fn documents_to_json(docs: &[StoredDocument]) -> Result<Json, JsonError> {
    docs.iter()
        .map(document_to_json)
        .collect::<Result<Vec<_>, _>>()
        .map(Json::Array)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doctype() -> DocType {
        DocType::from_toml_str(
            r#"
name = "t"
label = "T"
[[fields]]
name = "d"
type = "data"
[[fields]]
name = "x"
type = "text"
[[fields]]
name = "i"
type = "int"
[[fields]]
name = "m"
type = "decimal"
precision = 12
scale = 2
[[fields]]
name = "c"
type = "check"
[[fields]]
name = "on"
type = "date"
[[fields]]
name = "at"
type = "datetime"
[[fields]]
name = "s"
type = "select"
options = ["a", "b"]
[[fields]]
name = "l"
type = "link"
target = "other"
"#,
        )
        .expect("valid DocType")
    }

    fn one(text: &str) -> Result<BTreeMap<String, Value>, JsonError> {
        parse_input(&Registry::new(), &doctype(), text).map(|input| input.values)
    }

    fn message(text: &str) -> String {
        one(text).unwrap_err().to_string()
    }

    #[test]
    fn maps_every_type() {
        let values = one(
            r#"{"d":"x","x":"y","i":-5,"m":"12.50","c":true,"on":"2024-03-15",
                "at":"2024-03-15T10:20:30.5+07:00","s":"a"}"#,
        )
        .unwrap();
        assert_eq!(values["d"], Value::Text("x".into()));
        assert_eq!(values["x"], Value::Text("y".into()));
        assert_eq!(values["i"], Value::Int(-5));
        assert_eq!(values["m"], Value::Decimal("12.50".parse().unwrap()));
        assert_eq!(values["c"], Value::Bool(true));
        assert!(matches!(values["on"], Value::Date(d) if d.to_string() == "2024-03-15"));
        let Value::Datetime(at) = values["at"] else {
            panic!("not a datetime")
        };
        assert_eq!(
            at,
            OffsetDateTime::parse("2024-03-15T03:20:30.5Z", &Rfc3339).unwrap()
        );
        assert_eq!(values["s"], Value::Text("a".into()));
    }

    #[test]
    fn links_are_target_names() {
        let values = one(r#"{"l":"ann@example.com"}"#).unwrap();
        assert_eq!(values["l"], Value::Text("ann@example.com".into()));
        assert!(message(r#"{"l":5}"#).contains("expected a string, found a number"));
        assert_eq!(one(r#"{"l":null}"#).unwrap()["l"], Value::Null);
        let doc = StoredDocument {
            id: "0192a3b4-c5d6-7e8f-9a0b-1c2d3e4f5a6b".parse().unwrap(),
            name: "n".into(),
            owner: "o".into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            modified_at: OffsetDateTime::UNIX_EPOCH,
            modified_by: "m".into(),
            version: 1,
            docstatus: None,
            amended_from: None,
            values: BTreeMap::from([("l".to_owned(), Value::Text("ann@example.com".into()))]),
            tables: BTreeMap::new(),
        };
        assert_eq!(document_to_json(&doc).unwrap()["l"], "ann@example.com");
    }

    #[test]
    fn null_maps_to_null_for_every_type() {
        let values = one(r#"{"d":null,"i":null,"m":null,"c":null,"on":null,"at":null}"#).unwrap();
        assert!(values.values().all(|v| *v == Value::Null));
        assert_eq!(values.len(), 6);
    }

    #[test]
    fn integers_are_exact() {
        let values = one(&format!("{{\"i\":{}}}", i64::MAX)).unwrap();
        assert_eq!(values["i"], Value::Int(i64::MAX));
        let values = one(&format!("{{\"i\":{}}}", i64::MIN)).unwrap();
        assert_eq!(values["i"], Value::Int(i64::MIN));
        for bad in [
            "1.5",
            "1.0",
            "1e3",
            "9223372036854775808",
            "-9223372036854775809",
            "\"5\"",
            "true",
        ] {
            let m = message(&format!("{{\"i\":{bad}}}"));
            assert!(m.contains("field `i`"), "{bad}: {m}");
        }
    }

    #[test]
    fn decimals_must_be_strings() {
        assert!(message(r#"{"m":12.5}"#).contains("JSON string"));
        assert!(message(r#"{"m":"abc"}"#).contains("not a decimal"));
        assert!(message(r#"{"m":true}"#).contains("expected a decimal string"));
        // Precision is kept exactly; scale rules are the validator's job.
        let values = one(r#"{"m":"0.1234567890123456789"}"#).unwrap();
        assert_eq!(
            values["m"],
            Value::Decimal("0.1234567890123456789".parse().unwrap())
        );
    }

    #[test]
    fn wrong_kinds_are_rejected() {
        assert!(message(r#"{"d":5}"#).contains("expected a string, found a number"));
        assert!(message(r#"{"c":"yes"}"#).contains("expected true or false"));
        assert!(message(r#"{"s":["a"]}"#).contains("found an array"));
        assert!(message(r#"{"x":{"a":1}}"#).contains("found an object"));
    }

    #[test]
    fn dates_and_datetimes_are_checked() {
        for bad in [
            "2024-3-5",
            "2024-02-30",
            "15/03/2024",
            "2024-03-15T00:00:00Z",
            "",
            "20240315",
        ] {
            assert!(
                message(&format!("{{\"on\":\"{bad}\"}}")).contains("YYYY-MM-DD"),
                "{bad}"
            );
        }
        assert!(message(r#"{"on":20240315}"#).contains("expected a date string"));
        for bad in [
            "2024-03-15",
            "2024-03-15 10:00:00",
            "2024-03-15T10:00:00",
            "yesterday",
        ] {
            assert!(
                message(&format!("{{\"at\":\"{bad}\"}}")).contains("RFC 3339"),
                "{bad}"
            );
        }
    }

    #[test]
    fn input_must_be_an_object() {
        assert!(matches!(one("[1]"), Err(JsonError::NotAnObject)));
        assert!(matches!(one("\"x\""), Err(JsonError::NotAnObject)));
        assert!(matches!(one("null"), Err(JsonError::NotAnObject)));
        assert!(matches!(one("{"), Err(JsonError::Syntax(_))));
        assert!(matches!(one(""), Err(JsonError::Syntax(_))));
    }

    #[test]
    fn unknown_keys_are_passed_through_for_validation() {
        let values = one(r#"{"nope":[1,2],"d":"x"}"#).unwrap();
        assert_eq!(values["nope"], Value::Null);
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn output_mapping() {
        let at = OffsetDateTime::parse("2024-03-15T17:20:30.123456+07:00", &Rfc3339).unwrap();
        let date = Date::parse("2024-03-05", &date_format().unwrap()).unwrap();
        let doc = StoredDocument {
            id: "0192a3b4-c5d6-7e8f-9a0b-1c2d3e4f5a6b".parse().unwrap(),
            name: "n".into(),
            owner: "o".into(),
            created_at: at,
            modified_at: at,
            modified_by: "m".into(),
            version: 3,
            docstatus: None,
            amended_from: None,
            values: BTreeMap::from([
                ("at".to_owned(), Value::Datetime(at)),
                ("on".to_owned(), Value::Date(date)),
                ("m".to_owned(), Value::Decimal("12.50".parse().unwrap())),
                ("i".to_owned(), Value::Int(i64::MIN)),
                ("c".to_owned(), Value::Bool(false)),
                ("d".to_owned(), Value::Null),
            ]),
            tables: BTreeMap::new(),
        };
        let json = document_to_json(&doc).unwrap();
        assert_eq!(json["id"], "0192a3b4-c5d6-7e8f-9a0b-1c2d3e4f5a6b");
        assert_eq!(json["version"], 3);
        assert_eq!(json["created_at"], "2024-03-15T10:20:30.123456Z");
        assert_eq!(json["at"], "2024-03-15T10:20:30.123456Z");
        assert_eq!(json["on"], "2024-03-05");
        assert_eq!(json["m"], "12.50");
        assert_eq!(json["i"], i64::MIN);
        assert_eq!(json["c"], false);
        assert!(json["d"].is_null());
        // Output parses back to the same values.
        let round = parse_input(&Registry::new(), &doctype(), &json.to_string()).map(|i| i.values);
        // Standard columns are not fields, so they come back as unknown (null) keys only.
        let round = round.unwrap();
        assert_eq!(round["at"], Value::Datetime(at));
        assert_eq!(round["on"], Value::Date(date));
        assert_eq!(round["m"], Value::Decimal("12.50".parse().unwrap()));
        assert_eq!(round["i"], Value::Int(i64::MIN));
    }

    fn order_registry() -> (Registry, DocType) {
        let mut registry = Registry::new();
        for text in [
            "name = \"line\"\nlabel = \"L\"\nis_child = true\n[[fields]]\nname = \"qty\"\ntype = \"int\"\n\
             [[fields]]\nname = \"price\"\ntype = \"decimal\"\nprecision = 10\nscale = 2\n",
            "name = \"ord\"\nlabel = \"O\"\n[[fields]]\nname = \"title\"\ntype = \"data\"\n\
             [[fields]]\nname = \"lines\"\ntype = \"table\"\ntarget = \"line\"\n",
        ] {
            registry.add(DocType::from_toml_str(text).unwrap()).unwrap();
        }
        let ord = registry.get("ord").unwrap().clone();
        (registry, ord)
    }

    #[test]
    fn table_fields_are_arrays_of_row_objects() {
        let (registry, ord) = order_registry();
        let input = parse_input(
            &registry,
            &ord,
            r#"{"title":"T","lines":[{"qty":1,"price":"2.50"},{"qty":2},{}]}"#,
        )
        .unwrap();
        assert_eq!(input.values.len(), 1);
        let rows = &input.tables["lines"];
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["qty"], Value::Int(1));
        assert_eq!(rows[0]["price"], Value::Decimal("2.50".parse().unwrap()));
        assert!(rows[2].is_empty());
        // null and [] are both "no rows"; unknown row keys are passed on for validation.
        for text in [r#"{"lines":null}"#, r#"{"lines":[]}"#] {
            let input = parse_input(&registry, &ord, text).unwrap();
            assert_eq!(input.tables["lines"].len(), 0, "{text}");
        }
        let input = parse_input(&registry, &ord, r#"{"lines":[{"nope":1}]}"#).unwrap();
        assert_eq!(input.tables["lines"][0]["nope"], Value::Null);
    }

    #[test]
    fn table_input_errors_name_the_field_and_the_row() {
        let (registry, ord) = order_registry();
        let message = |text: &str| parse_input(&registry, &ord, text).unwrap_err().to_string();
        assert_eq!(
            message(r#"{"lines":"x"}"#),
            "field `lines`: expected an array of rows, found a string"
        );
        assert_eq!(
            message(r#"{"lines":{"qty":1}}"#),
            "field `lines`: expected an array of rows, found an object"
        );
        assert_eq!(
            message(r#"{"lines":[{"qty":1},5]}"#),
            "field `lines`: row 2: expected an object, found a number"
        );
        assert_eq!(
            message(r#"{"lines":[{"qty":1},{"qty":"two"}]}"#),
            "field `lines`: row 2: field `qty`: expected an integer, found a string"
        );
        assert!(message(r#"{"lines":[{"price":1.5}]}"#).contains("row 1: field `price`"));
    }

    #[test]
    fn a_child_table_in_the_registry_is_needed_for_rows() {
        let (_, ord) = order_registry();
        let error = parse_input(&Registry::new(), &ord, r#"{"lines":[{}]}"#).unwrap_err();
        assert!(
            error.to_string().contains("unknown child doctype `line`"),
            "{error}"
        );
        // But without rows nothing is looked up.
        assert!(parse_input(&Registry::new(), &ord, r#"{"lines":null}"#).is_ok());
    }

    #[test]
    fn table_output_is_an_array_of_row_objects_in_order() {
        let rows = vec![
            BTreeMap::from([
                ("qty".to_owned(), Value::Int(1)),
                ("price".to_owned(), Value::Decimal("2.50".parse().unwrap())),
            ]),
            BTreeMap::from([
                ("qty".to_owned(), Value::Null),
                ("price".to_owned(), Value::Null),
            ]),
        ];
        let doc = StoredDocument {
            id: "0192a3b4-c5d6-7e8f-9a0b-1c2d3e4f5a6b".parse().unwrap(),
            name: "n".into(),
            owner: "o".into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            modified_at: OffsetDateTime::UNIX_EPOCH,
            modified_by: "m".into(),
            version: 1,
            docstatus: None,
            amended_from: None,
            values: BTreeMap::new(),
            tables: BTreeMap::from([("lines".to_owned(), rows), ("empty".to_owned(), Vec::new())]),
        };
        let json = document_to_json(&doc).unwrap();
        assert_eq!(
            json["lines"],
            serde_json::json!([{"price": "2.50", "qty": 1}, {"price": null, "qty": null}])
        );
        assert_eq!(json["empty"], serde_json::json!([]));
    }

    #[test]
    fn extreme_datetimes_never_panic() {
        // Valid RFC 3339 whose UTC instant is outside the range of `time`: parsing succeeds,
        // writing it back as UTC must be an error, not a panic.
        let edge = OffsetDateTime::parse("9999-12-31T23:00:00-05:00", &Rfc3339).unwrap();
        assert!(matches!(
            from_value("at", &Value::Datetime(edge)),
            Err(JsonError::Output { .. })
        ));
        let ok = OffsetDateTime::parse("9999-12-31T23:59:59Z", &Rfc3339).unwrap();
        assert!(from_value("at", &Value::Datetime(ok)).is_ok());
    }

    #[test]
    fn docstatus_and_amended_from_are_output_for_submittable_doctypes_only() {
        use forgebase_core::document::DocStatus;
        let at = OffsetDateTime::UNIX_EPOCH;
        let stored = |docstatus, amended_from: Option<&str>| StoredDocument {
            id: "0192a3b4-c5d6-7e8f-9a0b-1c2d3e4f5a6b".parse().unwrap(),
            name: "X-1".to_owned(),
            owner: "o".to_owned(),
            created_at: at,
            modified_at: at,
            modified_by: "m".to_owned(),
            version: 3,
            docstatus,
            amended_from: amended_from.map(str::to_owned),
            values: BTreeMap::new(),
            tables: BTreeMap::new(),
        };
        let json = document_to_json(&stored(Some(DocStatus::Cancelled), Some("X"))).unwrap();
        assert_eq!(json["docstatus"], 2);
        assert_eq!(json["amended_from"], "X");
        let json = document_to_json(&stored(Some(DocStatus::Draft), None)).unwrap();
        assert_eq!(json["docstatus"], 0);
        assert_eq!(json["amended_from"], Json::Null);
        let json = document_to_json(&stored(None, None)).unwrap();
        assert!(json.get("docstatus").is_none() && json.get("amended_from").is_none());
    }
}
