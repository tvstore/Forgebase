//! The `forgebase` command-line binary. Depends on `forgebase-http`.
//! See ADR 0002 and `docs/blueprint/architecture.md`.

fn main() {
    println!("forgebase {}", env!("CARGO_PKG_VERSION"));
}
