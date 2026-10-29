//! The `forgebase` command-line binary.
//!
//! Commands: `migrate` (framework migrations and DocType schema sync), `doc` (`insert`, `get`,
//! `list`, `update`, `delete`, `submit`, `cancel` and `amend` of documents; `update`, `delete`,
//! `submit`, `cancel` and `amend` need `--version`), `user` (`create`, `set-password`,
//! `add-role`, `remove-role`, `disable`, `enable`; passwords come from standard input) and
//! `token` (`create`, `revoke`).
//!
//! The binary is a trusted local tool: `doc` commands run as the **system context**
//! (`forgebase_core::permission::Caller::System`), so no permission rules apply to them, and
//! `--actor` only names who is recorded (in `owner` / `modified_by`; default `cli`). It does not
//! authenticate anyone. Real users and their permissions arrive with the HTTP API (M3.3). See
//! `docs/blueprint/architecture.md`, ADR 0002 and ADR 0011.
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
