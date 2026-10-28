//! The DocType metadata model, its validation and the [`Registry`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use crate::ident::{self, IdentError};

/// Default `max_length` of a Data field.
pub const DEFAULT_MAX_LENGTH: u32 = 140;
/// Largest allowed `max_length` of a Data field.
pub const MAX_DATA_LENGTH: u32 = 65_535;
/// Largest allowed Decimal precision (the range of `rust_decimal`).
pub const MAX_DECIMAL_PRECISION: u8 = 28;

/// A document type: a named list of fields stored in one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocType {
    /// Identifier, see [`ident::validate_doctype_name`].
    pub name: String,
    /// Human-readable name. Never derived from `name`.
    pub label: String,
    /// How document names are generated.
    pub naming: Naming,
    /// The fields, in declaration order.
    pub fields: Vec<DocField>,
}

/// One field of a [`DocType`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocField {
    /// Identifier, see [`ident::validate_field_name`].
    pub name: String,
    /// Human-readable name; defaults to `name` when loaded from TOML without a label.
    pub label: String,
    /// The type and its parameters.
    pub field_type: FieldType,
    /// A value must be present: not null and, for Data, Text and Select, not an empty or
    /// whitespace-only string. Enforced by application validation, not by the database.
    pub required: bool,
    /// Values must be unique across documents.
    pub unique: bool,
}

/// The closed set of M1 field types. Link and Table arrive in M2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldType {
    /// Short text with a maximum length in characters (`1..=65535`).
    Data {
        /// Maximum number of characters.
        max_length: u32,
    },
    /// Text without a length limit.
    Text,
    /// A 64-bit signed integer.
    Int,
    /// A fixed-point number with `precision` total digits, `scale` of them after the point.
    Decimal {
        /// Total number of digits (`1..=28`).
        precision: u8,
        /// Digits after the decimal point (`<= precision`).
        scale: u8,
    },
    /// A boolean.
    Check,
    /// A calendar date.
    Date,
    /// A point in time with an offset.
    Datetime,
    /// One of a fixed list of strings (non-empty, unique, non-empty entries).
    Select {
        /// The allowed values.
        options: Vec<String>,
    },
}

impl FieldType {
    /// Name of the type as written in metadata files.
    pub fn name(&self) -> &'static str {
        match self {
            FieldType::Data { .. } => "data",
            FieldType::Text => "text",
            FieldType::Int => "int",
            FieldType::Decimal { .. } => "decimal",
            FieldType::Check => "check",
            FieldType::Date => "date",
            FieldType::Datetime => "datetime",
            FieldType::Select { .. } => "select",
        }
    }
}

/// How the `name` of a new document is produced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Naming {
    /// A generated random name.
    #[default]
    Random,
    /// The value of the named field. That field must be a required, unique Data field.
    Field(String),
}

/// One problem found in a DocType definition.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DefinitionError {
    /// The DocType name is not a valid identifier.
    #[error("doctype name `{name}` {reason}")]
    InvalidDocTypeName {
        /// The rejected name.
        name: String,
        /// Why it was rejected.
        reason: IdentError,
    },
    /// The DocType label is empty.
    #[error("doctype label must not be empty")]
    EmptyLabel,
    /// A field name is not a valid identifier or is reserved.
    #[error("field name `{field}` {reason}")]
    InvalidFieldName {
        /// The rejected name.
        field: String,
        /// Why it was rejected.
        reason: IdentError,
    },
    /// The DocType label contains a NUL character.
    #[error("doctype label must not contain a NUL character")]
    NulInLabel,
    /// A field label is empty.
    #[error("field `{field}`: label must not be empty")]
    EmptyFieldLabel {
        /// The field.
        field: String,
    },
    /// A field label contains a NUL character.
    #[error("field `{field}`: label must not contain a NUL character")]
    NulInFieldLabel {
        /// The field.
        field: String,
    },
    /// Two fields share a name.
    #[error("field `{field}` is defined more than once")]
    DuplicateField {
        /// The repeated name.
        field: String,
    },
    /// The `type` value is not one of the supported field types.
    #[error(
        "field `{field}`: unknown type `{found}` (expected one of data, text, int, decimal, check, date, datetime, select)"
    )]
    UnknownFieldType {
        /// The field.
        field: String,
        /// The rejected type name.
        found: String,
    },
    /// A parameter required by the field type is missing.
    #[error("field `{field}`: type `{field_type}` requires `{param}`")]
    MissingParam {
        /// The field.
        field: String,
        /// The type name.
        field_type: &'static str,
        /// The missing key.
        param: &'static str,
    },
    /// A parameter is not valid for the field type.
    #[error("field `{field}`: `{param}` is not allowed for type `{field_type}`")]
    UnexpectedParam {
        /// The field.
        field: String,
        /// The type name.
        field_type: String,
        /// The offending key.
        param: &'static str,
    },
    /// `max_length` is outside `1..=65535`.
    #[error("field `{field}`: max_length {value} is out of range (1..={MAX_DATA_LENGTH})")]
    MaxLengthOutOfRange {
        /// The field.
        field: String,
        /// The rejected value.
        value: u32,
    },
    /// Decimal `precision` is outside `1..=28`.
    #[error("field `{field}`: precision {value} is out of range (1..={MAX_DECIMAL_PRECISION})")]
    PrecisionOutOfRange {
        /// The field.
        field: String,
        /// The rejected value.
        value: u8,
    },
    /// Decimal `scale` is larger than `precision`.
    #[error("field `{field}`: scale {scale} must not exceed precision {precision}")]
    ScaleExceedsPrecision {
        /// The field.
        field: String,
        /// The scale.
        scale: u8,
        /// The precision.
        precision: u8,
    },
    /// A Select field has no options.
    #[error("field `{field}`: options must not be empty")]
    EmptyOptions {
        /// The field.
        field: String,
    },
    /// A Select option is the empty string.
    #[error("field `{field}`: options must not contain an empty string")]
    EmptyOption {
        /// The field.
        field: String,
    },
    /// A Select option contains a NUL character, which PostgreSQL text cannot store.
    #[error("field `{field}`: options must not contain a NUL character")]
    NulInOption {
        /// The field.
        field: String,
    },
    /// A Select option appears twice.
    #[error("field `{field}`: duplicate option `{option}`")]
    DuplicateOption {
        /// The field.
        field: String,
        /// The repeated option.
        option: String,
    },
    /// `naming` is a string other than `"random"`.
    #[error("unknown naming strategy `{found}` (expected \"random\" or {{ field = \"<name>\" }})")]
    UnknownNamingStrategy {
        /// The rejected value.
        found: String,
    },
    /// `naming` is neither a string nor a table with the single key `field`.
    #[error("naming must be \"random\" or {{ field = \"<fieldname>\" }}")]
    MalformedNaming,
    /// The naming field does not exist.
    #[error("naming refers to field `{field}`, which does not exist")]
    NamingFieldMissing {
        /// The referenced field.
        field: String,
    },
    /// The naming field is not a Data field.
    #[error("naming field `{field}` must be of type data, found `{found}`")]
    NamingFieldNotData {
        /// The referenced field.
        field: String,
        /// Its actual type name.
        found: &'static str,
    },
    /// The naming field is not required.
    #[error("naming field `{field}` must be required")]
    NamingFieldNotRequired {
        /// The referenced field.
        field: String,
    },
    /// The naming field is not unique.
    #[error("naming field `{field}` must be unique")]
    NamingFieldNotUnique {
        /// The referenced field.
        field: String,
    },
    /// A registry already holds a DocType with this name.
    #[error("doctype `{name}` is already registered")]
    DuplicateDocType {
        /// The repeated name.
        name: String,
    },
}

/// Error returned when loading a DocType from TOML.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MetaError {
    /// The text is not valid TOML or does not match the metadata schema (unknown key, wrong value type, missing key).
    #[error("cannot parse doctype definition: {0}")]
    Parse(#[from] toml::de::Error),
    /// The definition parsed but breaks one or more rules; all problems are listed.
    #[error("invalid doctype definition: {}", join(.0))]
    Invalid(Vec<DefinitionError>),
}

fn join(errors: &[DefinitionError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

impl DocType {
    /// Parses and validates a DocType from TOML text.
    ///
    /// The format is described in `docs/blueprint/architecture.md` section 3 ("DocType file
    /// format (M1)"); an example is `crates/forgebase-core/tests/fixtures/customer.toml`.
    /// Every rule violation is reported, not just the first. Decimal values with more decimal
    /// places than `scale` are rejected on validation, not rounded.
    pub fn from_toml_str(text: &str) -> Result<DocType, MetaError> {
        crate::load::from_toml_str(text)
    }

    /// Looks up a field by name.
    pub fn field(&self, name: &str) -> Option<&DocField> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// Checks every definition rule and returns all problems found.
    pub fn validate(&self) -> Result<(), Vec<DefinitionError>> {
        let errors = self.check(&[]);
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// `failed` lists the names of fields that could not be converted (and so are not in
    /// `self.fields`): their names are still checked, and naming errors about them are
    /// suppressed because the real problem is already reported.
    pub(crate) fn check(&self, failed: &[String]) -> Vec<DefinitionError> {
        let mut errors = Vec::new();
        if let Err(reason) = ident::validate_doctype_name(&self.name) {
            errors.push(DefinitionError::InvalidDocTypeName {
                name: self.name.clone(),
                reason,
            });
        }
        if self.label.trim().is_empty() {
            errors.push(DefinitionError::EmptyLabel);
        }
        if self.label.contains('\0') {
            errors.push(DefinitionError::NulInLabel);
        }
        let mut seen = BTreeSet::new();
        for field in &self.fields {
            if !seen.insert(field.name.as_str()) {
                errors.push(DefinitionError::DuplicateField {
                    field: field.name.clone(),
                });
                continue;
            }
            field.check(&mut errors);
        }
        for name in failed {
            if let Err(reason) = ident::validate_field_name(name) {
                errors.push(DefinitionError::InvalidFieldName {
                    field: name.clone(),
                    reason,
                });
            }
            if !seen.insert(name.as_str()) {
                errors.push(DefinitionError::DuplicateField {
                    field: name.clone(),
                });
            }
        }
        if let Naming::Field(name) = &self.naming
            && !failed.contains(name)
        {
            self.check_naming_field(name, &mut errors);
        }
        errors
    }

    fn check_naming_field(&self, name: &str, errors: &mut Vec<DefinitionError>) {
        let field = name.to_owned();
        match self.field(name) {
            None => errors.push(DefinitionError::NamingFieldMissing { field }),
            Some(f) => {
                if !matches!(f.field_type, FieldType::Data { .. }) {
                    errors.push(DefinitionError::NamingFieldNotData {
                        field: field.clone(),
                        found: f.field_type.name(),
                    });
                }
                if !f.required {
                    errors.push(DefinitionError::NamingFieldNotRequired {
                        field: field.clone(),
                    });
                }
                if !f.unique {
                    errors.push(DefinitionError::NamingFieldNotUnique { field });
                }
            }
        }
    }
}

impl DocField {
    fn check(&self, errors: &mut Vec<DefinitionError>) {
        let field = || self.name.clone();
        if let Err(reason) = ident::validate_field_name(&self.name) {
            errors.push(DefinitionError::InvalidFieldName {
                field: field(),
                reason,
            });
        }
        if self.label.trim().is_empty() {
            errors.push(DefinitionError::EmptyFieldLabel { field: field() });
        }
        if self.label.contains('\0') {
            errors.push(DefinitionError::NulInFieldLabel { field: field() });
        }
        match &self.field_type {
            FieldType::Data { max_length } => {
                if !(1..=MAX_DATA_LENGTH).contains(max_length) {
                    errors.push(DefinitionError::MaxLengthOutOfRange {
                        field: field(),
                        value: *max_length,
                    });
                }
            }
            FieldType::Decimal { precision, scale } => {
                if !(1..=MAX_DECIMAL_PRECISION).contains(precision) {
                    errors.push(DefinitionError::PrecisionOutOfRange {
                        field: field(),
                        value: *precision,
                    });
                } else if scale > precision {
                    errors.push(DefinitionError::ScaleExceedsPrecision {
                        field: field(),
                        scale: *scale,
                        precision: *precision,
                    });
                }
            }
            FieldType::Select { options } => {
                if options.is_empty() {
                    errors.push(DefinitionError::EmptyOptions { field: field() });
                }
                let mut seen = BTreeSet::new();
                for option in options {
                    if option.contains('\0') {
                        errors.push(DefinitionError::NulInOption { field: field() });
                    } else if option.is_empty() {
                        errors.push(DefinitionError::EmptyOption { field: field() });
                    } else if !seen.insert(option.as_str()) {
                        errors.push(DefinitionError::DuplicateOption {
                            field: field(),
                            option: option.clone(),
                        });
                    }
                }
            }
            FieldType::Text
            | FieldType::Int
            | FieldType::Check
            | FieldType::Date
            | FieldType::Datetime => {}
        }
    }
}

/// A set of DocTypes keyed by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    doctypes: BTreeMap<String, DocType>,
}

impl Registry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Validates and adds a DocType. Fails with every definition problem found
    /// ([`DocType::validate`]), or with [`DefinitionError::DuplicateDocType`] if one with the
    /// same name is already present. The registry therefore only holds valid DocTypes.
    pub fn add(&mut self, doctype: DocType) -> Result<(), Vec<DefinitionError>> {
        doctype.validate()?;
        if self.doctypes.contains_key(&doctype.name) {
            return Err(vec![DefinitionError::DuplicateDocType {
                name: doctype.name,
            }]);
        }
        self.doctypes.insert(doctype.name.clone(), doctype);
        Ok(())
    }

    /// Looks up a DocType by name.
    pub fn get(&self, name: &str) -> Option<&DocType> {
        self.doctypes.get(name)
    }

    /// Iterates over the DocTypes in name order.
    pub fn iter(&self) -> impl Iterator<Item = &DocType> {
        self.doctypes.values()
    }

    /// Number of registered DocTypes.
    pub fn len(&self) -> usize {
        self.doctypes.len()
    }

    /// True when no DocType is registered.
    pub fn is_empty(&self) -> bool {
        self.doctypes.is_empty()
    }
}

impl fmt::Display for FieldType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, field_type: FieldType) -> DocField {
        DocField {
            name: name.into(),
            label: name.into(),
            field_type,
            required: false,
            unique: false,
        }
    }

    fn doctype(fields: Vec<DocField>) -> DocType {
        DocType {
            name: "thing".into(),
            label: "Thing".into(),
            naming: Naming::Random,
            fields,
        }
    }

    fn errors_of(d: &DocType) -> Vec<DefinitionError> {
        d.validate().expect_err("expected validation errors")
    }

    #[test]
    fn valid_doctype_passes() {
        let d = doctype(vec![
            field("a", FieldType::Data { max_length: 1 }),
            field("b", FieldType::Data { max_length: 65_535 }),
            field(
                "c",
                FieldType::Decimal {
                    precision: 28,
                    scale: 28,
                },
            ),
            field(
                "d",
                FieldType::Select {
                    options: vec!["x".into(), "y".into()],
                },
            ),
        ]);
        assert_eq!(d.validate(), Ok(()));
    }

    #[test]
    fn name_and_label_rules() {
        let mut d = doctype(vec![]);
        d.name = "Thing".into();
        d.label = "  ".into();
        assert_eq!(
            errors_of(&d),
            vec![
                DefinitionError::InvalidDocTypeName {
                    name: "Thing".into(),
                    reason: IdentError::InvalidFormat
                },
                DefinitionError::EmptyLabel,
            ]
        );
    }

    #[test]
    fn field_rules() {
        let cases: Vec<(DocField, DefinitionError)> = vec![
            (
                field("Bad", FieldType::Int),
                DefinitionError::InvalidFieldName {
                    field: "Bad".into(),
                    reason: IdentError::InvalidFormat,
                },
            ),
            (
                field("owner", FieldType::Int),
                DefinitionError::InvalidFieldName {
                    field: "owner".into(),
                    reason: IdentError::Reserved,
                },
            ),
            (
                DocField {
                    label: String::new(),
                    ..field("x", FieldType::Int)
                },
                DefinitionError::EmptyFieldLabel { field: "x".into() },
            ),
            (
                field("x", FieldType::Data { max_length: 0 }),
                DefinitionError::MaxLengthOutOfRange {
                    field: "x".into(),
                    value: 0,
                },
            ),
            (
                field("x", FieldType::Data { max_length: 65_536 }),
                DefinitionError::MaxLengthOutOfRange {
                    field: "x".into(),
                    value: 65_536,
                },
            ),
            (
                field(
                    "x",
                    FieldType::Decimal {
                        precision: 0,
                        scale: 0,
                    },
                ),
                DefinitionError::PrecisionOutOfRange {
                    field: "x".into(),
                    value: 0,
                },
            ),
            (
                field(
                    "x",
                    FieldType::Decimal {
                        precision: 29,
                        scale: 2,
                    },
                ),
                DefinitionError::PrecisionOutOfRange {
                    field: "x".into(),
                    value: 29,
                },
            ),
            (
                field(
                    "x",
                    FieldType::Decimal {
                        precision: 5,
                        scale: 6,
                    },
                ),
                DefinitionError::ScaleExceedsPrecision {
                    field: "x".into(),
                    scale: 6,
                    precision: 5,
                },
            ),
            (
                field("x", FieldType::Select { options: vec![] }),
                DefinitionError::EmptyOptions { field: "x".into() },
            ),
            (
                field(
                    "x",
                    FieldType::Select {
                        options: vec!["a".into(), String::new()],
                    },
                ),
                DefinitionError::EmptyOption { field: "x".into() },
            ),
            (
                field(
                    "x",
                    FieldType::Select {
                        options: vec!["a".into(), "a".into()],
                    },
                ),
                DefinitionError::DuplicateOption {
                    field: "x".into(),
                    option: "a".into(),
                },
            ),
        ];
        for (f, expected) in cases {
            assert_eq!(
                errors_of(&doctype(vec![f.clone()])),
                vec![expected],
                "field {f:?}"
            );
        }
    }

    #[test]
    fn duplicate_fields_are_rejected() {
        let d = doctype(vec![
            field("a", FieldType::Int),
            field("a", FieldType::Text),
        ]);
        assert_eq!(
            errors_of(&d),
            vec![DefinitionError::DuplicateField { field: "a".into() }]
        );
    }

    #[test]
    fn naming_field_rules() {
        let good = DocField {
            required: true,
            unique: true,
            ..field("email", FieldType::Data { max_length: 10 })
        };
        let naming = |name: &str| Naming::Field(name.into());

        let mut d = doctype(vec![good.clone()]);
        d.naming = naming("email");
        assert_eq!(d.validate(), Ok(()));

        d.naming = naming("nope");
        assert_eq!(
            errors_of(&d),
            vec![DefinitionError::NamingFieldMissing {
                field: "nope".into()
            }]
        );

        let not_data = DocField {
            field_type: FieldType::Text,
            ..good.clone()
        };
        let mut d = doctype(vec![not_data]);
        d.naming = naming("email");
        assert_eq!(
            errors_of(&d),
            vec![DefinitionError::NamingFieldNotData {
                field: "email".into(),
                found: "text"
            }]
        );

        let weak = DocField {
            required: false,
            unique: false,
            ..good
        };
        let mut d = doctype(vec![weak]);
        d.naming = naming("email");
        assert_eq!(
            errors_of(&d),
            vec![
                DefinitionError::NamingFieldNotRequired {
                    field: "email".into()
                },
                DefinitionError::NamingFieldNotUnique {
                    field: "email".into()
                },
            ]
        );
    }

    #[test]
    fn multiple_errors_are_collected() {
        let mut d = doctype(vec![
            field("id", FieldType::Int),
            field("x", FieldType::Select { options: vec![] }),
        ]);
        d.label = String::new();
        assert_eq!(errors_of(&d).len(), 3);
    }

    #[test]
    fn nul_is_rejected_in_labels_and_options() {
        let mut d = doctype(vec![
            field(
                "s",
                FieldType::Select {
                    options: vec!["a\0b".into(), "ok".into()],
                },
            ),
            field("t", FieldType::Text),
        ]);
        d.fields[1].label = "x\0".into();
        d.label = "T\0".into();
        assert_eq!(
            errors_of(&d),
            vec![
                DefinitionError::NulInLabel,
                DefinitionError::NulInOption { field: "s".into() },
                DefinitionError::NulInFieldLabel { field: "t".into() },
            ]
        );
    }

    #[test]
    fn registry_rejects_invalid_doctypes() {
        let mut registry = Registry::new();
        let mut bad_name = doctype(vec![]);
        bad_name.name = "x\"; drop table y; --".into();
        let errors = registry.add(bad_name).unwrap_err();
        assert!(matches!(
            errors.as_slice(),
            [DefinitionError::InvalidDocTypeName { .. }]
        ));

        let bad_scale = doctype(vec![field(
            "amount",
            FieldType::Decimal {
                precision: 2,
                scale: 3,
            },
        )]);
        assert_eq!(
            registry.add(bad_scale),
            Err(vec![DefinitionError::ScaleExceedsPrecision {
                field: "amount".into(),
                scale: 3,
                precision: 2
            }])
        );
        assert!(registry.is_empty());
    }

    #[test]
    fn registry_rejects_duplicates() {
        let mut registry = Registry::new();
        assert!(registry.is_empty());
        registry.add(doctype(vec![])).unwrap();
        assert_eq!(
            registry.add(doctype(vec![])),
            Err(vec![DefinitionError::DuplicateDocType {
                name: "thing".into()
            }])
        );
        assert_eq!(registry.len(), 1);
        assert!(registry.get("thing").is_some());
        assert!(registry.get("other").is_none());
        assert_eq!(registry.iter().count(), 1);
    }
}
