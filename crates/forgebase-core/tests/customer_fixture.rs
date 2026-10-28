//! Loads the example DocType file and exercises the public API end to end.

use std::str::FromStr;

use forgebase_core::document::{Document, ValidationError, validate};
use forgebase_core::ident::table_name;
use forgebase_core::meta::{DocType, FieldType, Naming, Registry};
use forgebase_core::value::Value;
use rust_decimal::Decimal;
use time::{Date, Month, OffsetDateTime};

const CUSTOMER: &str = include_str!("fixtures/customer.toml");

fn customer() -> DocType {
    DocType::from_toml_str(CUSTOMER).expect("fixture is valid")
}

#[test]
fn fixture_loads() {
    let d = customer();
    assert_eq!(d.name, "customer");
    assert_eq!(table_name(&d.name), "doc_customer");
    assert_eq!(d.naming, Naming::Field("email".into()));
    assert_eq!(d.fields.len(), 9);
    assert_eq!(
        d.field("full_name").unwrap().field_type,
        FieldType::Data { max_length: 140 }
    );
    assert_eq!(d.validate(), Ok(()));

    let mut registry = Registry::new();
    registry.add(d.clone()).unwrap();
    assert!(registry.add(d).is_err());
}

#[test]
fn fixture_validates_documents() {
    let d = customer();
    let registry = Registry::new();
    let mut doc = Document::default();
    doc.values
        .insert("email".into(), Value::Text("a@example.com".into()));
    doc.values
        .insert("full_name".into(), Value::Text("Ada".into()));
    doc.values.insert(
        "credit_limit".into(),
        Value::Decimal(Decimal::from_str("1500.50").unwrap()),
    );
    doc.values.insert(
        "joined_on".into(),
        Value::Date(Date::from_calendar_date(2026, Month::October, 28).unwrap()),
    );
    doc.values.insert(
        "last_seen_at".into(),
        Value::Datetime(OffsetDateTime::UNIX_EPOCH),
    );
    doc.values.insert("tier".into(), Value::Text("gold".into()));
    assert_eq!(validate(&registry, &d, &doc), Ok(()));

    doc.values
        .insert("tier".into(), Value::Text("platinum".into()));
    doc.values.remove("email");
    assert_eq!(
        validate(&registry, &d, &doc),
        Err(vec![
            ValidationError::Required {
                field: "email".into()
            },
            ValidationError::InvalidOption {
                field: "tier".into(),
                value: "platinum".into()
            },
        ])
    );
}
