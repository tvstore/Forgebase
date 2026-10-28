//! SQL identifier helpers.

/// Quotes an identifier for SQL: wraps it in double quotes and doubles any quote inside.
///
/// Identifiers are already validated by `forgebase-core`; quoting every one anyway is defense
/// in depth.
pub fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("doc_customer"), "\"doc_customer\"");
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(
            quote_ident("x\"; DROP TABLE t; --"),
            "\"x\"\"; DROP TABLE t; --\""
        );
        assert_eq!(quote_ident(""), "\"\"");
    }
}
