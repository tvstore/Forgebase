[package]
name = "{{name}}"
version = "0.1.0"
edition = "2024"
rust-version = "{{rust_version}}"
publish = false

[dependencies]
forgebase = { path = "{{forgebase_path}}" }

[dev-dependencies]
# `testing` gives the tests a throwaway database and an in-process HTTP client. It is a
# dev-dependency, so it is not compiled into the binary you ship.
forgebase = { path = "{{forgebase_path}}", features = ["testing"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

# This crate is its own workspace: a Cargo.toml in a parent directory does not take it over.
[workspace]
