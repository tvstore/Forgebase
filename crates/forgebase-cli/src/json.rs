//! JSON text <-> [`Value`] mapping for the CLI.
//!
//! | Field type | JSON |
//! |---|---|
//! | Data, Text, Select | string |
//! | Int | integer (a float, or an integer outside the `i64` range, is rejected) |
//! | Decimal | **string**, for example `"1234.50"` (a JSON number is rejected so no precision is lost to floats); every form `rust_decimal` parses is accepted with its exact value, for example `"1e3"`, `"+12.5"`, `".5"` |
//! | Check | `true` / `false` |
//! | Date | string `"YYYY-MM-DD"` |
//! | Datetime | RFC 3339 string; output is always UTC with `Z`. Microsecond precision (PostgreSQL `timestamptz`): finer digits are truncated, not rounded or rejected |
//! | any | `null` for no value |
//!
//! The input must be a JSON object. A duplicate key is accepted and the last one wins (serde_json
//! behavior). Output keys are sorted alphabetically. Values stored by other tools outside the
//! supported ranges (years beyond ±9999, `infinity`, `NaN`) cannot be read; see
//! [`forgebase_db::DbError::Decode`]. Keys that are not fields of the DocType are passed on (with
//! a null value) so that core validation reports them together with the other problems.
//! This lives in the CLI for now and moves to a shared place when the HTTP API arrives (M3).

use std::collections::BTreeMap;

use forgebase_core::meta::{DocType, FieldType};
use forgebase_core::value::Value;
use forgebase_db::StoredDocument;
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

/// Parses JSON text into field values for `doctype`.
pub fn parse_values(doctype: &DocType, text: &str) -> Result<BTreeMap<String, Value>, JsonError> {
    let Json::Object(object) = serde_json::from_str::<Json>(text)? else {
        return Err(JsonError::NotAnObject);
    };
    let mut values = BTreeMap::new();
    for (name, json) in &object {
        let value = match doctype.field(name) {
            Some(field) => to_value(name, &field.field_type, json)?,
            // Reported by validation as an unknown field.
            None => Value::Null,
        };
        values.insert(name.clone(), value);
    }
    Ok(values)
}

fn to_value(name: &str, field_type: &FieldType, json: &Json) -> Result<Value, JsonError> {
    if json.is_null() {
        return Ok(Value::Null);
    }
    let expected = |what: &str| bad(name, format!("expected {what}, found {}", kind(json)));
    match field_type {
        FieldType::Data { .. } | FieldType::Text | FieldType::Select { .. } => json
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

/// A stored document as a JSON object: the standard columns plus the field values.
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
    for (name, value) in &doc.values {
        object.insert(name.clone(), from_value(name, value)?);
    }
    Ok(Json::Object(object))
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
"#,
        )
        .expect("valid DocType")
    }

    fn one(text: &str) -> Result<BTreeMap<String, Value>, JsonError> {
        parse_values(&doctype(), text)
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
            values: BTreeMap::from([
                ("at".to_owned(), Value::Datetime(at)),
                ("on".to_owned(), Value::Date(date)),
                ("m".to_owned(), Value::Decimal("12.50".parse().unwrap())),
                ("i".to_owned(), Value::Int(i64::MIN)),
                ("c".to_owned(), Value::Bool(false)),
                ("d".to_owned(), Value::Null),
            ]),
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
        let round = parse_values(&doctype(), &json.to_string());
        // Standard columns are not fields, so they come back as unknown (null) keys only.
        let round = round.unwrap();
        assert_eq!(round["at"], Value::Datetime(at));
        assert_eq!(round["on"], Value::Date(date));
        assert_eq!(round["m"], Value::Decimal("12.50".parse().unwrap()));
        assert_eq!(round["i"], Value::Int(i64::MIN));
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
}
