//! Metadata overlays (M10.1, ADR 0023): fields that an app adds to a DocType of a dependency
//! (app overlays) and fields that a site adds to a DocType of its own database (site custom
//! fields). Both are parsed by the code that parses the `[[fields]]` of a DocType file, so a
//! field means the same thing everywhere; this module has the error type, the reserved prefix
//! and the public parsing functions. The rules that need the registry are
//! [`Registry::add_overlay_fields`](crate::meta::Registry::add_overlay_fields) and
//! [`Registry::add_custom_field`](crate::meta::Registry::add_custom_field).

use crate::load;
use crate::meta::{DefinitionError, DocField};

/// Every field name of a site custom field starts with this prefix, and no app field may
/// (DocType field and overlay field alike): the prefix keeps the two name spaces apart, so a
/// site can never collide with an app, now or after an app update.
pub const CUSTOM_PREFIX: &str = "custom_";

/// Why a field could not be added to a DocType by an overlay or as a custom field.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OverlayError {
    /// The text is not a valid field definition (not valid TOML or JSON, an unknown key, a wrong
    /// value type, a missing key). The message comes from the parser.
    #[error("cannot parse the definition: {0}")]
    Parse(String),
    /// An overlay file without any `[[fields]]`.
    #[error("the overlay has no [[fields]]")]
    NoFields,
    /// The definition parsed but breaks the rules of a field or of the DocType it joins; all
    /// problems are listed.
    #[error("invalid definition: {}", join(.0))]
    Invalid(Vec<DefinitionError>),
    /// The target DocType does not exist.
    #[error("unknown doctype `{0}`")]
    UnknownDocType(String),
    /// The target is a child DocType (M10.1 does not extend child DocTypes).
    #[error(
        "doctype `{0}` is a child DocType; fields can only be added to a DocType with documents of its own"
    )]
    ChildDocType(String),
    /// A Table field: the rows of a child DocType cannot be added afterwards.
    #[error("field `{0}` has type table; overlays and custom fields cannot add Table fields")]
    TableField(String),
    /// The DocType has a field of this name already.
    #[error("field `{field}` already exists on doctype `{doctype}`")]
    Collision {
        /// The target DocType.
        doctype: String,
        /// The field name.
        field: String,
    },
    /// An app field named like a site custom field.
    #[error("field `{0}` starts with `custom_`, a prefix reserved for site custom fields")]
    ReservedPrefix(String),
    /// A site custom field without the prefix.
    #[error("the name of a site custom field must start with `custom_` (got `{0}`)")]
    MissingPrefix(String),
    /// A site custom field with `required`.
    #[error(
        "field `{0}`: a site custom field cannot be required (existing documents have no value for it)"
    )]
    Required(String),
    /// A site custom field with `read_only`.
    #[error("field `{0}`: a site custom field cannot be read_only (no hook can write it)")]
    ReadOnly(String),
}

fn join(errors: &[DefinitionError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Parses the text of an overlay file: `[[fields]]` entries written like those of a DocType
/// file, and no other key. The fields are checked on their own here; the rules that involve the
/// target DocType apply when they are added to the registry.
pub fn parse_overlay(text: &str) -> Result<Vec<DocField>, OverlayError> {
    load::overlay_from_toml_str(text)
}

/// Parses one field written as the body of a TOML `[[fields]]` table.
pub fn parse_field_toml(text: &str) -> Result<DocField, OverlayError> {
    load::field_from_toml_str(text)
}

/// Parses one field written as a JSON object with the keys of a TOML `[[fields]]` table.
pub fn parse_field_json(value: &serde_json::Value) -> Result<DocField, OverlayError> {
    load::field_from_json(value)
}

/// The field as the JSON object that [`parse_field_json`] reads back (what a site stores).
pub fn field_to_json(field: &DocField) -> serde_json::Value {
    use crate::meta::FieldType;
    let mut map = serde_json::Map::new();
    map.insert("name".into(), field.name.clone().into());
    map.insert("label".into(), field.label.clone().into());
    map.insert("type".into(), field.field_type.name().into());
    match &field.field_type {
        FieldType::Data { max_length } => {
            map.insert("max_length".into(), (*max_length).into());
        }
        FieldType::Decimal { precision, scale } => {
            map.insert("precision".into(), (*precision).into());
            map.insert("scale".into(), (*scale).into());
        }
        FieldType::Select { options } => {
            map.insert("options".into(), options.clone().into());
        }
        FieldType::Link { target } | FieldType::Table { target } => {
            map.insert("target".into(), target.clone().into());
        }
        FieldType::Text
        | FieldType::Int
        | FieldType::Check
        | FieldType::Date
        | FieldType::Datetime => {}
    }
    for (key, on) in [
        ("required", field.required),
        ("unique", field.unique),
        ("allow_on_submit", field.allow_on_submit),
        ("read_only", field.read_only),
    ] {
        if on {
            map.insert(key.into(), true.into());
        }
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::FieldType;

    #[test]
    fn an_overlay_is_only_fields() {
        let fields = parse_overlay(
            "[[fields]]\nname = \"tier\"\ntype = \"select\"\noptions = [\"a\", \"b\"]\n\n[[fields]]\nname = \"vip\"\ntype = \"check\"\n",
        )
        .expect("valid");
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].label, "Tier");
        assert!(
            matches!(&fields[0].field_type, FieldType::Select { options } if options.len() == 2)
        );

        for bad in [
            "name = \"x\"\n[[fields]]\nname = \"a\"\ntype = \"check\"\n",
            "[[permissions]]\nrole = \"x\"\n",
            "[[fields]]\nname = \"a\"\ntype = \"check\"\ncolour = 1\n",
        ] {
            assert!(
                matches!(parse_overlay(bad), Err(OverlayError::Parse(_))),
                "{bad}"
            );
        }
        assert_eq!(parse_overlay(""), Err(OverlayError::NoFields));
    }

    #[test]
    fn a_field_gets_the_rules_of_a_doctype_field() {
        let err =
            parse_overlay("[[fields]]\nname = \"a\"\ntype = \"select\"\n").expect_err("options");
        assert!(matches!(err, OverlayError::Invalid(_)), "{err}");
        let err = parse_overlay("[[fields]]\nname = \"a\"\ntype = \"nope\"\n").expect_err("type");
        assert!(matches!(err, OverlayError::Invalid(_)), "{err}");
    }

    #[test]
    fn a_bad_name_is_refused_when_the_field_is_added() {
        let mut registry = registry();
        for name in ["Bad Name", "id", "1x", "custom_Bad"] {
            let field = parse_field_json(&serde_json::json!({"name": name, "type": "check"}))
                .expect("parses");
            assert!(
                matches!(
                    registry.add_custom_field("customer", field),
                    Err(OverlayError::Invalid(_) | OverlayError::MissingPrefix(_))
                ),
                "{name}"
            );
        }
    }

    #[test]
    fn toml_and_json_give_the_same_field() {
        let from_toml =
            parse_field_toml("name = \"custom_vip\"\ntype = \"check\"\nunique = true\n")
                .expect("toml");
        let from_json = parse_field_json(&serde_json::json!({
            "name": "custom_vip", "type": "check", "unique": true
        }))
        .expect("json");
        assert_eq!(from_toml, from_json);
        assert_eq!(
            parse_field_json(&field_to_json(&from_json)).expect("round trip"),
            from_json
        );
        assert!(matches!(
            parse_field_json(&serde_json::json!({"name": "a", "type": "check", "x": 1})),
            Err(OverlayError::Parse(_))
        ));
    }

    #[test]
    fn every_type_round_trips_through_json() {
        for text in [
            "name = \"a\"\ntype = \"data\"\nmax_length = 20\nrequired = true\n",
            "name = \"a\"\ntype = \"decimal\"\nprecision = 10\nscale = 2\n",
            "name = \"a\"\ntype = \"select\"\noptions = [\"x\", \"y\"]\nallow_on_submit = true\n",
            "name = \"a\"\ntype = \"link\"\ntarget = \"customer\"\nlabel = \"Cust\"\nread_only = true\n",
            "name = \"a\"\ntype = \"text\"\n",
            "name = \"a\"\ntype = \"int\"\n",
            "name = \"a\"\ntype = \"date\"\n",
            "name = \"a\"\ntype = \"datetime\"\n",
        ] {
            let field = parse_field_toml(text).expect("valid");
            assert_eq!(
                parse_field_json(&field_to_json(&field)),
                Ok(field),
                "{text}"
            );
        }
    }

    use crate::meta::{DocType, FieldSource, Registry};

    const CUSTOMER: &str = "name = \"customer\"\nlabel = \"Customer\"\n[[fields]]\nname = \"email\"\ntype = \"data\"\n";
    const ORDER: &str = "name = \"order\"\nlabel = \"Order\"\nsubmittable = true\n[[fields]]\nname = \"total\"\ntype = \"int\"\n[[fields]]\nname = \"lines\"\ntype = \"table\"\ntarget = \"line\"\n";
    const LINE: &str = "name = \"line\"\nlabel = \"Line\"\nis_child = true\n";

    fn registry() -> Registry {
        let mut registry = Registry::new();
        for text in [CUSTOMER, ORDER, LINE] {
            registry
                .add_owned("sales", DocType::from_toml_str(text).expect("valid"))
                .expect("added");
        }
        registry
    }

    fn fields(text: &str) -> Vec<DocField> {
        parse_overlay(text).expect("valid overlay")
    }

    #[test]
    fn an_overlay_appends_in_file_order_and_records_the_owner() {
        let mut registry = registry();
        registry
            .add_overlay_fields(
                "ext",
                "customer",
                fields("[[fields]]\nname = \"b\"\ntype = \"int\"\n[[fields]]\nname = \"a\"\ntype = \"int\"\n"),
            )
            .expect("added");
        let names: Vec<_> = registry
            .get("customer")
            .expect("customer")
            .fields
            .iter()
            .map(|f| f.name.as_str())
            .collect();
        assert_eq!(names, ["email", "b", "a"]);
        assert_eq!(registry.field_owner("customer", "email"), Some("sales"));
        assert_eq!(registry.field_owner("customer", "b"), Some("ext"));
        assert_eq!(
            registry.field_source("customer", "b"),
            Some(&FieldSource::App("ext".to_owned()))
        );
        assert_eq!(registry.field_source("customer", "email"), None);
    }

    #[test]
    fn an_overlay_is_refused_all_or_nothing() {
        let mut registry = registry();
        let before = registry.get("customer").cloned();
        let err = registry
            .add_overlay_fields(
                "ext",
                "customer",
                fields("[[fields]]\nname = \"ok\"\ntype = \"int\"\n[[fields]]\nname = \"email\"\ntype = \"int\"\n"),
            )
            .expect_err("collision");
        assert_eq!(
            err,
            OverlayError::Collision {
                doctype: "customer".to_owned(),
                field: "email".to_owned()
            }
        );
        assert_eq!(registry.get("customer").cloned(), before);
        assert_eq!(registry.field_source("customer", "ok"), None);

        let twice = fields(
            "[[fields]]\nname = \"x\"\ntype = \"int\"\n[[fields]]\nname = \"x\"\ntype = \"int\"\n",
        );
        assert!(matches!(
            registry.add_overlay_fields("ext", "customer", twice),
            Err(OverlayError::Collision { .. })
        ));
    }

    #[test]
    fn overlay_refusals() {
        let mut registry = registry();
        let one = |text: &str| fields(text);
        assert_eq!(
            registry.add_overlay_fields(
                "ext",
                "nope",
                one("[[fields]]\nname = \"a\"\ntype = \"int\"\n")
            ),
            Err(OverlayError::UnknownDocType("nope".to_owned()))
        );
        assert_eq!(
            registry.add_overlay_fields(
                "ext",
                "line",
                one("[[fields]]\nname = \"a\"\ntype = \"int\"\n")
            ),
            Err(OverlayError::ChildDocType("line".to_owned()))
        );
        assert_eq!(
            registry.add_overlay_fields(
                "ext",
                "customer",
                one("[[fields]]\nname = \"rows\"\ntype = \"table\"\ntarget = \"line\"\n")
            ),
            Err(OverlayError::TableField("rows".to_owned()))
        );
        assert_eq!(
            registry.add_overlay_fields(
                "ext",
                "customer",
                one("[[fields]]\nname = \"custom_a\"\ntype = \"int\"\n")
            ),
            Err(OverlayError::ReservedPrefix("custom_a".to_owned()))
        );
        // The DocType's own rules still apply: allow_on_submit needs a submittable DocType.
        assert!(matches!(
            registry.add_overlay_fields(
                "ext",
                "customer",
                one("[[fields]]\nname = \"a\"\ntype = \"int\"\nallow_on_submit = true\n")
            ),
            Err(OverlayError::Invalid(_))
        ));
        registry
            .add_overlay_fields(
                "ext",
                "order",
                one("[[fields]]\nname = \"a\"\ntype = \"int\"\nallow_on_submit = true\nrequired = true\nunique = true\n"),
            )
            .expect("every property is allowed");
    }

    #[test]
    fn a_custom_field_has_its_own_rules() {
        let mut registry = registry();
        let field = |text: &str| parse_field_toml(text).expect("valid");
        assert_eq!(
            registry.add_custom_field("customer", field("name = \"vip\"\ntype = \"check\"\n")),
            Err(OverlayError::MissingPrefix("vip".to_owned()))
        );
        assert_eq!(
            registry.add_custom_field(
                "customer",
                field("name = \"custom_a\"\ntype = \"check\"\nrequired = true\n")
            ),
            Err(OverlayError::Required("custom_a".to_owned()))
        );
        assert_eq!(
            registry.add_custom_field(
                "customer",
                field("name = \"custom_a\"\ntype = \"check\"\nread_only = true\n")
            ),
            Err(OverlayError::ReadOnly("custom_a".to_owned()))
        );
        assert_eq!(
            registry.add_custom_field("line", field("name = \"custom_a\"\ntype = \"check\"\n")),
            Err(OverlayError::ChildDocType("line".to_owned()))
        );
        assert_eq!(
            registry.add_custom_field("nope", field("name = \"custom_a\"\ntype = \"check\"\n")),
            Err(OverlayError::UnknownDocType("nope".to_owned()))
        );
        assert!(matches!(
            registry.add_custom_field(
                "customer",
                field("name = \"custom_a\"\ntype = \"link\"\ntarget = \"ghost\"\n")
            ),
            Err(OverlayError::Invalid(_))
        ));
        assert!(matches!(
            registry.add_custom_field(
                "customer",
                field("name = \"custom_a\"\ntype = \"link\"\ntarget = \"line\"\n")
            ),
            Err(OverlayError::Invalid(_))
        ));
        assert!(
            registry
                .get("customer")
                .expect("customer")
                .field("custom_a")
                .is_none()
        );
        registry
            .add_custom_field(
                "customer",
                field(
                    "name = \"custom_ref\"\ntype = \"link\"\ntarget = \"order\"\nunique = true\n",
                ),
            )
            .expect("added");
        assert_eq!(
            registry.field_source("customer", "custom_ref"),
            Some(&FieldSource::Site)
        );
        assert_eq!(registry.field_owner("customer", "custom_ref"), None);
        assert_eq!(
            registry.add_custom_field(
                "customer",
                field("name = \"custom_ref\"\ntype = \"check\"\n")
            ),
            Err(OverlayError::Collision {
                doctype: "customer".to_owned(),
                field: "custom_ref".to_owned()
            })
        );
    }
}
