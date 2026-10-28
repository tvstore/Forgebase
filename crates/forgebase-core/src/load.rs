//! TOML loading: parse into a raw, string-typed form, then convert into the validated model.

use serde::Deserialize;

use crate::meta::{
    DEFAULT_MAX_LENGTH, DefinitionError, DocField, DocType, FieldType, MetaError, Naming,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDocType {
    name: String,
    label: String,
    /// Kept as a generic value so a malformed `naming` gets a clear message.
    naming: Option<toml::Value>,
    #[serde(default)]
    fields: Vec<RawField>,
}

fn convert_naming(value: Option<toml::Value>, errors: &mut Vec<DefinitionError>) -> Naming {
    match value {
        None => Naming::Random,
        Some(toml::Value::String(s)) if s == "random" => Naming::Random,
        Some(toml::Value::String(s)) => {
            errors.push(DefinitionError::UnknownNamingStrategy { found: s });
            Naming::Random
        }
        Some(toml::Value::Table(t)) => match (t.len(), t.get("field")) {
            (1, Some(toml::Value::String(field))) => Naming::Field(field.clone()),
            _ => {
                errors.push(DefinitionError::MalformedNaming);
                Naming::Random
            }
        },
        Some(_) => {
            errors.push(DefinitionError::MalformedNaming);
            Naming::Random
        }
    }
}

/// All type parameters live in one flat table so unknown keys can be rejected; which keys
/// are allowed depends on `type` and is checked in [`convert_type`].
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    name: String,
    label: Option<String>,
    #[serde(rename = "type")]
    field_type: String,
    #[serde(default)]
    required: bool,
    #[serde(default)]
    unique: bool,
    max_length: Option<u32>,
    precision: Option<u8>,
    scale: Option<u8>,
    options: Option<Vec<String>>,
    target: Option<String>,
}

pub(crate) fn from_toml_str(text: &str) -> Result<DocType, MetaError> {
    let raw: RawDocType = toml::from_str(text)?;
    let mut errors = Vec::new();
    let mut failed = Vec::new();

    let naming = convert_naming(raw.naming, &mut errors);

    let mut fields = Vec::with_capacity(raw.fields.len());
    for f in raw.fields {
        match convert_type(&f) {
            Ok(field_type) => fields.push(DocField {
                label: f.label.unwrap_or_else(|| f.name.clone()),
                name: f.name,
                field_type,
                required: f.required,
                unique: f.unique,
            }),
            Err(mut e) => {
                errors.append(&mut e);
                failed.push(f.name);
            }
        }
    }

    let doctype = DocType {
        name: raw.name,
        label: raw.label,
        naming,
        fields,
    };
    errors.extend(doctype.check(&failed));
    if errors.is_empty() {
        Ok(doctype)
    } else {
        Err(MetaError::Invalid(errors))
    }
}

fn convert_type(f: &RawField) -> Result<FieldType, Vec<DefinitionError>> {
    let name = f.field_type.as_str();
    let (field_type, allowed): (Option<FieldType>, &[&str]) = match name {
        "data" => (
            Some(FieldType::Data {
                max_length: f.max_length.unwrap_or(DEFAULT_MAX_LENGTH),
            }),
            &["max_length"],
        ),
        "text" => (Some(FieldType::Text), &[]),
        "int" => (Some(FieldType::Int), &[]),
        "check" => (Some(FieldType::Check), &[]),
        "date" => (Some(FieldType::Date), &[]),
        "datetime" => (Some(FieldType::Datetime), &[]),
        "decimal" => (None, &["precision", "scale"]),
        "select" => (None, &["options"]),
        "link" => (None, &["target"]),
        _ => {
            return Err(vec![DefinitionError::UnknownFieldType {
                field: f.name.clone(),
                found: f.field_type.clone(),
            }]);
        }
    };

    let mut errors = Vec::new();
    let present = [
        ("max_length", f.max_length.is_some()),
        ("precision", f.precision.is_some()),
        ("scale", f.scale.is_some()),
        ("options", f.options.is_some()),
        ("target", f.target.is_some()),
    ];
    for (param, is_present) in present {
        if is_present && !allowed.contains(&param) {
            errors.push(DefinitionError::UnexpectedParam {
                field: f.name.clone(),
                field_type: f.field_type.clone(),
                param,
            });
        }
    }

    let field_type = match (field_type, name) {
        (Some(t), _) => Some(t),
        (None, "decimal") => match (f.precision, f.scale) {
            (Some(precision), Some(scale)) => Some(FieldType::Decimal { precision, scale }),
            (precision, scale) => {
                for (param, value) in [
                    ("precision", precision.is_none()),
                    ("scale", scale.is_none()),
                ] {
                    if value {
                        errors.push(DefinitionError::MissingParam {
                            field: f.name.clone(),
                            field_type: "decimal",
                            param,
                        });
                    }
                }
                None
            }
        },
        (None, "select") => match &f.options {
            Some(options) => Some(FieldType::Select {
                options: options.clone(),
            }),
            None => {
                errors.push(DefinitionError::MissingParam {
                    field: f.name.clone(),
                    field_type: "select",
                    param: "options",
                });
                None
            }
        },
        (None, "link") => match &f.target {
            Some(target) => Some(FieldType::Link {
                target: target.clone(),
            }),
            None => {
                errors.push(DefinitionError::MissingParam {
                    field: f.name.clone(),
                    field_type: "link",
                    param: "target",
                });
                None
            }
        },
        (None, _) => unreachable!("every type without fixed parameters is matched above"),
    };

    match (field_type, errors.is_empty()) {
        (Some(t), true) => Ok(t),
        _ => Err(errors),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CUSTOMER: &str = r#"
name = "customer"
label = "Customer"
naming = { field = "email" }

[[fields]]
name = "email"
type = "data"
max_length = 140
required = true
unique = true

[[fields]]
name = "status"
label = "Account status"
type = "select"
options = ["active", "closed"]

[[fields]]
name = "credit_limit"
type = "decimal"
precision = 12
scale = 2
"#;

    fn invalid(text: &str) -> Vec<DefinitionError> {
        match from_toml_str(text) {
            Err(MetaError::Invalid(e)) => e,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    fn doc_with_field(field_body: &str) -> String {
        format!("name = \"t\"\nlabel = \"T\"\n[[fields]]\nname = \"f\"\n{field_body}\n")
    }

    #[test]
    fn parses_valid_definition() {
        let d = from_toml_str(CUSTOMER).unwrap();
        assert_eq!(d.name, "customer");
        assert_eq!(d.label, "Customer");
        assert_eq!(d.naming, Naming::Field("email".into()));
        assert_eq!(d.fields.len(), 3);
        assert_eq!(d.fields[0].label, "email");
        assert_eq!(d.fields[1].label, "Account status");
        assert_eq!(
            d.fields[2].field_type,
            FieldType::Decimal {
                precision: 12,
                scale: 2
            }
        );
        assert!(d.fields[0].required && d.fields[0].unique);
    }

    #[test]
    fn naming_defaults_and_random() {
        let base = "name = \"t\"\nlabel = \"T\"\n";
        for text in [base.to_owned(), format!("naming = \"random\"\n{base}")] {
            assert_eq!(from_toml_str(&text).unwrap().naming, Naming::Random);
        }
        assert_eq!(
            invalid(&format!("naming = \"series\"\n{base}")),
            vec![DefinitionError::UnknownNamingStrategy {
                found: "series".into()
            }]
        );
    }

    #[test]
    fn data_default_max_length() {
        let d = from_toml_str(&doc_with_field("type = \"data\"")).unwrap();
        assert_eq!(d.fields[0].field_type, FieldType::Data { max_length: 140 });
    }

    #[test]
    fn simple_types() {
        for t in ["text", "int", "check", "date", "datetime"] {
            let d = from_toml_str(&doc_with_field(&format!("type = \"{t}\""))).unwrap();
            assert_eq!(d.fields[0].field_type.name(), t);
        }
    }

    #[test]
    fn parses_link_fields() {
        let d = from_toml_str(&doc_with_field("type = \"link\"\ntarget = \"customer\"")).unwrap();
        assert_eq!(
            d.fields[0].field_type,
            FieldType::Link {
                target: "customer".into()
            }
        );
        assert_eq!(d.fields[0].field_type.name(), "link");
    }

    #[test]
    fn link_parameter_errors() {
        assert_eq!(
            invalid(&doc_with_field("type = \"link\"")),
            vec![DefinitionError::MissingParam {
                field: "f".into(),
                field_type: "link",
                param: "target",
            }]
        );
        assert_eq!(
            invalid(&doc_with_field("type = \"int\"\ntarget = \"customer\"")),
            vec![DefinitionError::UnexpectedParam {
                field: "f".into(),
                field_type: "int".into(),
                param: "target",
            }]
        );
        assert_eq!(
            invalid(&doc_with_field(
                "type = \"link\"\ntarget = \"customer\"\nmax_length = 5"
            )),
            vec![DefinitionError::UnexpectedParam {
                field: "f".into(),
                field_type: "link".into(),
                param: "max_length",
            }]
        );
        let errors = invalid(&doc_with_field("type = \"link\"\ntarget = \"Bad Target\""));
        assert!(
            matches!(
                errors.as_slice(),
                [DefinitionError::InvalidLinkTarget { target, .. }] if target == "Bad Target"
            ),
            "{errors:?}"
        );
    }

    #[test]
    fn parse_errors() {
        let cases = [
            (
                "unknown top-level key",
                "name = \"t\"\nlabel = \"T\"\nextra = 1\n",
            ),
            (
                "unknown field key",
                "name = \"t\"\nlabel = \"T\"\n[[fields]]\nname = \"f\"\ntype = \"int\"\nextra = 1\n",
            ),
            ("missing name", "label = \"T\"\n"),
            ("missing label", "name = \"t\"\n"),
            (
                "missing field type",
                "name = \"t\"\nlabel = \"T\"\n[[fields]]\nname = \"f\"\n",
            ),
            (
                "wrong value kind",
                "name = \"t\"\nlabel = \"T\"\n[[fields]]\nname = \"f\"\ntype = 5\n",
            ),
            (
                "negative max_length",
                "name = \"t\"\nlabel = \"T\"\n[[fields]]\nname = \"f\"\ntype = \"data\"\nmax_length = -1\n",
            ),
            ("not toml", "this is not toml"),
        ];
        for (what, text) in cases {
            assert!(
                matches!(from_toml_str(text), Err(MetaError::Parse(_))),
                "{what}"
            );
        }
    }

    #[test]
    fn type_errors() {
        let cases: Vec<(&str, DefinitionError)> = vec![
            (
                "type = \"float\"",
                DefinitionError::UnknownFieldType {
                    field: "f".into(),
                    found: "float".into(),
                },
            ),
            (
                "type = \"decimal\"\nscale = 2",
                DefinitionError::MissingParam {
                    field: "f".into(),
                    field_type: "decimal",
                    param: "precision",
                },
            ),
            (
                "type = \"decimal\"\nprecision = 5",
                DefinitionError::MissingParam {
                    field: "f".into(),
                    field_type: "decimal",
                    param: "scale",
                },
            ),
            (
                "type = \"select\"",
                DefinitionError::MissingParam {
                    field: "f".into(),
                    field_type: "select",
                    param: "options",
                },
            ),
            (
                "type = \"int\"\nmax_length = 5",
                DefinitionError::UnexpectedParam {
                    field: "f".into(),
                    field_type: "int".into(),
                    param: "max_length",
                },
            ),
            (
                "type = \"data\"\noptions = [\"a\"]",
                DefinitionError::UnexpectedParam {
                    field: "f".into(),
                    field_type: "data".into(),
                    param: "options",
                },
            ),
            (
                "type = \"data\"\nmax_length = 0",
                DefinitionError::MaxLengthOutOfRange {
                    field: "f".into(),
                    value: 0,
                },
            ),
            (
                "type = \"select\"\noptions = []",
                DefinitionError::EmptyOptions { field: "f".into() },
            ),
            (
                "type = \"decimal\"\nprecision = 2\nscale = 3",
                DefinitionError::ScaleExceedsPrecision {
                    field: "f".into(),
                    scale: 3,
                    precision: 2,
                },
            ),
        ];
        for (body, expected) in cases {
            assert_eq!(invalid(&doc_with_field(body)), vec![expected], "{body}");
        }
    }

    #[test]
    fn collects_all_errors_across_fields() {
        let text = r#"
name = "Bad Name"
label = "T"
naming = { field = "gone" }

[[fields]]
name = "owner"
type = "int"

[[fields]]
name = "a"
type = "nope"

[[fields]]
name = "gone"
type = "date"
"#;
        let errors = invalid(text);
        // Bad doctype name, reserved field name, unknown type, naming field not data/required/unique.
        assert_eq!(errors.len(), 6, "{errors:?}");
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, DefinitionError::UnknownFieldType { .. }))
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, DefinitionError::InvalidFieldName { .. }))
        );
    }

    #[test]
    fn failed_naming_field_is_not_reported_twice() {
        let text = "name = \"t\"\nlabel = \"T\"\nnaming = { field = \"f\" }\n[[fields]]\nname = \"f\"\ntype = \"nope\"\n";
        assert_eq!(
            invalid(text),
            vec![DefinitionError::UnknownFieldType {
                field: "f".into(),
                found: "nope".into()
            }]
        );
    }

    #[test]
    fn naming_shape_errors() {
        let base = "name = \"t\"\nlabel = \"T\"\n";
        for naming in [
            "5",
            "{ fieldd = \"a\" }",
            "{ field = 1 }",
            "{ field = \"a\", x = 1 }",
            "[\"a\"]",
        ] {
            let errors = invalid(&format!("naming = {naming}\n{base}"));
            assert_eq!(errors, vec![DefinitionError::MalformedNaming], "{naming}");
            assert_eq!(
                errors[0].to_string(),
                "naming must be \"random\" or { field = \"<fieldname>\" }"
            );
        }
    }

    #[test]
    fn failed_fields_still_get_name_and_duplicate_checks() {
        let text =
            "name = \"t\"\nlabel = \"T\"\n[[fields]]\nname = \"Bad Name\"\ntype = \"nope\"\n";
        let errors = invalid(text);
        assert_eq!(errors.len(), 2, "{errors:?}");
        assert!(matches!(
            errors[0],
            DefinitionError::UnknownFieldType { .. }
        ));
        assert!(matches!(
            errors[1],
            DefinitionError::InvalidFieldName { .. }
        ));

        let text = "name = \"t\"\nlabel = \"T\"\n[[fields]]\nname = \"f\"\ntype = \"int\"\n[[fields]]\nname = \"f\"\ntype = \"nope\"\n";
        let errors = invalid(text);
        assert!(
            errors.contains(&DefinitionError::DuplicateField { field: "f".into() }),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, DefinitionError::UnknownFieldType { .. }))
        );
    }

    #[test]
    fn duplicate_field_names() {
        let text = "name = \"t\"\nlabel = \"T\"\n[[fields]]\nname = \"f\"\ntype = \"int\"\n[[fields]]\nname = \"f\"\ntype = \"int\"\n";
        assert_eq!(
            invalid(text),
            vec![DefinitionError::DuplicateField { field: "f".into() }]
        );
    }

    #[test]
    fn error_message_lists_problems() {
        let err = from_toml_str(&doc_with_field("type = \"float\"")).unwrap_err();
        assert!(err.to_string().contains("unknown type `float`"), "{err}");
    }
}
