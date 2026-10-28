//! Identifier rules for DocType and field names.
//!
//! Names are plain snake_case ASCII identifiers (`^[a-z][a-z0-9_]*$`) so they can be used
//! safely to derive SQL identifiers. This module only validates and builds strings; quoting
//! them for SQL belongs to `forgebase-db`.

/// Prefix of every DocType table name.
pub const TABLE_PREFIX: &str = "doc_";

/// Maximum DocType name length in bytes: PostgreSQL identifiers hold 63 bytes and the table
/// name is `doc_<name>`.
pub const MAX_DOCTYPE_NAME_LEN: usize = 63 - TABLE_PREFIX.len();

/// Maximum field (column) name length in bytes.
pub const MAX_FIELD_NAME_LEN: usize = 63;

/// Names a user field may not use: the standard columns present on every DocType table, and
/// the PostgreSQL system columns (`tableoid` ... `ctid`), which `CREATE TABLE` rejects.
pub const RESERVED_FIELD_NAMES: [&str; 16] = [
    "id",
    "name",
    "owner",
    "created_at",
    "modified_at",
    "modified_by",
    "version",
    "docstatus",
    "idx",
    "parent_id",
    "tableoid",
    "xmin",
    "xmax",
    "cmin",
    "cmax",
    "ctid",
];

/// Why an identifier was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdentError {
    /// The identifier is the empty string.
    #[error("must not be empty")]
    Empty,
    /// The identifier does not match `^[a-z][a-z0-9_]*$`.
    #[error(
        "must match ^[a-z][a-z0-9_]*$ (lowercase ASCII letters, digits and underscores, starting with a letter)"
    )]
    InvalidFormat,
    /// The identifier is longer than the allowed number of bytes.
    #[error("is {len} bytes long, the maximum is {max}")]
    TooLong {
        /// Actual length in bytes.
        len: usize,
        /// Maximum length in bytes.
        max: usize,
    },
    /// The identifier is the name of a standard or PostgreSQL system column.
    #[error("is a reserved column name")]
    Reserved,
}

/// Validates a DocType name (`^[a-z][a-z0-9_]*$`, at most 59 bytes).
pub fn validate_doctype_name(name: &str) -> Result<(), IdentError> {
    validate_pattern(name, MAX_DOCTYPE_NAME_LEN)
}

/// Validates a field name (`^[a-z][a-z0-9_]*$`, at most 63 bytes, not a reserved column name).
pub fn validate_field_name(name: &str) -> Result<(), IdentError> {
    validate_pattern(name, MAX_FIELD_NAME_LEN)?;
    if RESERVED_FIELD_NAMES.contains(&name) {
        return Err(IdentError::Reserved);
    }
    Ok(())
}

/// Returns the table name for a DocType (`doc_<name>`), unquoted.
///
/// The name must already have passed [`validate_doctype_name`].
pub fn table_name(doctype: &str) -> String {
    format!("{TABLE_PREFIX}{doctype}")
}

fn validate_pattern(name: &str, max_len: usize) -> Result<(), IdentError> {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return Err(IdentError::Empty);
    };
    if !first.is_ascii_lowercase()
        || !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(IdentError::InvalidFormat);
    }
    if name.len() > max_len {
        return Err(IdentError::TooLong {
            len: name.len(),
            max: max_len,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctype_names() {
        let at_limit = format!("a{}", "b".repeat(58));
        let over_limit = format!("a{}", "b".repeat(59));
        let cases: Vec<(&str, Result<(), IdentError>)> = vec![
            ("customer", Ok(())),
            ("sales_order_2", Ok(())),
            ("a", Ok(())),
            // "id" is only reserved as a field name.
            ("id", Ok(())),
            (at_limit.as_str(), Ok(())),
            (
                over_limit.as_str(),
                Err(IdentError::TooLong { len: 60, max: 59 }),
            ),
            ("", Err(IdentError::Empty)),
            ("Customer", Err(IdentError::InvalidFormat)),
            ("1customer", Err(IdentError::InvalidFormat)),
            ("_customer", Err(IdentError::InvalidFormat)),
            ("sales-order", Err(IdentError::InvalidFormat)),
            ("sales order", Err(IdentError::InvalidFormat)),
            ("caf\u{e9}", Err(IdentError::InvalidFormat)),
            ("a\"; drop table x; --", Err(IdentError::InvalidFormat)),
        ];
        for (name, expected) in cases {
            assert_eq!(validate_doctype_name(name), expected, "name {name:?}");
        }
        assert_eq!(at_limit.len(), 59);
        assert_eq!(table_name(&at_limit).len(), 63);
    }

    #[test]
    fn field_names() {
        let at_limit = format!("a{}", "b".repeat(62));
        let over_limit = format!("a{}", "b".repeat(63));
        let cases: Vec<(&str, Result<(), IdentError>)> = vec![
            ("email", Ok(())),
            ("address_line_2", Ok(())),
            (at_limit.as_str(), Ok(())),
            (
                over_limit.as_str(),
                Err(IdentError::TooLong { len: 64, max: 63 }),
            ),
            ("", Err(IdentError::Empty)),
            ("Email", Err(IdentError::InvalidFormat)),
            ("9lives", Err(IdentError::InvalidFormat)),
        ];
        for (name, expected) in cases {
            assert_eq!(validate_field_name(name), expected, "name {name:?}");
        }
    }

    #[test]
    fn every_reserved_name_is_rejected_as_field_name() {
        for name in RESERVED_FIELD_NAMES {
            assert_eq!(
                validate_field_name(name),
                Err(IdentError::Reserved),
                "name {name:?}"
            );
        }
        assert_eq!(RESERVED_FIELD_NAMES.len(), 16);
    }

    #[test]
    fn postgres_system_columns_are_reserved() {
        for name in ["tableoid", "xmin", "xmax", "cmin", "cmax", "ctid"] {
            assert_eq!(
                validate_field_name(name),
                Err(IdentError::Reserved),
                "name {name:?}"
            );
        }
    }

    #[test]
    fn table_name_has_prefix() {
        assert_eq!(table_name("sales_order"), "doc_sales_order");
    }
}
