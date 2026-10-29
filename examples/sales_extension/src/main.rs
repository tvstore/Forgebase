use std::process::ExitCode;

// The composed binary: the framework, then `sales`, then this app (a dependency is registered
// before the apps that depend on it).
fn main() -> ExitCode {
    forgebase::Forgebase::new()
        .app(sales::app())
        .app(sales_extension::app())
        .run()
}
