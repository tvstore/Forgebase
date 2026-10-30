//! TOML loading: parse into a raw, string-typed form, then convert into the validated model.

use serde::Deserialize;

use crate::meta::{
    DEFAULT_MAX_LENGTH, DefinitionError, DocField, DocType, FieldType, MetaError, Naming, humanize,
};
use crate::permission::PermissionRule;
use crate::series::SeriesPattern;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawDocType {
    name: String,
    label: Option<String>,
    /// Kept as a generic value so a malformed `naming` gets a clear message.
    naming: Option<toml::Value>,
    #[serde(default)]
    is_child: bool,
    #[serde(default)]
    submittable: bool,
    #[serde(default)]
    fields: Vec<RawField>,
    #[serde(default)]
    permissions: Vec<RawPermission>,
    title_field: Option<String>,
    list_fields: Option<Vec<String>>,
}

/// A `[[permissions]]` entry; every right defaults to false.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPermission {
    role: String,
    #[serde(default)]
    read: bool,
    #[serde(default)]
    write: bool,
    #[serde(default)]
    create: bool,
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    submit: bool,
    #[serde(default)]
    cancel: bool,
    #[serde(default)]
    amend: bool,
    #[serde(default)]
    if_owner: bool,
}

impl From<RawPermission> for PermissionRule {
    fn from(raw: RawPermission) -> Self {
        PermissionRule {
            role: raw.role,
            read: raw.read,
            write: raw.write,
            create: raw.create,
            delete: raw.delete,
            submit: raw.submit,
            cancel: raw.cancel,
            amend: raw.amend,
            if_owner: raw.if_owner,
        }
    }
}

fn convert_naming(value: Option<toml::Value>, errors: &mut Vec<DefinitionError>) -> Naming {
    match value {
        None => Naming::Random,
        Some(toml::Value::String(s)) if s == "random" => Naming::Random,
        Some(toml::Value::String(s)) => {
            errors.push(DefinitionError::UnknownNamingStrategy { found: s });
            Naming::Random
        }
        Some(toml::Value::Table(t)) => match (t.len(), t.get("field"), t.get("series")) {
            (1, Some(toml::Value::String(field)), _) => Naming::Field(field.clone()),
            (1, _, Some(toml::Value::String(pattern))) => match SeriesPattern::parse(pattern) {
                Ok(pattern) => Naming::Series(pattern),
                Err(reason) => {
                    errors.push(DefinitionError::InvalidSeriesPattern {
                        pattern: pattern.clone(),
                        reason,
                    });
                    Naming::Random
                }
            },
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
    #[serde(default)]
    allow_on_submit: bool,
    #[serde(default)]
    read_only: bool,
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

    // A child has no naming rule at all; `convert_naming` would accept `naming = "random"`.
    let naming = if raw.is_child && raw.naming.is_some() {
        errors.push(DefinitionError::ChildWithNaming);
        Naming::Random
    } else {
        convert_naming(raw.naming, &mut errors)
    };

    let mut fields = Vec::with_capacity(raw.fields.len());
    for f in raw.fields {
        match convert_type(&f) {
            Ok(field_type) => fields.push(DocField {
                label: f.label.unwrap_or_else(|| humanize(&f.name)),
                name: f.name,
                field_type,
                required: f.required,
                unique: f.unique,
                allow_on_submit: f.allow_on_submit,
                read_only: f.read_only,
            }),
            Err(mut e) => {
                errors.append(&mut e);
                failed.push(f.name);
            }
        }
    }

    let doctype = DocType {
        label: raw.label.unwrap_or_else(|| humanize(&raw.name)),
        name: raw.name,
        naming,
        fields,
        is_child: raw.is_child,
        submittable: raw.submittable,
        permissions: raw.permissions.into_iter().map(Into::into).collect(),
        title_field: raw.title_field,
        list_fields: raw.list_fields,
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
        "link" | "table" => (None, &["target"]),
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
        (None, "table") => match &f.target {
            Some(target) => Some(FieldType::Table {
                target: target.clone(),
            }),
            None => {
                errors.push(DefinitionError::MissingParam {
                    field: f.name.clone(),
                    field_type: "table",
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
        assert_eq!(d.fields[0].label, "Email");
        assert_eq!(d.fields[1].label, "Account status");
        assert_eq!(d.fields[2].label, "Credit Limit");
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
    fn parses_table_fields_and_child_doctypes() {
        let d = from_toml_str(&doc_with_field("type = \"table\"\ntarget = \"row\"")).unwrap();
        assert_eq!(
            d.fields[0].field_type,
            FieldType::Table {
                target: "row".into()
            }
        );
        assert_eq!(d.fields[0].field_type.name(), "table");
        assert!(!d.is_child);

        let child = "name = \"row\"\nlabel = \"R\"\nis_child = true\n[[fields]]\nname = \"qty\"\ntype = \"int\"\n";
        let d = from_toml_str(child).unwrap();
        assert!(d.is_child);
        assert_eq!(d.naming, Naming::Random);
    }

    #[test]
    fn table_parameter_errors() {
        assert_eq!(
            invalid(&doc_with_field("type = \"table\"")),
            vec![DefinitionError::MissingParam {
                field: "f".into(),
                field_type: "table",
                param: "target",
            }]
        );
        assert_eq!(
            invalid(&doc_with_field(
                "type = \"table\"\ntarget = \"row\"\nmax_length = 5"
            )),
            vec![DefinitionError::UnexpectedParam {
                field: "f".into(),
                field_type: "table".into(),
                param: "max_length",
            }]
        );
        assert_eq!(
            invalid(&doc_with_field(
                "type = \"table\"\ntarget = \"row\"\nunique = true"
            )),
            vec![DefinitionError::UniqueTable { field: "f".into() }]
        );
        // `required` (at least one row) is fine.
        assert!(
            from_toml_str(&doc_with_field(
                "type = \"table\"\ntarget = \"row\"\nrequired = true"
            ))
            .is_ok()
        );
    }

    #[test]
    fn child_doctype_rules_in_toml() {
        let base = "name = \"row\"\nlabel = \"R\"\nis_child = true\n";
        // Any naming key is refused, also the default one.
        for naming in ["\"random\"", "{ field = \"code\" }", "\"bogus\""] {
            assert_eq!(
                invalid(&format!("naming = {naming}\n{base}")),
                vec![DefinitionError::ChildWithNaming],
                "{naming}"
            );
        }
        let nested =
            format!("{base}[[fields]]\nname = \"more\"\ntype = \"table\"\ntarget = \"x\"\n");
        assert_eq!(
            invalid(&nested),
            vec![DefinitionError::ChildWithTable {
                field: "more".into()
            }]
        );
        let unique = format!("{base}[[fields]]\nname = \"code\"\ntype = \"data\"\nunique = true\n");
        assert_eq!(
            invalid(&unique),
            vec![DefinitionError::ChildWithUnique {
                field: "code".into()
            }]
        );
        // A wrong value kind is a parse error.
        assert!(matches!(
            from_toml_str("name = \"t\"\nlabel = \"T\"\nis_child = \"yes\"\n"),
            Err(MetaError::Parse(_))
        ));
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
    fn labels_default_to_the_humanized_name() {
        let d = from_toml_str(
            "name = \"sales_order\"\n[[fields]]\nname = \"order_date\"\ntype = \"date\"\n\
             [[fields]]\nname = \"qty\"\ntype = \"int\"\nlabel = \"Quantity\"\n",
        )
        .unwrap();
        assert_eq!(d.label, "Sales Order");
        assert_eq!(d.fields[0].label, "Order Date");
        assert_eq!(d.fields[1].label, "Quantity");
        let d = from_toml_str("name = \"t\"\nlabel = \"Ticket\"\n").unwrap();
        assert_eq!(d.label, "Ticket");
    }

    #[test]
    fn read_only_rules() {
        let d = from_toml_str(&doc_with_field("type = \"int\"\nread_only = true")).unwrap();
        assert!(d.fields[0].read_only);
        let d = from_toml_str(&doc_with_field("type = \"int\"")).unwrap();
        assert!(!d.fields[0].read_only);
        assert_eq!(
            invalid(&doc_with_field(
                "type = \"int\"\nread_only = true\nrequired = true"
            )),
            vec![DefinitionError::ReadOnlyRequired { field: "f".into() }]
        );
        assert_eq!(
            invalid(&doc_with_field(
                "type = \"table\"\ntarget = \"row\"\nread_only = true"
            )),
            vec![DefinitionError::ReadOnlyTable { field: "f".into() }]
        );
    }

    #[test]
    fn title_field_and_list_fields() {
        let base = "name = \"t\"\nlabel = \"T\"\n";
        let fields = "[[fields]]\nname = \"a\"\ntype = \"data\"\n[[fields]]\nname = \"b\"\ntype = \"check\"\n\
                      [[fields]]\nname = \"c\"\ntype = \"text\"\n[[fields]]\nname = \"rows\"\ntype = \"table\"\ntarget = \"row\"\n";
        let plain = from_toml_str(&format!("{base}{fields}")).unwrap();
        assert_eq!((plain.title_field, plain.list_fields), (None, None));
        let d = from_toml_str(&format!(
            "{base}title_field = \"a\"\nlist_fields = [\"a\", \"b\"]\n{fields}"
        ))
        .unwrap();
        assert_eq!(d.title_field.as_deref(), Some("a"));
        assert_eq!(d.list_fields, Some(vec!["a".to_owned(), "b".to_owned()]));

        let title = |value: &str| invalid(&format!("{base}title_field = \"{value}\"\n{fields}"));
        assert_eq!(
            title("nope"),
            vec![DefinitionError::TitleFieldMissing {
                field: "nope".into()
            }]
        );
        for (name, found) in [("b", "check"), ("c", "text"), ("rows", "table")] {
            assert_eq!(
                title(name),
                vec![DefinitionError::TitleFieldKind {
                    field: name.into(),
                    found
                }]
            );
        }
        let list = |value: &str| invalid(&format!("{base}list_fields = {value}\n{fields}"));
        assert_eq!(
            list("[\"nope\"]"),
            vec![DefinitionError::ListFieldMissing {
                field: "nope".into()
            }]
        );
        assert_eq!(
            list("[\"c\", \"rows\"]"),
            vec![
                DefinitionError::ListFieldKind {
                    field: "c".into(),
                    found: "text"
                },
                DefinitionError::ListFieldKind {
                    field: "rows".into(),
                    found: "table"
                },
            ]
        );
        assert_eq!(
            list("[\"a\", \"a\"]"),
            vec![DefinitionError::DuplicateListField { field: "a".into() }]
        );
        assert_eq!(list("[]"), vec![DefinitionError::EmptyListFields]);
        assert_eq!(
            list("[\"a\", \"a\", \"a\", \"a\", \"a\", \"a\", \"a\", \"a\", \"a\"]").first(),
            Some(&DefinitionError::TooManyListFields { count: 9, max: 8 })
        );
        // Exactly the limit is fine.
        let names: Vec<String> = (1..=8).map(|i| format!("f{i}")).collect();
        let mut text = format!("{base}list_fields = {names:?}\n");
        for name in &names {
            text.push_str(&format!("[[fields]]\nname = \"{name}\"\ntype = \"int\"\n"));
        }
        assert_eq!(from_toml_str(&text).unwrap().list_fields.unwrap().len(), 8);
    }

    #[test]
    fn title_field_and_list_fields_are_refused_on_a_child() {
        let child = "name = \"row\"\nlabel = \"R\"\nis_child = true\ntitle_field = \"a\"\nlist_fields = [\"a\"]\n\
                     [[fields]]\nname = \"a\"\ntype = \"data\"\n";
        assert_eq!(
            invalid(child),
            vec![
                DefinitionError::ChildWithDeskKey { key: "title_field" },
                DefinitionError::ChildWithDeskKey { key: "list_fields" },
            ]
        );
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
            "{ series = 1 }",
            "{ series = \"A.#\", field = \"a\" }",
            "{ field = \"a\", x = 1 }",
            "[\"a\"]",
        ] {
            let errors = invalid(&format!("naming = {naming}\n{base}"));
            assert_eq!(errors, vec![DefinitionError::MalformedNaming], "{naming}");
            assert_eq!(
                errors[0].to_string(),
                "naming must be \"random\", { field = \"<fieldname>\" } or { series = \"<pattern>\" }"
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

    #[test]
    fn series_naming_is_parsed_and_checked() {
        let base = "name = \"t\"\nlabel = \"T\"\n";
        let d = from_toml_str(&format!(
            "naming = {{ series = \"INV-.YYYY.-.#####\" }}\n{base}"
        ))
        .unwrap();
        let Naming::Series(pattern) = &d.naming else {
            panic!("expected a series, got {:?}", d.naming);
        };
        assert_eq!(pattern.as_str(), "INV-.YYYY.-.#####");
        for bad in ["", "INV-", "A.#.#", "A..#", "A#.##"] {
            let errors = invalid(&format!("naming = {{ series = \"{bad}\" }}\n{base}"));
            assert!(
                matches!(
                    errors.as_slice(),
                    [DefinitionError::InvalidSeriesPattern { pattern, .. }] if pattern == bad
                ),
                "{bad:?}: {errors:?}"
            );
        }
        let message = invalid(&format!("naming = {{ series = \"INV-\" }}\n{base}"))[0].to_string();
        assert_eq!(
            message,
            "naming series pattern `INV-` needs exactly one number part such as `#####`, found none"
        );
    }

    #[test]
    fn a_child_cannot_use_a_series_or_be_submittable() {
        let base = "name = \"t\"\nlabel = \"T\"\nis_child = true\n";
        assert_eq!(
            invalid(&format!("naming = {{ series = \"A.#\" }}\n{base}")),
            vec![DefinitionError::ChildWithNaming]
        );
        assert_eq!(
            invalid(&format!("submittable = true\n{base}")),
            vec![DefinitionError::ChildSubmittable]
        );
    }

    #[test]
    fn submittable_rules() {
        let base = "name = \"t\"\nlabel = \"T\"\nsubmittable = true\n";
        let d = from_toml_str(base).unwrap();
        assert!(d.submittable);
        assert!(
            !from_toml_str("name = \"t\"\nlabel = \"T\"\n")
                .unwrap()
                .submittable
        );
        // Random naming and a series are fine, field naming is not.
        assert!(from_toml_str(&format!("naming = \"random\"\n{base}")).is_ok());
        assert!(from_toml_str(&format!("naming = {{ series = \"A.#\" }}\n{base}")).is_ok());
        let field =
            "[[fields]]\nname = \"code\"\ntype = \"data\"\nrequired = true\nunique = true\n";
        assert_eq!(
            invalid(&format!("naming = {{ field = \"code\" }}\n{base}{field}")),
            vec![DefinitionError::SubmittableWithFieldNaming]
        );
    }

    #[test]
    fn allow_on_submit_rules() {
        let submittable = "name = \"t\"\nlabel = \"T\"\nsubmittable = true\n";
        let plain = "name = \"t\"\nlabel = \"T\"\n";
        let field = |extra: &str| format!("[[fields]]\nname = \"f\"\n{extra}\n");
        let ok = from_toml_str(&format!(
            "{submittable}{}",
            field("type = \"text\"\nallow_on_submit = true")
        ))
        .unwrap();
        assert!(ok.fields[0].allow_on_submit);
        // Default is false.
        let d = from_toml_str(&format!("{submittable}{}", field("type = \"text\""))).unwrap();
        assert!(!d.fields[0].allow_on_submit);
        // Not meaningful without `submittable`: a definition error, not ignored.
        assert_eq!(
            invalid(&format!(
                "{plain}{}",
                field("type = \"text\"\nallow_on_submit = true")
            )),
            vec![DefinitionError::AllowOnSubmitNotSubmittable { field: "f".into() }]
        );
        // `allow_on_submit = false` is fine anywhere.
        assert!(
            from_toml_str(&format!(
                "{plain}{}",
                field("type = \"text\"\nallow_on_submit = false")
            ))
            .is_ok()
        );
        // A table is always locked.
        assert_eq!(
            invalid(&format!(
                "{submittable}{}",
                field("type = \"table\"\ntarget = \"row\"\nallow_on_submit = true")
            )),
            vec![DefinitionError::AllowOnSubmitTable { field: "f".into() }]
        );
    }

    #[test]
    fn docstatus_and_amended_from_are_reserved_field_names() {
        for name in ["docstatus", "amended_from"] {
            let errors = invalid(&format!(
                "name = \"t\"\nlabel = \"T\"\nsubmittable = true\n[[fields]]\nname = \"{name}\"\ntype = \"int\"\n"
            ));
            assert!(
                matches!(errors.as_slice(), [DefinitionError::InvalidFieldName { field, .. }] if field == name),
                "{errors:?}"
            );
        }
    }

    const PLAIN: &str = "name = \"t\"\nlabel = \"T\"\n";
    const SUBMITTABLE: &str = "name = \"t\"\nlabel = \"T\"\nsubmittable = true\n";

    fn rule(body: &str) -> String {
        format!("[[permissions]]\n{body}\n")
    }

    #[test]
    fn parses_permission_rules_with_false_defaults() {
        let text = format!(
            "{SUBMITTABLE}{}{}",
            rule("role = \"Sales User\"\nread = true\nwrite = true\ncreate = true\nsubmit = true"),
            rule("role = \"Auditor\"\nread = true\nif_owner = true")
        );
        let d = from_toml_str(&text).unwrap();
        assert_eq!(
            d.permissions,
            vec![
                PermissionRule {
                    read: true,
                    write: true,
                    create: true,
                    submit: true,
                    ..PermissionRule::new("Sales User")
                },
                PermissionRule {
                    read: true,
                    if_owner: true,
                    ..PermissionRule::new("Auditor")
                },
            ]
        );
        // No rules at all is valid: deny by default.
        assert!(from_toml_str(PLAIN).unwrap().permissions.is_empty());
    }

    #[test]
    fn permission_rules_reject_unknown_keys_and_missing_roles() {
        for body in [
            "role = \"A\"\nread = true\nexecute = true",
            "read = true",
            "role = \"A\"\nread = \"yes\"",
        ] {
            let result = from_toml_str(&format!("{PLAIN}{}", rule(body)));
            assert!(
                matches!(result, Err(MetaError::Parse(_))),
                "{body}: {result:?}"
            );
        }
    }

    #[test]
    fn permission_rule_definition_errors() {
        // Blank role, a rule that grants nothing.
        let errors = invalid(&format!("{PLAIN}{}", rule("role = \"  \"")));
        assert_eq!(
            errors,
            vec![
                DefinitionError::InvalidPermissionRole {
                    role: "  ".into(),
                    reason: "must not be blank"
                },
                DefinitionError::EmptyPermissionRule { role: "  ".into() },
            ]
        );
        let errors = invalid(&format!("{PLAIN}{}", rule("role = \"a\\tb\"\nread = true")));
        assert!(
            matches!(
                errors.as_slice(),
                [DefinitionError::InvalidPermissionRole { .. }]
            ),
            "{errors:?}"
        );
        assert_eq!(
            invalid(&format!("{PLAIN}{}", rule("role = \"A\""))),
            vec![DefinitionError::EmptyPermissionRule { role: "A".into() }]
        );
        // submit, cancel and amend need a submittable DocType.
        assert_eq!(
            invalid(&format!(
                "{PLAIN}{}",
                rule("role = \"A\"\nsubmit = true\ncancel = true\namend = true")
            )),
            vec![
                DefinitionError::PermissionNeedsSubmittable {
                    role: "A".into(),
                    right: crate::permission::Right::Submit
                },
                DefinitionError::PermissionNeedsSubmittable {
                    role: "A".into(),
                    right: crate::permission::Right::Cancel
                },
                DefinitionError::PermissionNeedsSubmittable {
                    role: "A".into(),
                    right: crate::permission::Right::Amend
                },
            ]
        );
        assert!(
            from_toml_str(&format!(
                "{SUBMITTABLE}{}",
                rule("role = \"A\"\nsubmit = true\ncancel = true\namend = true")
            ))
            .is_ok()
        );
        // Same role and if_owner twice; a different if_owner is fine.
        let twice = format!(
            "{PLAIN}{}{}",
            rule("role = \"A\"\nread = true"),
            rule("role = \"A\"\nwrite = true")
        );
        assert_eq!(
            invalid(&twice),
            vec![DefinitionError::DuplicatePermissionRule {
                role: "A".into(),
                if_owner: false
            }]
        );
        let both = format!(
            "{PLAIN}{}{}",
            rule("role = \"A\"\nread = true"),
            rule("role = \"A\"\nwrite = true\nif_owner = true")
        );
        assert!(from_toml_str(&both).is_ok());
        // The Administrator role is allowed (pointless, but not wrong).
        assert!(
            from_toml_str(&format!(
                "{PLAIN}{}",
                rule("role = \"Administrator\"\nread = true")
            ))
            .is_ok()
        );
    }

    #[test]
    fn a_child_doctype_cannot_have_permission_rules() {
        let child = "name = \"row\"\nlabel = \"R\"\nis_child = true\n";
        assert_eq!(
            invalid(&format!("{child}{}", rule("role = \"A\"\nread = true"))),
            vec![DefinitionError::ChildWithPermissions]
        );
        assert!(from_toml_str(child).is_ok());
    }
}
