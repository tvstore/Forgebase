//! Documents and their validation against a [`DocType`].

use std::collections::BTreeMap;
use std::fmt;

use crate::meta::{DocField, DocType, FieldType, Registry};
use crate::value::Value;

/// One row of a child table: field values keyed by field name, like [`Document::values`].
pub type Row = BTreeMap<String, Value>;

/// The state of a document of a submittable DocType (`submittable = true`), stored as the
/// `docstatus` column: 0, 1 or 2.
///
/// A draft is editable and deletable; a submitted document is locked (only fields marked
/// `allow_on_submit` change) and cannot be deleted; a cancelled document can be deleted or
/// amended into a new draft. Documents of other DocTypes have no docstatus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocStatus {
    /// 0: being edited.
    Draft,
    /// 1: submitted, locked.
    Submitted,
    /// 2: cancelled after submission.
    Cancelled,
}

impl DocStatus {
    /// The value of the `docstatus` column.
    pub fn as_i16(self) -> i16 {
        match self {
            DocStatus::Draft => 0,
            DocStatus::Submitted => 1,
            DocStatus::Cancelled => 2,
        }
    }

    /// The status for a `docstatus` column value, `None` for anything but 0, 1 and 2.
    pub fn from_i16(value: i16) -> Option<Self> {
        match value {
            0 => Some(DocStatus::Draft),
            1 => Some(DocStatus::Submitted),
            2 => Some(DocStatus::Cancelled),
            _ => None,
        }
    }
}

impl fmt::Display for DocStatus {
    /// `draft`, `submitted` or `cancelled`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            DocStatus::Draft => "draft",
            DocStatus::Submitted => "submitted",
            DocStatus::Cancelled => "cancelled",
        })
    }
}

/// A document: an optional name plus field values keyed by field name.
///
/// A missing key and [`Value::Null`] both mean "no value".
///
/// Plain data with public fields, on purpose (ADR 0012). Build one with
/// `Document { name: .., ..Default::default() }` so that code keeps compiling if a field is added.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Document {
    /// The document name, once assigned.
    pub name: Option<String>,
    /// Field values. A Table field has no value here; its rows are in [`Document::tables`].
    pub values: BTreeMap<String, Value>,
    /// The rows of each Table field, keyed by field name. A row's position in the list is its
    /// order (`idx`, counting from 1); an absent key and an empty list both mean "no rows".
    pub tables: BTreeMap<String, Vec<Row>>,
}

/// A partial update: the fields to change.
///
/// A field in [`Changes::values`] is set (a [`Value::Null`] clears it). A Table field in
/// [`Changes::tables`] **replaces its rows** with the given ones (an empty list removes every
/// row; `forgebase_db::update` leaves a table alone when the rows equal the stored ones); a Table
/// field that is not mentioned keeps its rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Changes {
    /// New field values.
    pub values: BTreeMap<String, Value>,
    /// New rows for Table fields, replacing the existing ones.
    pub tables: BTreeMap<String, Vec<Row>>,
}

impl Changes {
    /// True when nothing is to be changed.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && self.tables.is_empty()
    }
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
    /// A caller supplied a value for a `read_only` field. Not produced by [`validate`] (a hook
    /// may set such a field): `forgebase-db` reports it for the caller's input, before the hooks.
    #[error("field `{field}` is read-only")]
    ReadOnly {
        /// The field.
        field: String,
    },
    /// A Link value names a document that does not exist. Not produced by [`validate`] (which
    /// does no I/O): `forgebase-db` reports it when it resolves the link.
    #[error("field `{field}`: {target} `{name}` does not exist")]
    LinkNotFound {
        /// The field.
        field: String,
        /// The target DocType.
        target: String,
        /// The document name that was not found.
        name: String,
    },
    /// A row of a Table field has a problem. `row` counts from 1, like the row's position.
    #[error("field `{field}` row {row}: {error}")]
    InRow {
        /// The Table field.
        field: String,
        /// The position of the row, starting at 1.
        row: usize,
        /// The problem, about a field of the child DocType.
        error: Box<ValidationError>,
    },
}

impl ValidationError {
    /// The field this problem is about. For a problem in a row, the Table field.
    pub fn field(&self) -> &str {
        match self {
            ValidationError::UnknownField { field }
            | ValidationError::Required { field }
            | ValidationError::TypeMismatch { field, .. }
            | ValidationError::TooLong { field, .. }
            | ValidationError::InvalidOption { field, .. }
            | ValidationError::ContainsNul { field }
            | ValidationError::ReadOnly { field }
            | ValidationError::DecimalScale { field, .. }
            | ValidationError::DecimalPrecision { field, .. }
            | ValidationError::LinkNotFound { field, .. }
            | ValidationError::InRow { field, .. } => field,
        }
    }
}

/// Validates a complete document for insert.
///
/// Reports every problem: unknown fields, missing or null required fields, wrong value kinds
/// and constraint violations. `doc.name` is not checked.
///
/// A required Text, Data, Select or Link field counts as missing when it is absent, null, or
/// an empty or whitespace-only string. Text, Data, Select and Link values must not contain NUL.
/// Whether a Link target exists is not checked here (no I/O); `forgebase-db` does that.
///
/// **Tables.** A Table field takes its rows from [`Document::tables`], never from
/// [`Document::values`] (a value there is a [`ValidationError::TypeMismatch`], and so is a row
/// list for a field that is not a Table field). A required Table field needs at least one row.
/// Each row is validated against the child DocType (found in `registry`) like a document:
/// unknown fields, missing required fields, value kinds and constraints; its problems are
/// wrapped in [`ValidationError::InRow`] with the row's position. A table whose child DocType
/// is not in `registry` is not checked (a registry that passed
/// [`Registry::check_references`] has it).
///
/// `doctype` must be valid ([`DocType::validate`]); a [`Registry`] only holds valid DocTypes.
/// The result for an invalid DocType is unspecified (but never a panic).
pub fn validate(
    registry: &Registry,
    doctype: &DocType,
    doc: &Document,
) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();
    for field in &doctype.fields {
        let present = match field.field_type {
            FieldType::Table { .. } => doc.tables.contains_key(&field.name),
            // An explicit null is reported by `check_values`.
            _ => doc.values.contains_key(&field.name),
        };
        if field.required && !present {
            errors.push(ValidationError::Required {
                field: field.name.clone(),
            });
        }
    }
    check_values(doctype, &doc.values, &mut errors);
    check_tables(registry, doctype, &doc.tables, &mut errors);
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
            // A Table field has no value; its rows go in `tables`.
            Some(field) if matches!(field.field_type, FieldType::Table { .. }) => {
                errors.push(ValidationError::TypeMismatch {
                    field: name.clone(),
                    expected: "rows",
                    found: value.kind(),
                });
            }
            Some(field) => check_value(field, value, errors),
        }
    }
}

fn check_tables(
    registry: &Registry,
    doctype: &DocType,
    tables: &BTreeMap<String, Vec<Row>>,
    errors: &mut Vec<ValidationError>,
) {
    for (name, rows) in tables {
        let field = match doctype.field(name) {
            None => {
                errors.push(ValidationError::UnknownField {
                    field: name.clone(),
                });
                continue;
            }
            Some(field) => field,
        };
        let FieldType::Table { target } = &field.field_type else {
            errors.push(ValidationError::TypeMismatch {
                field: name.clone(),
                expected: expected_kind(&field.field_type),
                found: "rows",
            });
            continue;
        };
        if field.required && rows.is_empty() {
            errors.push(ValidationError::Required {
                field: name.clone(),
            });
        }
        let Some(child) = registry.get(target) else {
            continue;
        };
        for (i, row) in rows.iter().enumerate() {
            let mut row_errors = Vec::new();
            for child_field in &child.fields {
                if child_field.required && !row.contains_key(&child_field.name) {
                    row_errors.push(ValidationError::Required {
                        field: child_field.name.clone(),
                    });
                }
            }
            check_values(child, row, &mut row_errors);
            errors.extend(row_errors.into_iter().map(|error| ValidationError::InRow {
                field: name.clone(),
                row: i + 1,
                error: Box::new(error),
            }));
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
    if let (
        FieldType::Data { .. }
        | FieldType::Text
        | FieldType::Select { .. }
        | FieldType::Link { .. },
        Value::Text(s),
    ) = (&field.field_type, value)
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
        (FieldType::Text | FieldType::Link { .. }, Value::Text(_))
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
        FieldType::Data { .. }
        | FieldType::Text
        | FieldType::Select { .. }
        | FieldType::Link { .. } => "text",
        FieldType::Int => "int",
        FieldType::Decimal { .. } => "decimal",
        FieldType::Check => "bool",
        FieldType::Date => "date",
        FieldType::Datetime => "datetime",
        FieldType::Table { .. } => "rows",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{Naming, Registry};
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
            allow_on_submit: false,
            read_only: false,
        }
    }

    fn doctype() -> DocType {
        DocType {
            name: "thing".into(),
            label: "Thing".into(),
            naming: Naming::Random,
            is_child: false,
            submittable: false,
            permissions: vec![],
            title_field: None,
            list_fields: None,
            icon: None,
            workflow: None,
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
                f(
                    "owner_ref",
                    FieldType::Link {
                        target: "person".into(),
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
        validate(&Registry::new(), &doctype(), d).expect_err("expected errors")
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
        assert_eq!(validate(&Registry::new(), &doctype(), &d), Ok(()));
    }

    #[test]
    fn optional_fields_may_be_null_or_absent() {
        let d = doc(vec![("count", Value::Null), ("kind", Value::Null)]);
        assert_eq!(validate(&Registry::new(), &doctype(), &d), Ok(()));
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
                    validate(&Registry::new(), &dt, &d),
                    Err(vec![ValidationError::Required {
                        field: field.into()
                    }]),
                    "{field} = {blank:?}"
                );
            }
        }
        // Optional fields may hold blank text.
        let d = doc(vec![
            ("body", text("x")),
            ("stage", text("a")),
            ("notes", text("  ")),
        ]);
        assert_eq!(validate(&Registry::new(), &dt, &d), Ok(()));
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
        // The result is unspecified for an invalid DocType; it must only not panic.
        let _ = validate(&Registry::new(), &dt, &doc(vec![("amount", dec("0.001"))]));
    }

    #[test]
    fn link_values_are_text_checked_like_data() {
        // The target name is any non-blank text; existence is checked by forgebase-db.
        assert_eq!(
            validate(
                &Registry::new(),
                &doctype(),
                &doc(vec![("owner_ref", text("ann@example.com"))])
            ),
            Ok(())
        );
        assert_eq!(
            errs(&doc(vec![("owner_ref", Value::Int(1))])),
            vec![ValidationError::TypeMismatch {
                field: "owner_ref".into(),
                expected: "text",
                found: "int"
            }]
        );
        assert_eq!(
            errs(&doc(vec![("owner_ref", text("a\0b"))])),
            vec![ValidationError::ContainsNul {
                field: "owner_ref".into()
            }]
        );
        let mut dt = doctype();
        dt.fields.push(f(
            "must_link",
            FieldType::Link {
                target: "person".into(),
            },
            true,
        ));
        for blank in ["", "  \t"] {
            let mut d = doc(vec![("must_link", text(blank))]);
            assert_eq!(
                validate(&Registry::new(), &dt, &d),
                Err(vec![ValidationError::Required {
                    field: "must_link".into()
                }]),
                "{blank:?}"
            );
            d.values.insert("must_link".into(), Value::Null);
            assert_eq!(validate(&Registry::new(), &dt, &d).unwrap_err().len(), 1);
        }
        // Optional links may be null.
        assert_eq!(
            validate(
                &Registry::new(),
                &doctype(),
                &doc(vec![("owner_ref", Value::Null)])
            ),
            Ok(())
        );
        assert_eq!(
            ValidationError::LinkNotFound {
                field: "owner_ref".into(),
                target: "person".into(),
                name: "x".into()
            }
            .field(),
            "owner_ref"
        );
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
        assert_eq!(validate(&Registry::new(), &doctype(), &ok), Ok(()));
        // Five multi-byte characters are 10 bytes but still within the limit.
        let multibyte = doc(vec![("title", text("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"))]);
        assert_eq!(validate(&Registry::new(), &doctype(), &multibyte), Ok(()));
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
            validate(
                &Registry::new(),
                &doctype(),
                &doc(vec![("kind", text("a"))])
            ),
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
                validate(
                    &Registry::new(),
                    &doctype(),
                    &doc(vec![("amount", dec(ok))])
                ),
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
            assert_eq!(validate(&Registry::new(), &d, &doc), Ok(()), "{ok}");
        }
        let doc = doc(vec![("ratio", dec("1"))]);
        assert_eq!(
            validate(&Registry::new(), &d, &doc),
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

    // ---- child tables ----

    fn order_registry() -> (Registry, DocType) {
        let mut registry = Registry::new();
        let order = DocType {
            name: "order".into(),
            label: "Order".into(),
            naming: Naming::Random,
            is_child: false,
            submittable: false,
            permissions: vec![],
            title_field: None,
            list_fields: None,
            icon: None,
            workflow: None,
            fields: vec![
                f("title", FieldType::Data { max_length: 5 }, false),
                f(
                    "items",
                    FieldType::Table {
                        target: "item".into(),
                    },
                    false,
                ),
            ],
        };
        let item = DocType {
            name: "item".into(),
            label: "Item".into(),
            naming: Naming::Random,
            is_child: true,
            submittable: false,
            permissions: vec![],
            title_field: None,
            list_fields: None,
            icon: None,
            workflow: None,
            fields: vec![
                f("qty", FieldType::Int, true),
                f("note", FieldType::Data { max_length: 3 }, false),
                f(
                    "customer",
                    FieldType::Link {
                        target: "person".into(),
                    },
                    false,
                ),
            ],
        };
        registry.add(item).unwrap();
        registry.add(order.clone()).unwrap();
        (registry, order)
    }

    fn row(pairs: Vec<(&str, Value)>) -> Row {
        pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
    }

    fn with_rows(rows: Vec<Row>) -> Document {
        let mut d = Document::default();
        d.tables.insert("items".into(), rows);
        d
    }

    fn in_row(row: usize, error: ValidationError) -> ValidationError {
        ValidationError::InRow {
            field: "items".into(),
            row,
            error: Box::new(error),
        }
    }

    #[test]
    fn valid_rows_pass_and_none_or_empty_is_fine() {
        let (registry, order) = order_registry();
        let d = with_rows(vec![
            row(vec![("qty", Value::Int(1)), ("customer", text("ann"))]),
            row(vec![("qty", Value::Int(2)), ("note", Value::Null)]),
        ]);
        assert_eq!(validate(&registry, &order, &d), Ok(()));
        assert_eq!(validate(&registry, &order, &Document::default()), Ok(()));
        assert_eq!(validate(&registry, &order, &with_rows(vec![])), Ok(()));
    }

    #[test]
    fn row_errors_carry_the_row_position_and_are_all_collected() {
        let (registry, order) = order_registry();
        let d = with_rows(vec![
            row(vec![("qty", Value::Int(1))]),
            row(vec![]),
            row(vec![
                ("qty", text("x")),
                ("note", text("toolong")),
                ("nope", Value::Int(1)),
            ]),
        ]);
        let errors = validate(&registry, &order, &d).unwrap_err();
        assert_eq!(
            errors,
            vec![
                in_row(
                    2,
                    ValidationError::Required {
                        field: "qty".into()
                    }
                ),
                in_row(
                    3,
                    ValidationError::UnknownField {
                        field: "nope".into()
                    }
                ),
                in_row(
                    3,
                    ValidationError::TooLong {
                        field: "note".into(),
                        len: 7,
                        max: 3
                    }
                ),
                in_row(
                    3,
                    ValidationError::TypeMismatch {
                        field: "qty".into(),
                        expected: "int",
                        found: "text"
                    }
                ),
            ]
        );
        assert_eq!(
            errors[0].to_string(),
            "field `items` row 2: field `qty` is required"
        );
        assert_eq!(errors[0].field(), "items");
        // A row with a blank required value or NUL.
        let d = with_rows(vec![row(vec![("qty", Value::Null)])]);
        assert_eq!(
            validate(&registry, &order, &d),
            Err(vec![in_row(
                1,
                ValidationError::Required {
                    field: "qty".into()
                }
            )])
        );
    }

    #[test]
    fn a_required_table_needs_at_least_one_row() {
        let (registry, mut order) = order_registry();
        order.fields[1].required = true;
        let required = ValidationError::Required {
            field: "items".into(),
        };
        assert_eq!(
            validate(&registry, &order, &Document::default()),
            Err(vec![required.clone()])
        );
        assert_eq!(
            validate(&registry, &order, &with_rows(vec![])),
            Err(vec![required])
        );
        assert_eq!(
            validate(
                &registry,
                &order,
                &with_rows(vec![row(vec![("qty", Value::Int(1))])])
            ),
            Ok(())
        );
    }

    #[test]
    fn unknown_table_and_table_misuse() {
        let (registry, order) = order_registry();
        let mut d = Document::default();
        d.tables.insert("ghosts".into(), vec![]);
        assert_eq!(
            validate(&registry, &order, &d),
            Err(vec![ValidationError::UnknownField {
                field: "ghosts".into()
            }])
        );
        // Rows given for a field that is not a Table field.
        let mut d = Document::default();
        d.tables.insert("title".into(), vec![row(vec![])]);
        assert_eq!(
            validate(&registry, &order, &d),
            Err(vec![ValidationError::TypeMismatch {
                field: "title".into(),
                expected: "text",
                found: "rows"
            }])
        );
        // A value given for a Table field (even null).
        for value in [Value::Int(1), Value::Null, text("x")] {
            let mut d = Document::default();
            let found = value.kind();
            d.values.insert("items".into(), value);
            assert_eq!(
                validate(&registry, &order, &d),
                Err(vec![ValidationError::TypeMismatch {
                    field: "items".into(),
                    expected: "rows",
                    found
                }])
            );
        }
        // A Table field name inside a row is not a child field.
        let d = with_rows(vec![row(vec![
            ("qty", Value::Int(1)),
            ("items", Value::Int(1)),
        ])]);
        assert_eq!(
            validate(&registry, &order, &d),
            Err(vec![in_row(
                1,
                ValidationError::UnknownField {
                    field: "items".into()
                }
            )])
        );
    }

    #[test]
    fn changes_know_when_they_are_empty() {
        let mut changes = Changes::default();
        assert!(changes.is_empty());
        changes.tables.insert("items".into(), vec![]);
        assert!(!changes.is_empty());
    }

    #[test]
    fn a_table_whose_child_is_missing_does_not_panic() {
        let (_, order) = order_registry();
        let d = with_rows(vec![row(vec![])]);
        assert_eq!(validate(&Registry::new(), &order, &d), Ok(()));
    }
}
