//! The `forgebase` command-line binary. Depends on `forgebase-http`.
//! See ADR 0002 and `docs/blueprint/architecture.md`.

fn version_line() -> String {
    format!("forgebase {}", env!("CARGO_PKG_VERSION"))
}

fn main() {
    println!("{}", version_line());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_has_name_and_version() {
        assert_eq!(
            version_line(),
            format!("forgebase {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
