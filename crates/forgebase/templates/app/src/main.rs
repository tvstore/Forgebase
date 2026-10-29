use std::process::ExitCode;

// The whole wiring: the apps of this binary, in dependency order.
fn main() -> ExitCode {
    forgebase::Forgebase::new().app({{name}}::app()).run()
}
