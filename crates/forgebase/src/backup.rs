//! `site backup` and `site restore` (M7.3, ADR 0014): `pg_dump` and `pg_restore` run as child
//! processes, never bundled.
//!
//! **Secrets.** The database URL and the password are never on a command line (where `ps` shows
//! them) and never in a message: the child gets the connection through `PG*` environment
//! variables (`forgebase_db::pg_environment`), and every other `PG*` variable of this process is
//! removed from its environment, so a `PGSERVICE`, `PGPASSFILE` or `PGOPTIONS` of the operator
//! cannot change where it connects.
//!
//! **Partial files.** A backup reserves its file with an exclusive create (mode 0600 on Unix: the
//! file holds password hashes), so it never overwrites anything, and removes it again when
//! anything fails.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use forgebase_db::{DbError, inspect_database, pg_environment};

use crate::config::Site;
use crate::error::CliError;

/// The environment variable with the directory of the PostgreSQL client tools. Unset: `PATH`.
pub(crate) const PG_BIN_VAR: &str = "FORGEBASE_PG_BIN";

/// The environment of a child tool: the `PG*` variables of `connection`, and nothing else
/// inherited under that prefix.
type PgEnv = Vec<(&'static str, String)>;

/// A client tool and where to find it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    Dump,
    Restore,
}

impl Tool {
    fn name(self) -> &'static str {
        match self {
            Tool::Dump => "pg_dump",
            Tool::Restore => "pg_restore",
        }
    }
}

/// The program to start for `tool`: in `bin_dir` when it is given, else the bare name (`PATH`).
fn program(tool: Tool, bin_dir: Option<&Path>) -> PathBuf {
    bin_dir.map_or_else(|| PathBuf::from(tool.name()), |dir| dir.join(tool.name()))
}

fn not_found(tool: Tool) -> CliError {
    CliError::Backup(format!(
        "{0} not found; install the PostgreSQL client tools of version >= the server's (see \
         docs/guide/operations.md); set {PG_BIN_VAR} to their directory if they are not on PATH",
        tool.name()
    ))
}

fn start_error(tool: Tool, error: &std::io::Error) -> CliError {
    if error.kind() == std::io::ErrorKind::NotFound {
        not_found(tool)
    } else {
        CliError::Backup(format!("cannot start {}: {error}", tool.name()))
    }
}

/// A command for `tool` with the clean `PG*` environment of `env`.
fn command(tool: Tool, bin_dir: Option<&Path>, env: &PgEnv) -> Command {
    let mut command = Command::new(program(tool, bin_dir));
    for (name, _) in std::env::vars_os() {
        if name.to_string_lossy().starts_with("PG") {
            command.env_remove(name);
        }
    }
    for (name, value) in env {
        command.env(name, value);
    }
    command.stdin(Stdio::null());
    command
}

/// The arguments of `pg_dump`. No URL, no password: the connection is in the environment.
pub(crate) fn dump_args(file: &Path) -> Vec<OsString> {
    let mut file_option = OsString::from("--file=");
    file_option.push(file);
    vec![
        "--format=custom".into(),
        "--no-owner".into(),
        "--no-privileges".into(),
        file_option,
    ]
}

/// The arguments of `pg_restore`. `pg_restore` restores into a database only when it is given
/// one, and does not read `PGDATABASE` for that, so the (validated) database name is an
/// argument: it is no secret and cannot be read as a connection string (see
/// `forgebase_db::pg_environment`).
pub(crate) fn restore_args(database: &str, file: &Path) -> Vec<OsString> {
    vec![
        "--no-owner".into(),
        "--no-privileges".into(),
        "--exit-on-error".into(),
        "--single-transaction".into(),
        format!("--dbname={database}").into(),
        file_argument(file),
    ]
}

/// A file name as a positional argument: one that starts with `-` must not be read as an option.
fn file_argument(file: &Path) -> OsString {
    if file.to_string_lossy().starts_with('-') {
        Path::new(".").join(file).into_os_string()
    } else {
        file.as_os_str().to_owned()
    }
}

/// The major version in the first line of `pg_dump --version` and its relatives, for example
/// `pg_dump (PostgreSQL) 16.4 (Ubuntu 16.4-1)` gives 16, and `17devel` gives 17.
pub(crate) fn parse_major(text: &str) -> Option<u32> {
    let line = text.lines().next()?;
    let version = line
        .split_whitespace()
        .find(|word| word.starts_with(|c: char| c.is_ascii_digit()))?;
    let digits: String = version.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// The version of the program that wrote a dump, from the header that `pg_restore --list`
/// prints (`;     Dumped by pg_dump version: 16.4 (...)`).
pub(crate) fn dump_major(listing: &str) -> Option<u32> {
    let rest = listing.lines().find_map(|line| {
        let line = line.strip_prefix(';')?.trim_start();
        line.strip_prefix("Dumped by pg_dump version")
    })?;
    parse_major(rest.trim_start_matches(':').trim_start())
}

/// Why `tool_major` cannot work with a server or dump of `other_major`, or `None` when it can:
/// the tool must be of the same or a newer major version.
pub(crate) fn version_problem(
    tool: Tool,
    tool_major: u32,
    what: &str,
    other_major: u32,
) -> Option<String> {
    (tool_major < other_major).then(|| {
        format!(
            "{} is version {tool_major}, older than {what} (version {other_major}); install the \
             PostgreSQL client tools of version {other_major} or newer (see \
             docs/guide/operations.md)",
            tool.name()
        )
    })
}

/// The major version of `tool`, from `tool --version`.
fn tool_major(tool: Tool, bin_dir: Option<&Path>) -> Result<u32, CliError> {
    let output = command(tool, bin_dir, &Vec::new())
        .arg("--version")
        .output()
        .map_err(|e| start_error(tool, &e))?;
    parse_major(&String::from_utf8_lossy(&output.stdout)).ok_or_else(|| {
        CliError::Backup(format!(
            "cannot read the version of {} from `{} --version`",
            tool.name(),
            tool.name()
        ))
    })
}

/// Runs the tool to the end, copying each line of its standard error to ours, prefixed with the
/// tool's name. Returns whether it exited with success.
fn run_streaming(
    tool: Tool,
    bin_dir: Option<&Path>,
    env: &PgEnv,
    args: &[OsString],
) -> Result<bool, CliError> {
    let mut child = command(tool, bin_dir, env)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| start_error(tool, &e))?;
    let stderr = child.stderr.take().expect("stderr is piped");
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    while reader.read_until(b'\n', &mut line).unwrap_or(0) > 0 {
        let text = String::from_utf8_lossy(&line);
        // A closed standard error must not stop the tool half way: ignore write errors.
        let _ = writeln!(std::io::stderr(), "{}: {}", tool.name(), text.trim_end());
        line.clear();
    }
    let status = child
        .wait()
        .map_err(|e| CliError::Backup(format!("cannot wait for {}: {e}", tool.name())))?;
    Ok(status.success())
}

/// The directory of the client tools, from `FORGEBASE_PG_BIN` (unset or empty: `PATH`).
fn bin_dir() -> Option<PathBuf> {
    std::env::var_os(PG_BIN_VAR)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn site_error(site: &Site, source: DbError) -> CliError {
    CliError::Site {
        site: site.name.clone(),
        source: Box::new(source),
    }
}

/// Runs a blocking tool step off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, CliError> + Send + 'static,
) -> Result<T, CliError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| CliError::Backup(format!("the backup task failed: {e}")))?
}

/// `site backup`: the message to print. See [`backup`].
pub(crate) async fn site_backup(site: &Site, file: &Path) -> Result<String, CliError> {
    check_new_file(file)?;
    let url = site.database_url()?;
    let env = pg_environment(&url).map_err(|e| site_error(site, e))?;
    let info = inspect_database(&url)
        .await
        .map_err(|e| site_error(site, e))?;
    let (path, major) = (file.to_owned(), info.server_major());
    let done = blocking(move || backup(bin_dir().as_deref(), &env, major, &path)).await?;
    Ok(format!(
        "backed up site `{}` to {} ({} bytes)\n",
        site.name,
        file.display(),
        done.bytes
    ))
}

/// `site restore`: the message to print. The database must exist and be empty.
pub(crate) async fn site_restore(site: &Site, file: &Path) -> Result<String, CliError> {
    let name = &site.name;
    if !file.is_file() {
        return Err(CliError::Backup(format!(
            "{} is not a file; give the backup made by `forgebase site backup`",
            file.display()
        )));
    }
    let url = site.database_url()?;
    let env = pg_environment(&url).map_err(|e| site_error(site, e))?;
    let database = env
        .iter()
        .find_map(|(var, value)| (*var == "PGDATABASE").then(|| value.clone()))
        .expect("pg_environment always sets PGDATABASE");
    let info = match inspect_database(&url).await {
        Ok(info) => info,
        Err(e) if e.is_missing_database() => {
            return Err(CliError::Backup(format!(
                "the database of site `{name}` does not exist; create it with `forgebase site \
                 create {name} --empty`"
            )));
        }
        Err(e) => return Err(site_error(site, e)),
    };
    if !info.is_empty {
        return Err(CliError::Backup(format!(
            "the database of site `{name}` is not empty; restore only into an empty database \
             (create one with `forgebase site create {name} --empty`)"
        )));
    }
    let (path, major) = (file.to_owned(), info.server_major());
    blocking(move || restore(bin_dir().as_deref(), &env, &database, major, &path)).await?;
    Ok(format!(
        "restored {} into the database of site `{name}`\n\
         run `forgebase migrate --site {name}` if this binary is newer than the backup\n",
        file.display()
    ))
}

/// What a finished backup wrote.
pub(crate) struct Backup {
    pub bytes: u64,
}

/// Dumps the database of `env` into `file`. `server_major` is the server's major version, read
/// by the caller. Refuses an existing file; removes `file` again when anything fails. Blocking:
/// run it with `spawn_blocking`.
pub(crate) fn backup(
    bin_dir: Option<&Path>,
    env: &PgEnv,
    server_major: u32,
    file: &Path,
) -> Result<Backup, CliError> {
    let major = tool_major(Tool::Dump, bin_dir)?;
    if let Some(problem) = version_problem(Tool::Dump, major, "the server", server_major) {
        return Err(CliError::Backup(problem));
    }
    reserve(file)?;
    let result = run_streaming(Tool::Dump, bin_dir, env, &dump_args(file))
        .and_then(|ok| {
            ok.then_some(()).ok_or_else(|| {
                CliError::Backup("pg_dump failed; no backup file was written".to_owned())
            })
        })
        .and_then(|()| {
            std::fs::metadata(file)
                .map(|m| Backup { bytes: m.len() })
                .map_err(|e| CliError::Backup(format!("cannot read {}: {e}", file.display())))
        });
    if result.is_err() {
        let _ = std::fs::remove_file(file);
    }
    result
}

/// Creates `file`, empty, only if it does not exist (so a backup never overwrites anything, also
/// not by a race). Mode 0600 on Unix.
fn reserve(file: &Path) -> Result<(), CliError> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(file) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(CliError::Backup(format!(
            "refusing to overwrite {}",
            file.display()
        ))),
        Err(e) => Err(CliError::Backup(format!(
            "cannot create {}: {e}",
            file.display()
        ))),
    }
}

/// Refuses early, before any database or tool is touched, a backup file that exists already.
pub(crate) fn check_new_file(file: &Path) -> Result<(), CliError> {
    if file.symlink_metadata().is_ok() {
        return Err(CliError::Backup(format!(
            "refusing to overwrite {}",
            file.display()
        )));
    }
    Ok(())
}

/// Restores `file` into the database of `env` (`site restore`), after checking that
/// `pg_restore` is not older than the dump or the server. The caller has checked that the
/// database is empty. Blocking: run it with `spawn_blocking`.
pub(crate) fn restore(
    bin_dir: Option<&Path>,
    env: &PgEnv,
    database: &str,
    server_major: u32,
    file: &Path,
) -> Result<(), CliError> {
    let major = tool_major(Tool::Restore, bin_dir)?;
    if let Some(problem) = version_problem(Tool::Restore, major, "the server", server_major) {
        return Err(CliError::Backup(problem));
    }
    let listing = command(Tool::Restore, bin_dir, &Vec::new())
        .arg("--list")
        .arg(file_argument(file))
        .output()
        .map_err(|e| start_error(Tool::Restore, &e))?;
    if !listing.status.success() {
        return Err(CliError::Backup(format!(
            "{} is not a backup that pg_restore can read: {}",
            file.display(),
            String::from_utf8_lossy(&listing.stderr).trim()
        )));
    }
    let dumped_by = dump_major(&String::from_utf8_lossy(&listing.stdout)).ok_or_else(|| {
        CliError::Backup(format!(
            "cannot read the pg_dump version of {}; it is not a custom-format backup of `site backup`",
            file.display()
        ))
    })?;
    if let Some(problem) = version_problem(Tool::Restore, major, "the backup", dumped_by) {
        return Err(CliError::Backup(problem));
    }
    let args = restore_args(database, file);
    if run_streaming(Tool::Restore, bin_dir, env, &args)? {
        Ok(())
    } else {
        Err(CliError::Backup(
            "pg_restore failed; the restore ran in one transaction, so the database is unchanged"
                .to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_from_the_tool_output() {
        assert_eq!(
            parse_major("pg_dump (PostgreSQL) 16.4 (Ubuntu 16.4-1.pgdg24.04+1)\n"),
            Some(16)
        );
        assert_eq!(parse_major("pg_restore (PostgreSQL) 17devel"), Some(17));
        assert_eq!(parse_major("pg_dump (PostgreSQL) 9.6.24"), Some(9));
        assert_eq!(parse_major("pg_dump"), None);
        assert_eq!(parse_major(""), None);
        assert_eq!(
            dump_major(
                ";\n; Archive created at 2026-10-30\n; Dumped from database version: 16.4\n;     Dumped by pg_dump version: 17.0 (x)\n;\n"
            ),
            Some(17)
        );
        assert_eq!(dump_major("; nothing here\n"), None);
    }

    #[test]
    fn the_tool_must_be_the_same_or_newer() {
        assert!(version_problem(Tool::Dump, 16, "the server", 16).is_none());
        assert!(version_problem(Tool::Dump, 17, "the server", 16).is_none());
        let problem = version_problem(Tool::Dump, 15, "the server", 16).expect("older");
        assert!(
            problem.starts_with("pg_dump is version 15, older than the server (version 16)"),
            "{problem}"
        );
        let problem = version_problem(Tool::Restore, 16, "the backup", 17).expect("older");
        assert!(problem.contains("pg_restore") && problem.contains("backup"));
    }

    #[test]
    fn no_argument_carries_the_url_or_the_password() {
        let url = "postgres://ann:s3cretpw@db.example.com:6543/shop";
        let file = Path::new("/backups/shop.dump");
        let env = forgebase_db::pg_environment(url).expect("env");
        // The password is in the environment of the child, and only there.
        assert!(env.contains(&("PGPASSWORD", "s3cretpw".to_owned())));
        let database = env
            .iter()
            .find_map(|(name, value)| (*name == "PGDATABASE").then_some(value.as_str()))
            .expect("database");
        for args in [dump_args(file), restore_args(database, file)] {
            for arg in args {
                let arg = arg.to_string_lossy().into_owned();
                for secret in ["s3cretpw", "ann", "db.example.com", "6543", "postgres://"] {
                    assert!(!arg.contains(secret), "{arg} contains {secret}");
                }
            }
        }
    }

    #[test]
    fn restore_arguments_are_fixed_and_a_dash_file_is_not_an_option() {
        let args = restore_args("shop", Path::new("-odd.dump"));
        let args: Vec<_> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--no-owner",
                "--no-privileges",
                "--exit-on-error",
                "--single-transaction",
                "--dbname=shop",
                "./-odd.dump"
            ]
        );
    }

    #[test]
    fn an_existing_file_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("fb_backup_unit_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let file = dir.join("x.dump");
        std::fs::write(&file, "precious").expect("write");
        for result in [check_new_file(&file), reserve(&file)] {
            let message = result.expect_err("exists").to_string();
            assert_eq!(message, format!("refusing to overwrite {}", file.display()));
        }
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "precious");
        std::fs::remove_file(&file).expect("remove");
        check_new_file(&file).expect("free");
        reserve(&file).expect("reserve");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).expect("meta").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(reserve(&file).is_err());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_missing_tool_is_a_clear_error() {
        let dir = std::env::temp_dir().join(format!("fb_backup_nobin_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        for tool in [Tool::Dump, Tool::Restore] {
            let message = tool_major(tool, Some(&dir))
                .expect_err("missing")
                .to_string();
            assert!(
                message.starts_with(&format!(
                    "{} not found; install the PostgreSQL client tools of version >= the server's",
                    tool.name()
                )) && message.contains("docs/guide/operations.md"),
                "{message}"
            );
        }
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
