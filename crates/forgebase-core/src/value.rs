//! The dynamic value type used for document fields.

use rust_decimal::Decimal;
use time::{Date, OffsetDateTime};

/// A single field value in a [`Document`](crate::document::Document).
///
/// `Select` fields hold their value as [`Value::Text`]; so do Link fields, which hold the
/// `name` of the target document.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Text, used by Data, Text, Select and Link fields.
    Text(String),
    /// A 64-bit integer (Int fields).
    Int(i64),
    /// A fixed-point decimal number (Decimal fields).
    Decimal(Decimal),
    /// A boolean (Check fields).
    Bool(bool),
    /// A calendar date (Date fields).
    Date(Date),
    /// A point in time with an offset (Datetime fields).
    Datetime(OffsetDateTime),
    /// No value.
    Null,
}

impl Value {
    /// Short name of the variant, used in error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            Value::Text(_) => "text",
            Value::Int(_) => "int",
            Value::Decimal(_) => "decimal",
            Value::Bool(_) => "bool",
            Value::Date(_) => "date",
            Value::Datetime(_) => "datetime",
            Value::Null => "null",
        }
    }
}
