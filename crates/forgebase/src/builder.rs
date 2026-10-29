//! [`Forgebase`]: the binary of an application, composed of apps.

use std::io::Write;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, error::ErrorKind};

use crate::app::App;
use crate::args::Cli;
use crate::compose::{StartupError, compose};
use crate::run::run;

/// A Forgebase binary: the apps compiled into it, and the command line that runs them.
///
/// ```ignore
/// fn main() -> std::process::ExitCode {
///     forgebase::Forgebase::new()
///         .app(crm::app())                 // first: sales depends on it
///         .app(sales::app())
///         .run()
/// }
/// ```
///
/// The apps are listed **explicitly and in order**: an app must come after every app it
/// depends on (nothing is sorted), and the DocType hooks of an app that extends another run
/// after the hooks of the app it extends. The stock `forgebase` binary has no apps and uses
/// `--doctypes DIR` instead; a binary with apps accepts `--doctypes DIR` too, as one more app
/// named `local` that depends on all the others.
#[derive(Clone, Default)]
pub struct Forgebase {
    pub(crate) apps: Vec<App>,
}

impl Forgebase {
    /// A binary without apps.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an app after the ones added before.
    pub fn app(mut self, app: App) -> Self {
        self.apps.push(app);
        self
    }

    /// Checks that the apps fit together, without a database: valid and unique names,
    /// dependencies present and registered before their dependents, no DocType defined twice,
    /// valid DocType files, Links only within an app and its dependencies, hooks only for
    /// DocTypes of the app and its dependencies, increasing migration versions. Every problem
    /// is in the error. [`Forgebase::run`] does this at startup; an app can also call it in a
    /// test.
    pub fn check(&self) -> Result<(), StartupError> {
        compose(&self.apps, None).map(drop)
    }

    /// Runs the command line (`std::env::args`) and returns the process exit code: 0 on
    /// success, 1 on a startup, runtime, validation or database error (the message goes to
    /// standard error, starting with `error:`), 2 on a usage error. See the crate
    /// documentation for the commands.
    pub fn run(self) -> ExitCode {
        let cli = Cli::parse();
        // A binary without apps has nothing but --doctypes to learn DocTypes from.
        if self.apps.is_empty() && cli.command.doctypes().is_some_and(|d| d.doctypes.is_none()) {
            Cli::command()
                .error(
                    ErrorKind::MissingRequiredArgument,
                    "the following required arguments were not provided:\n  --doctypes <DIR>",
                )
                .exit();
        }
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => return fail(&format!("cannot start the async runtime: {error}")),
        };
        match runtime.block_on(run(cli.command, &self.apps)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => fail(&error.to_string()),
        }
    }
}

fn fail(message: &str) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "error: {message}");
    ExitCode::from(1)
}
