//! The `forgebase` command-line binary.
//!
//! Commands: `migrate` (framework migrations and DocType schema sync) and `doc`
//! (`insert`, `get`, `update`, `delete` of single documents). The binary is a trusted local
//! tool: it has no authentication or permissions, and `--actor` is only recorded. See
//! `docs/blueprint/architecture.md` and ADR 0002.
//!
//! Exit codes: 0 success; 1 runtime, validation or database error (message on standard error,
//! starting with `error:`); 2 usage error (from clap).

mod args;
mod doctypes;
mod error;
mod json;
mod report;
mod run;

use std::io::Write;
use std::process::ExitCode;

use clap::Parser;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = args::Cli::parse();
    match run::run(cli.command).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "error: {error}");
            ExitCode::from(1)
        }
    }
}
