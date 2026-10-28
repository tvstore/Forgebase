//! Documents and their validation against a [`DocType`].

use std::collections::BTreeMap;

use crate::meta::{DocField, DocType, FieldType};
use crate::value::Value;

/// A document: an optional name plus field values keyed by field name.
///
/// A missing key and [`Value::Null`] both mean "no value".
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document {
    /// The document name, once assigned.
    pub name: Option<String>,
    /// Field values.
    pub values: BTreeMap<String, Value>,
}

/// One problem found while validating a document.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// The document has a value for a field the DocType does not define.
    #[error("unknown field `{field}`")]
    UnknownField {
        /// The field.
        field: String,
    },
    /// A required field is missing, null, or (Text, Data, Select) an empty or whitespace-only string.
    #[error("field `{field}` is required")]
    Required {
        /// The field.
        field: String,
    },
    /// The value has the wrong kind for the field type.
    #[error("field `{field}`: expected {expected}, found {found}")]
    TypeMismatch {
        /// The field.
        field: String,
        /// Kind of value the field type accepts.
        expected: &'static str,
        /// Kind of value that was given.
        found: &'static str,
    },
    /// A Data value is longer than `max_length` characters.
    #[error("field `{field}`: {len} characters exceeds max_length {max}")]
    TooLong {
        /// The field.
        field: String,
        /// Length in characters.
        len: usize,
        /// The limit.
        max: u32,
    },
    /// A Select value is not one of the options.
    #[error("field `{field}`: `{value}` is not one of the allowed options")]
    InvalidOption {
        /// The field.
        field: String,
        /// The rejected value.
        value: String,
    },
    /// A text value contains a NUL character, which PostgreSQL text cannot store.
    #[error("field `{field}`: value must not contain a NUL character")]
    ContainsNul {
        /// The field.
        field: String,
    },
    /// A Decimal value has more fractional digits than the field's scale.
    #[error("field `{field}`: {scale} decimal places exceeds scale {max}")]
    DecimalScale {
        /// The field.
        field: String,
        /// Fractional digits in the value (trailing zeros ignored).
        scale: u32,
        /// The field's scale.
        max: u8,
    },
    /// A Decimal value has more integer digits than `precision - scale` allows.
    #[error("field `{field}`: {digits} integer digits exceeds the maximum of {max}")]
    DecimalPrecision {
        /// The field.
        field: String,
        /// Integer digits in the value.
        digits: u32,
        /// Allowed integer digits (`precision - scale`).
        max: u8,
    },
}

/// Validates a complete document for insert.
///
/// Reports every problem: unknown fields, missing or null required fields, wrong value kinds
/// and constraint violations. `doc.name` is not checked.
///
/// A required Text, Data or Select field counts as missing when it is absent, null, or an
/// empty or whitespace-only string. Text, Data and Select values must not contain NUL.
///
/// `doctype` must be valid ([`DocType::validate`]); a [`Registry`](crate::meta::Registry)
/// only holds valid DocTypes. The result for an invalid DocType is unspecified (but never a panic).
pub fn validate(doctype: &DocType, doc: &Document) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    for field in &doctype.fields {
        // An explicit null is reported by `check_values`.
        if field.required && !doc.values.contains_key(&field.name) {
            errors.push(ValidationError::Required {
                field: field.name.clone(),
            });
        }
    }
    check_values(doctype, &doc.values, &mut errors);
    finish(errors)
}

/// Validates a partial update: only the given fields are checked.
///
/// Fields that are not in `changes` are left alone, so required fields need not be present,
/// but a required field may not be set to [`Value::Null`] or, for Text, Data and Select, to an
/// empty or whitespace-only string (see [`validate`]).
///
/// `doctype` must be valid ([`DocType::validate`]); a [`Registry`](crate::meta::Registry)
/// only holds valid DocTypes.
pub fn validate_update(
    doctype: &DocType,
    changes: &BTreeMap<String, Value>,
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    check_values(doctype, changes, &mut errors);
    finish(errors)
}

fn finish(errors: Vec<ValidationError>) -> Result<(), Vec<ValidationError>> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check_values(
    doctype: &DocType,
    values: &BTreeMap<String, Value>,
    errors: &mut Vec<ValidationError>,
) {
    for (name, value) in values {
        match doctype.field(name) {
            None => errors.push(ValidationError::UnknownField {
                field: name.clone(),
            }),
            Some(field) => check_value(field, value, errors),
        }
    }
}

fn check_value(field: &DocField, value: &Value, errors: &mut Vec<ValidationError>) {
    let name = || field.name.clone();
    if matches!(value, Value::Null) {
        if field.required {
            errors.push(ValidationError::Required { field: name() });
        }
        return;
    }
    if let (FieldType::Data { .. } | FieldType::Text | FieldType::Select { .. }, Value::Text(s)) =
        (&field.field_type, value)
    {
        if s.contains('\0') {
            errors.push(ValidationError::ContainsNul { field: name() });
            return;
        }
        if field.required && s.trim().is_empty() {
            errors.push(ValidationError::Required { field: name() });
            return;
        }
    }
    match (&field.field_type, value) {
        (FieldType::Data { max_length }, Value::Text(s)) => {
            let len = s.chars().count();
            if len > *max_length as usize {
                errors.push(ValidationError::TooLong {
                    field: name(),
                    len,
                    max: *max_length,
                });
            }
        }
        (FieldType::Select { options }, Value::Text(s)) => {
            if !options.contains(s) {
                errors.push(ValidationError::InvalidOption {
                    field: name(),
                    value: s.clone(),
                });
            }
        }
        (FieldType::Decimal { precision, scale }, Value::Decimal(d)) => {
            // Trailing zeros do not count: 1.500 fits scale 1.
            let d = d.normalize();
            if d.scale() > u32::from(*scale) {
                errors.push(ValidationError::DecimalScale {
                    field: name(),
                    scale: d.scale(),
                    max: *scale,
                });
            } else {
                let total = d
                    .mantissa()
                    .unsigned_abs()
                    .checked_ilog10()
                    .map_or(0, |l| l + 1);
                let digits = total.saturating_sub(d.scale());
                let max = precision.saturating_sub(*scale);
                if digits > u32::from(max) {
                    errors.push(ValidationError::DecimalPrecision {
                        field: name(),
                        digits,
                        max,
                    });
                }
            }
        }
        (FieldType::Text, Value::Text(_))
        | (FieldType::Int, Value::Int(_))
        | (FieldType::Check, Value::Bool(_))
        | (FieldType::Date, Value::Date(_))
        | (FieldType::Datetime, Value::Datetime(_)) => {}
        (field_type, value) => errors.push(ValidationError::TypeMismatch {
            field: name(),
            expected: expected_kind(field_type),
            found: value.kind(),
        }),
    }
}

fn expected_kind(field_type: &FieldType) -> &'static str {
    match field_type {
        FieldType::Data { .. } | FieldType::Text | FieldType::Select { .. } => "text",
        FieldType::Int => "int",
        FieldType::Decimal { .. } => "decimal",
        FieldType::Check => "bool",
        FieldType::Date => "date",
        FieldType::Datetime => "datetime",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::Naming;
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use time::{Date, Month, OffsetDateTime};

    fn dec(s: &str) -> Value {
        Value::Decimal(Decimal::from_str(s).unwrap())
    }

    fn text(s: &str) -> Value {
        Value::Text(s.into())
    }

    fn date() -> Value {
        Value::Date(Date::from_calendar_date(2026, Month::October, 28).unwrap())
    }

    fn f(name: &str, field_type: FieldType, required: bool) -> DocField {
        DocField {
            name: name.into(),
            label: name.into(),
            field_type,
            required,
            unique: false,
        }
    }

    fn doctype() -> DocType {
        DocType {
            name: "thing".into(),
            label: "Thing".into(),
            naming: Naming::Random,
            fields: vec![
                f("title", FieldType::Data { max_length: 5 }, true),
                f("notes", FieldType::Text, false),
                f("count", FieldType::Int, false),
                f(
                    "amount",
                    FieldType::Decimal {
                        precision: 5,
                        scale: 2,
                    },
                    false,
                ),
                f("active", FieldType::Check, false),
                f("day", FieldType::Date, false),
                f("at", FieldType::Datetime, false),
                f(
                    "kind",
                    FieldType::Select {
                        options: vec!["a".into(), "b".into()],
                    },
                    false,
                ),
            ],
        }
    }

    fn doc(pairs: Vec<(&str, Value)>) -> Document {
        let mut d = Document::default();
        d.values.insert("title".into(), text("ok"));
        for (k, v) in pairs {
            d.values.insert(k.into(), v);
        }
        d
    }

    fn errs(d: &Document) -> Vec<ValidationError> {
        validate(&doctype(), d).expect_err("expected errors")
    }

    #[test]
    fn valid_document_passes() {
        let d = doc(vec![
            ("notes", text(&"x".repeat(100_000))),
            ("count", Value::Int(i64::MIN)),
            ("amount", dec("123.45")),
            ("active", Value::Bool(false)),
            ("day", date()),
            ("at", Value::Datetime(OffsetDateTime::UNIX_EPOCH)),
            ("kind", text("b")),
        ]);
        assert_eq!(validate(&doctype(), &d), Ok(()));
    }

    #[test]
    fn optional_fields_may_be_null_or_absent() {
        let d = doc(vec![("count", Value::Null), ("kind", Value::Null)]);
        assert_eq!(validate(&doctype(), &d), Ok(()));
    }

    #[test]
    fn required_missing_or_null() {
        let mut d = Document::default();
        assert_eq!(
            errs(&d),
            vec![ValidationError::Required {
                field: "title".into()
            }]
        );
        d.values.insert("title".into(), Value::Null);
        assert_eq!(
            errs(&d),
            vec![ValidationError::Required {
                field: "title".into()
            }]
        );
    }

    #[test]
    fn required_rejects_empty_and_whitespace_text() {
        let mut dt = doctype();
        dt.fields.push(f("body", FieldType::Text, true));
        dt.fields.push(f(
            "stage",
            FieldType::Select {
                options: vec!["a".into()],
            },
            true,
        ));
        for blank in ["", "   ", "\t\n "] {
            for field in ["title", "body", "stage"] {
                let mut d = Document::default();
                d.values.insert("title".into(), text("ok"));
                d.values.insert("body".into(), text("x"));
                d.values.insert("stage".into(), text("a"));
                d.values.insert(field.into(), text(blank));
                assert_eq!(
                    validate(&dt, &d),
                    Err(vec![ValidationError::Required {
                        field: field.into()
                    }]),
                    "{field} = {blank:?}"
                );
                let changes = BTreeMap::from([(field.to_string(), text(blank))]);
                assert_eq!(
                    validate_update(&dt, &changes),
                    Err(vec![ValidationError::Required {
                        field: field.into()
                    }]),
                    "update {field} = {blank:?}"
                );
            }
        }
        // Optional fields may hold blank text.
        let d = doc(vec![
            ("body", text("x")),
            ("stage", text("a")),
            ("notes", text("  ")),
        ]);
        assert_eq!(validate(&dt, &d), Ok(()));
    }

    #[test]
    fn nul_is_rejected_in_text_data_and_select() {
        for (field, value) in [("title", "a\0b"), ("notes", "a\0"), ("kind", "\0")] {
            let d = doc(vec![(field, text(value))]);
            assert_eq!(
                errs(&d),
                vec![ValidationError::ContainsNul {
                    field: field.into()
                }],
                "{field}"
            );
        }
        let changes = BTreeMap::from([("notes".to_string(), text("a\0b"))]);
        assert_eq!(
            validate_update(&doctype(), &changes),
            Err(vec![ValidationError::ContainsNul {
                field: "notes".into()
            }])
        );
    }

    #[test]
    fn invalid_decimal_definition_does_not_panic() {
        let mut dt = doctype();
        dt.fields = vec![f(
            "amount",
            FieldType::Decimal {
                precision: 2,
                scale: 3,
            },
            false,
        )];
        let changes = BTreeMap::from([("amount".to_string(), dec("0.001"))]);
        // The result is unspecified for an invalid DocType; it must only not panic.
        let _ = validate_update(&dt, &changes);
    }

    #[test]
    fn unknown_field() {
        let d = doc(vec![("nope", Value::Int(1))]);
        assert_eq!(
            errs(&d),
            vec![ValidationError::UnknownField {
                field: "nope".into()
            }]
        );
    }

    #[test]
    fn type_mismatches() {
        let cases: Vec<(&str, Value, &str, &str)> = vec![
            ("notes", Value::Int(1), "text", "int"),
            ("count", text("1"), "int", "text"),
            ("count", dec("1"), "int", "decimal"),
            ("amount", Value::Int(1), "decimal", "int"),
            ("active", Value::Int(1), "bool", "int"),
            ("day", text("2026-10-28"), "date", "text"),
            (
                "day",
                Value::Datetime(OffsetDateTime::UNIX_EPOCH),
                "date",
                "datetime",
            ),
            ("at", date(), "datetime", "date"),
            ("kind", Value::Int(1), "text", "int"),
            ("title", Value::Bool(true), "text", "bool"),
        ];
        for (field, value, expected, found) in cases {
            let d = doc(vec![(field, value)]);
            assert_eq!(
                errs(&d),
                vec![ValidationError::TypeMismatch {
                    field: field.into(),
                    expected,
                    found
                }],
                "field {field}"
            );
        }
    }

    #[test]
    fn data_max_length_counts_characters() {
        let ok = doc(vec![("title", text("12345"))]);
        assert_eq!(validate(&doctype(), &ok), Ok(()));
        // Five multi-byte characters are 10 bytes but still within the limit.
        let multibyte = doc(vec![("title", text("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"))]);
        assert_eq!(validate(&doctype(), &multibyte), Ok(()));
        let long = doc(vec![("title", text("123456"))]);
        assert_eq!(
            errs(&long),
            vec![ValidationError::TooLong {
                field: "title".into(),
                len: 6,
                max: 5
            }]
        );
    }

    #[test]
    fn select_membership() {
        assert_eq!(
            validate(&doctype(), &doc(vec![("kind", text("a"))])),
            Ok(())
        );
        assert_eq!(
            errs(&doc(vec![("kind", text("c"))])),
            vec![ValidationError::InvalidOption {
                field: "kind".into(),
                value: "c".into()
            }]
        );
        // Case matters.
        assert_eq!(errs(&doc(vec![("kind", text("A"))])).len(), 1);
    }

    #[test]
    fn decimal_precision_and_scale() {
        // precision 5, scale 2: up to 3 integer digits and 2 decimal places.
        for ok in [
            "0", "0.5", "-0.01", "999.99", "-999.99", "1.5", "1.500", "100", "0.00", "12.30000",
        ] {
            assert_eq!(
                validate(&doctype(), &doc(vec![("amount", dec(ok))])),
                Ok(()),
                "{ok}"
            );
        }
        let scale_cases = [("1.234", 3), ("0.001", 3)];
        for (v, scale) in scale_cases {
            assert_eq!(
                errs(&doc(vec![("amount", dec(v))])),
                vec![ValidationError::DecimalScale {
                    field: "amount".into(),
                    scale,
                    max: 2
                }],
                "{v}"
            );
        }
        for (v, digits) in [("1000", 4), ("-1000.5", 4), ("12345.67", 5)] {
            assert_eq!(
                errs(&doc(vec![("amount", dec(v))])),
                vec![ValidationError::DecimalPrecision {
                    field: "amount".into(),
                    digits,
                    max: 3
                }],
                "{v}"
            );
        }
    }

    #[test]
    fn decimal_with_scale_equal_to_precision() {
        let mut d = doctype();
        d.fields.push(f(
            "ratio",
            FieldType::Decimal {
                precision: 3,
                scale: 3,
            },
            false,
        ));
        for ok in ["0", "0.999", "-0.5"] {
            let doc = doc(vec![("ratio", dec(ok))]);
            assert_eq!(validate(&d, &doc), Ok(()), "{ok}");
        }
        let doc = doc(vec![("ratio", dec("1"))]);
        assert_eq!(
            validate(&d, &doc),
            Err(vec![ValidationError::DecimalPrecision {
                field: "ratio".into(),
                digits: 1,
                max: 0
            }])
        );
    }

    #[test]
    fn all_errors_are_collected() {
        let mut d = Document::default();
        d.values.insert("nope".into(), Value::Null);
        d.values.insert("count".into(), text("x"));
        assert_eq!(errs(&d).len(), 3);
    }

    #[test]
    fn partial_update() {
        let dt = doctype();
        let mut changes = BTreeMap::new();
        // Required `title` need not be present.
        changes.insert("count".to_owned(), Value::Int(3));
        assert_eq!(validate_update(&dt, &changes), Ok(()));
        // Optional fields may be cleared.
        changes.insert("kind".to_owned(), Value::Null);
        assert_eq!(validate_update(&dt, &changes), Ok(()));
        // Empty update is fine.
        assert_eq!(validate_update(&dt, &BTreeMap::new()), Ok(()));
    }

    #[test]
    fn partial_update_errors() {
        let dt = doctype();
        let mut changes = BTreeMap::new();
        changes.insert("title".to_owned(), Value::Null);
        changes.insert("kind".to_owned(), text("z"));
        changes.insert("nope".to_owned(), Value::Int(1));
        assert_eq!(
            validate_update(&dt, &changes),
            Err(vec![
                ValidationError::InvalidOption {
                    field: "kind".into(),
                    value: "z".into()
                },
                ValidationError::UnknownField {
                    field: "nope".into()
                },
                ValidationError::Required {
                    field: "title".into()
                },
            ])
        );
    }
}
