//! The stock `forgebase` binary: the framework without apps. DocTypes come from `--doctypes DIR`.
//! An application builds its own binary with its apps (see the `forgebase` crate documentation).

use std::process::ExitCode;

fn main() -> ExitCode {
    forgebase::Forgebase::new().run()
}
