//! Executes the commands.

use std::io::{BufRead, Read, Write};
use std::time::Duration;

use forgebase_core::document::{Changes, Document};
use forgebase_core::meta::{DocType, Registry};
use forgebase_core::permission::Caller;
use forgebase_db::auth;
use forgebase_db::{DbError, ListQuery, StoredDocument, SyncOptions};

use std::sync::Arc;

use forgebase_http::{AppConfig, AppState};

use crate::app::App;
use crate::args::{
    ActorArg, Command, DocCommand, DocTypesDir, TokenCommand, UserCommand, VersionArg,
};
use crate::compose::{Composed, compose};
use crate::error::CliError;
use crate::job_cmd::job;
use crate::new_app::new_app;
use crate::report::migrate_report;
use crate::worker::{Worker, WorkerConfig, run_worker, worker_id};
use forgebase_http::json::{
    Input, document_to_json, documents_to_json, parse_filters, parse_input, parse_sort,
};

/// The system actor of commands that have no `--actor` (`doc get`, `doc list`): they only read.
const DEFAULT_ACTOR: &str = "cli";

/// Environment variable with the PostgreSQL URL.
const DATABASE_URL_VAR: &str = "FORGEBASE_DATABASE_URL";

/// Runs one command with the apps compiled into the binary. Every command first checks that
/// the apps (and the `--doctypes` directory) fit together; a startup error ends it with exit
/// code 1.
pub async fn run(command: Command, apps: &[App]) -> Result<(), CliError> {
    // The one command that needs neither the apps nor a database.
    if let Command::NewApp {
        name,
        forgebase_path,
        dir,
    } = &command
    {
        let created = new_app(name, forgebase_path, dir)?;
        return write_stdout(&crate::new_app::report(&created));
    }
    let url = database_url()?;
    match command {
        Command::Migrate {
            doctypes,
            dry_run,
            lock_timeout_ms,
        } => {
            let composed = composed(apps, &doctypes)?;
            let pool = forgebase_db::connect(&url).await?;
            let options = SyncOptions {
                dry_run,
                lock_timeout: Duration::from_millis(lock_timeout_ms),
            };
            let report = composed.migrate(&pool, &options).await?;
            write_stdout(&migrate_report(&report, dry_run, composed.app_count()))
        }
        Command::Doc(command) => execute(&url, prepare(command, apps)?).await,
        Command::User(command) => {
            compose(apps, None)?;
            user(&url, command).await
        }
        Command::Token(command) => {
            compose(apps, None)?;
            token(&url, command).await
        }
        Command::Serve { doctypes } => serve(&url, &doctypes, apps).await,
        Command::Worker {
            doctypes,
            concurrency,
        } => worker(&url, &doctypes, apps, concurrency).await,
        Command::Job(command) => {
            let composed = compose(apps, None)?;
            job(&url, &composed.registry, command).await
        }
        Command::NewApp { .. } => unreachable!("handled above"),
    }
}

/// A document operation with its input already read and checked, so that nothing touches the
/// database when the DocType, the JSON or standard input is wrong.
enum Operation {
    Insert {
        registry: Registry,
        doctype: String,
        input: Input,
        actor: String,
    },
    Get {
        registry: Registry,
        doctype: String,
        name: String,
    },
    List {
        registry: Registry,
        doctype: String,
        query: ListQuery,
    },
    Update {
        registry: Registry,
        doctype: String,
        name: String,
        expected_version: i64,
        input: Input,
        actor: String,
    },
    Delete {
        registry: Registry,
        doctype: String,
        name: String,
        expected_version: i64,
        actor: String,
    },
    /// Submit, cancel or amend: a document operation without data.
    Status {
        kind: StatusKind,
        registry: Registry,
        doctype: String,
        name: String,
        expected_version: i64,
        actor: String,
    },
}

#[derive(Clone, Copy)]
enum StatusKind {
    Submit,
    Cancel,
    Amend,
}

fn prepare(command: DocCommand, apps: &[App]) -> Result<Operation, CliError> {
    Ok(match command {
        DocCommand::Insert {
            doctype,
            doctypes,
            data,
            actor,
        } => {
            let (registry, found) = find_doctype(apps, &doctypes, &doctype)?;
            let input = parse_input(&registry, &found, &read_data(&data.data)?)?;
            Operation::Insert {
                registry,
                doctype: found.name,
                input,
                actor: actor.actor,
            }
        }
        DocCommand::Get {
            doctype,
            name,
            doctypes,
        } => {
            let (registry, found) = find_doctype(apps, &doctypes, &doctype)?;
            Operation::Get {
                registry,
                doctype: found.name,
                name,
            }
        }
        DocCommand::List {
            doctype,
            doctypes,
            filter,
            sort,
            limit,
            offset,
        } => {
            let (registry, found) = find_doctype(apps, &doctypes, &doctype)?;
            let query = ListQuery {
                filters: match filter {
                    Some(text) => parse_filters(&found, &text)?,
                    None => Vec::new(),
                },
                sort: parse_sort(&sort)?,
                limit,
                offset,
            };
            Operation::List {
                registry,
                doctype: found.name,
                query,
            }
        }
        DocCommand::Update {
            doctype,
            name,
            version,
            doctypes,
            data,
            actor,
        } => {
            let (registry, found) = find_doctype(apps, &doctypes, &doctype)?;
            let input = parse_input(&registry, &found, &read_data(&data.data)?)?;
            Operation::Update {
                registry,
                doctype: found.name,
                name,
                expected_version: version.expected_version,
                input,
                actor: actor.actor,
            }
        }
        DocCommand::Submit {
            doctype,
            name,
            version,
            doctypes,
            actor,
        } => status(
            apps,
            StatusKind::Submit,
            doctype,
            name,
            version,
            doctypes,
            actor,
        )?,
        DocCommand::Cancel {
            doctype,
            name,
            version,
            doctypes,
            actor,
        } => status(
            apps,
            StatusKind::Cancel,
            doctype,
            name,
            version,
            doctypes,
            actor,
        )?,
        DocCommand::Amend {
            doctype,
            name,
            version,
            doctypes,
            actor,
        } => status(
            apps,
            StatusKind::Amend,
            doctype,
            name,
            version,
            doctypes,
            actor,
        )?,
        DocCommand::Delete {
            doctype,
            name,
            version,
            doctypes,
            actor,
        } => {
            let (registry, found) = find_doctype(apps, &doctypes, &doctype)?;
            Operation::Delete {
                registry,
                doctype: found.name,
                name,
                expected_version: version.expected_version,
                actor: actor.actor,
            }
        }
    })
}

fn status(
    apps: &[App],
    kind: StatusKind,
    doctype: String,
    name: String,
    version: VersionArg,
    doctypes: DocTypesDir,
    actor: ActorArg,
) -> Result<Operation, CliError> {
    let (registry, found) = find_doctype(apps, &doctypes, &doctype)?;
    Ok(Operation::Status {
        kind,
        registry,
        doctype: found.name,
        name,
        expected_version: version.expected_version,
        actor: actor.actor,
    })
}

async fn execute(url: &str, operation: Operation) -> Result<(), CliError> {
    let pool = forgebase_db::connect(url).await?;
    let mut client = pool.get().await.map_err(DbError::from)?;
    let client = &mut **client;
    let stored = match operation {
        Operation::List {
            registry,
            doctype,
            query,
        } => {
            let caller = Caller::system(DEFAULT_ACTOR);
            let documents =
                forgebase_db::list(&*client, &registry, &doctype, &caller, &query).await?;
            let json = documents_to_json(&documents)?;
            let text = serde_json::to_string_pretty(&json)
                .map_err(forgebase_http::json::JsonError::Serialize)?;
            return write_stdout(&format!("{text}\n"));
        }
        Operation::Insert {
            registry,
            doctype,
            input,
            actor,
        } => {
            let document = Document {
                name: None,
                values: input.values,
                tables: input.tables,
            };
            forgebase_db::insert(
                client,
                &registry,
                &doctype,
                document,
                &Caller::system(actor),
            )
            .await?
        }
        Operation::Get {
            registry,
            doctype,
            name,
        } => forgebase_db::get(
            client,
            &registry,
            &doctype,
            &name,
            &Caller::system(DEFAULT_ACTOR),
        )
        .await?
        .ok_or(DbError::NotFound { doctype, name })?,
        Operation::Update {
            registry,
            doctype,
            name,
            expected_version,
            input,
            actor,
        } => {
            let changes = Changes {
                values: input.values,
                tables: input.tables,
            };
            forgebase_db::update(
                client,
                &registry,
                &doctype,
                &name,
                expected_version,
                &changes,
                &Caller::system(actor),
            )
            .await?
        }
        Operation::Status {
            kind,
            registry,
            doctype,
            name,
            expected_version,
            actor,
        } => {
            let doctype = doctype.as_str();
            let name = name.as_str();
            let caller = Caller::system(actor);
            match kind {
                StatusKind::Submit => {
                    forgebase_db::submit(
                        client,
                        &registry,
                        doctype,
                        name,
                        expected_version,
                        &caller,
                    )
                    .await?
                }
                StatusKind::Cancel => {
                    forgebase_db::cancel(
                        client,
                        &registry,
                        doctype,
                        name,
                        expected_version,
                        &caller,
                    )
                    .await?
                }
                StatusKind::Amend => {
                    forgebase_db::amend(client, &registry, doctype, name, expected_version, &caller)
                        .await?
                }
            }
        }
        Operation::Delete {
            registry,
            doctype,
            name,
            expected_version,
            actor,
        } => {
            return Ok(forgebase_db::delete(
                client,
                &registry,
                &doctype,
                &name,
                expected_version,
                &Caller::system(actor),
            )
            .await?);
        }
    };
    print_document(&stored)
}

/// Runs a `user` command. The password (if any) is read from standard input before the database
/// is touched, and never printed.
async fn user(url: &str, command: UserCommand) -> Result<(), CliError> {
    let password = match &command {
        UserCommand::Create { .. } | UserCommand::SetPassword { .. } => Some(read_password()?),
        _ => None,
    };
    let pool = forgebase_db::connect(url).await?;
    let mut client = pool.get().await.map_err(DbError::from)?;
    let client = &mut **client;
    let password = password.as_deref().unwrap_or_default();
    let text = match command {
        UserCommand::Create { username, roles } => {
            let roles: Vec<&str> = roles.iter().map(String::as_str).collect();
            auth::create_user(client, &username, password, &roles).await?;
            format!("created user {username}\n")
        }
        UserCommand::SetPassword { username } => {
            auth::set_password(client, &username, password).await?;
            format!("changed the password of user {username}; its sessions were ended\n")
        }
        UserCommand::AddRole { username, role } => {
            if auth::add_role(client, &username, &role).await? {
                format!("added role {role} to user {username}\n")
            } else {
                format!("user {username} has role {role} already\n")
            }
        }
        UserCommand::RemoveRole { username, role } => {
            if auth::remove_role(client, &username, &role).await? {
                format!("removed role {role} from user {username}\n")
            } else {
                format!("user {username} does not have role {role}\n")
            }
        }
        UserCommand::Disable { username } => {
            auth::set_enabled(client, &username, false).await?;
            format!("disabled user {username}; its sessions were ended\n")
        }
        UserCommand::Enable { username } => {
            auth::set_enabled(client, &username, true).await?;
            format!("enabled user {username}\n")
        }
    };
    write_stdout(&text)
}

/// How long `serve` waits for open connections after the first stop signal.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// Runs `serve`: the HTTP API until Ctrl-C or SIGTERM. Prints the listening address (never the
/// database URL or any secret). It does not run migrations.
///
/// The first signal stops accepting connections and lets running requests finish, for at most
/// [`SHUTDOWN_GRACE`]; a second signal, or the end of the grace period, exits at once.
async fn serve(url: &str, doctypes: &DocTypesDir, apps: &[App]) -> Result<(), CliError> {
    let registry = composed(apps, doctypes)?.registry;
    let listen = env_or("FORGEBASE_LISTEN", "127.0.0.1:8000");
    let hosts = env_or("FORGEBASE_ALLOWED_HOSTS", "localhost,127.0.0.1");
    let allowed_hosts = parse_allowed_hosts(&hosts)?;
    let insecure = std::env::var("FORGEBASE_DEV_INSECURE_COOKIES").is_ok_and(|v| v == "1");
    if insecure {
        let _ = writeln!(
            std::io::stderr(),
            "warning: FORGEBASE_DEV_INSECURE_COOKIES=1: the session cookie has no Secure attribute; \
             use this for local development over plain HTTP only"
        );
    }
    let desk_dir = desk_dir()?;
    let config = AppConfig {
        allowed_hosts,
        secure_cookies: !insecure,
        desk_dir: desk_dir.clone(),
        ..AppConfig::default()
    };
    let pool = forgebase_db::connect(url).await?;
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| CliError::Config(format!("cannot listen on {listen}: {e}")))?;
    let address = listener.local_addr().map_err(CliError::Serve)?;
    let signals = stop_signals().map_err(CliError::Serve)?;
    write_stdout(&format!("listening on http://{address}\n"))?;
    if let Some(dir) = &desk_dir {
        write_stdout(&format!(
            "serving the desk from {} at /desk/\n",
            dir.display()
        ))?;
    }
    let state = AppState::new(pool, Arc::new(registry), config);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = forgebase_http::serve(listener, state, async {
        let _ = stopped.await;
    });
    run_until_stopped(server, stop, signals, SHUTDOWN_GRACE)
        .await
        .map_err(CliError::Serve)
}

/// Runs `worker`: the jobs of the apps until Ctrl-C or SIGTERM (see `crate::worker`).
///
/// The pool has `concurrency + 2` connections: each running job holds one for its transaction,
/// and a claim or the record of a failure needs one briefly while the handlers hold theirs.
async fn worker(
    url: &str,
    doctypes: &DocTypesDir,
    apps: &[App],
    concurrency: u32,
) -> Result<(), CliError> {
    let composed = composed(apps, doctypes)?;
    if composed.jobs.is_empty() {
        return Err(CliError::Config(
            "this binary has no jobs: register them with App::job".to_owned(),
        ));
    }
    let size = usize::try_from(concurrency)
        .unwrap_or(usize::MAX)
        .saturating_add(2);
    let pool = forgebase_db::connect_sized(url, Some(size)).await?;
    let signals = stop_signals().map_err(CliError::Serve)?;
    let worker = Worker::new(
        pool,
        composed.registry,
        composed.jobs,
        WorkerConfig::default(),
    );
    run_worker(Arc::new(worker), concurrency, &worker_id(), signals).await;
    Ok(())
}

/// The directory of the built desk from `FORGEBASE_DESK_DIR` (typically `desk/dist`), or `None`
/// when it is unset or blank (no desk is served). It must be a directory that has an
/// `index.html`, so a wrong path fails at startup instead of giving 404s later.
fn desk_dir() -> Result<Option<std::path::PathBuf>, CliError> {
    let Some(value) = std::env::var_os("FORGEBASE_DESK_DIR").filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let dir = std::path::PathBuf::from(value);
    if !dir.join("index.html").is_file() {
        return Err(CliError::Config(format!(
            "FORGEBASE_DESK_DIR {} has no index.html (build the desk first: cd desk && npm ci && npm run build)",
            dir.display()
        )));
    }
    Ok(Some(dir))
}

/// The host names of `FORGEBASE_ALLOWED_HOSTS`: comma separated, trimmed, no empty entry. An
/// entry with a port can never match (the port of the `Host` header is ignored), so it is
/// refused at startup instead of silently locking everyone out.
fn parse_allowed_hosts(text: &str) -> Result<Vec<String>, CliError> {
    let hosts: Vec<String> = text
        .split(',')
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_owned)
        .collect();
    if hosts.is_empty() {
        return Err(CliError::Config(
            "FORGEBASE_ALLOWED_HOSTS has no host name".to_owned(),
        ));
    }
    for host in &hosts {
        // `[::1]` is a host name with colons; anything after its `]`, or a colon outside
        // brackets, is a port (or an unbracketed IPv6 address).
        let bad = if host.starts_with('[') {
            !host.ends_with(']')
        } else {
            host.contains(':')
        };
        if bad {
            return Err(CliError::Config(format!(
                "FORGEBASE_ALLOWED_HOSTS entry {host:?} must be a host name without a port \
                 (the port of the Host header is ignored; write IPv6 as [::1])"
            )));
        }
    }
    Ok(hosts)
}

/// A channel that receives one message per stop signal (Ctrl-C, and SIGTERM on Unix).
fn stop_signals() -> std::io::Result<tokio::sync::mpsc::UnboundedReceiver<()>> {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = interrupt.recv() => {}
                    _ = terminate.recv() => {}
                }
                if sender.send(()).is_err() {
                    break;
                }
            }
        });
    }
    #[cfg(not(unix))]
    tokio::spawn(async move {
        while tokio::signal::ctrl_c().await.is_ok() {
            if sender.send(()).is_err() {
                break;
            }
        }
    });
    Ok(receiver)
}

/// Runs `server` until it ends. After the first message on `signals` it sends `stop` (the
/// server's graceful shutdown) and waits for it at most `grace`; a second message or the end of
/// `grace` stops waiting, which drops the server and its open connections.
async fn run_until_stopped(
    server: impl std::future::Future<Output = std::io::Result<()>>,
    stop: tokio::sync::oneshot::Sender<()>,
    mut signals: tokio::sync::mpsc::UnboundedReceiver<()>,
    grace: Duration,
) -> std::io::Result<()> {
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => return result,
        Some(()) = signals.recv() => {}
    }
    let _ = stop.send(());
    let _ = writeln!(
        std::io::stderr(),
        "shutting down: waiting up to {} s for open requests (send the signal again to exit now)",
        grace.as_secs()
    );
    tokio::select! {
        result = &mut server => result,
        Some(()) = signals.recv() => forced("second signal"),
        () = tokio::time::sleep(grace) => forced("grace period over"),
    }
}

fn forced(why: &str) -> std::io::Result<()> {
    let _ = writeln!(
        std::io::stderr(),
        "exiting now ({why}); open connections are dropped"
    );
    Ok(())
}

fn env_or(name: &str, default: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => default.to_owned(),
    }
}

/// Runs a `token` command. `create` prints `key:secret` and nothing else.
async fn token(url: &str, command: TokenCommand) -> Result<(), CliError> {
    let pool = forgebase_db::connect(url).await?;
    let client = pool.get().await.map_err(DbError::from)?;
    let client = &**client;
    match command {
        TokenCommand::Create { username } => {
            let token = auth::create_api_token(client, &username).await?;
            write_stdout(&format!("{}:{}\n", token.key, token.secret))
        }
        TokenCommand::Revoke { key } => {
            auth::revoke_api_token(client, &key).await?;
            write_stdout(&format!("revoked API token {key}\n"))
        }
    }
}

/// The first line of standard input without its line ending. At most 4096 bytes are read, far
/// more than the longest valid password, so a runaway input cannot fill memory.
fn read_password() -> Result<String, CliError> {
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .take(4096)
        .read_line(&mut line)
        .map_err(CliError::Stdin)?;
    while line.ends_with(['\n', '\r']) {
        line.pop();
    }
    Ok(line)
}

fn database_url() -> Result<String, CliError> {
    match std::env::var(DATABASE_URL_VAR) {
        Ok(url) if !url.is_empty() => Ok(url),
        _ => Err(CliError::MissingDatabaseUrl(DATABASE_URL_VAR)),
    }
}

/// The apps of the binary plus the `--doctypes` directory, composed into one registry.
fn composed(apps: &[App], dir: &DocTypesDir) -> Result<Composed, CliError> {
    Ok(compose(apps, dir.doctypes.as_deref())?)
}

/// The registry of the apps and the directory, and the DocType called `name` in it.
fn find_doctype(
    apps: &[App],
    dir: &DocTypesDir,
    name: &str,
) -> Result<(Registry, DocType), CliError> {
    let registry = composed(apps, dir)?.registry;
    match registry.get(name) {
        Some(doctype) => {
            let doctype = doctype.clone();
            Ok((registry, doctype))
        }
        None => Err(CliError::UnknownDocType {
            name: name.to_owned(),
            known: if registry.is_empty() {
                "none".to_owned()
            } else {
                registry
                    .iter()
                    .map(|d| d.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        }),
    }
}

/// The JSON text of `--data`: the argument itself, or standard input for `-`.
pub(crate) fn read_data(arg: &str) -> Result<String, CliError> {
    if arg != "-" {
        return Ok(arg.to_owned());
    }
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(CliError::Stdin)?;
    Ok(text)
}

fn print_document(stored: &StoredDocument) -> Result<(), CliError> {
    let json = document_to_json(stored)?;
    // Serializing a `serde_json::Value` cannot fail (string keys only).
    let text =
        serde_json::to_string_pretty(&json).map_err(forgebase_http::json::JsonError::Serialize)?;
    write_stdout(&format!("{text}\n"))
}

/// Writes to standard output without panicking on a closed pipe.
pub(crate) fn write_stdout(text: &str) -> Result<(), CliError> {
    let mut out = std::io::stdout().lock();
    out.write_all(text.as_bytes())
        .and_then(|()| out.flush())
        .map_err(CliError::Output)
}

#[cfg(test)]
mod tests {
    use std::future::pending;
    use std::time::{Duration, Instant};

    use tokio::sync::{mpsc, oneshot};

    use super::{parse_allowed_hosts, run_until_stopped};

    #[test]
    fn allowed_hosts_are_trimmed_and_refuse_ports() {
        assert_eq!(
            parse_allowed_hosts(" a.example , [::1],,localhost ").expect("valid"),
            ["a.example", "[::1]", "localhost"]
        );
        for bad in ["", " , ", "localhost:8000", "[::1]:8000", "::1", "a,b:1"] {
            assert!(parse_allowed_hosts(bad).is_err(), "{bad:?}");
        }
    }

    /// A server that ignores the stop request, like one with a stuck connection.
    async fn stuck() -> std::io::Result<()> {
        pending().await
    }

    #[tokio::test]
    async fn a_second_signal_exits_at_once() {
        let (stop, _stopped) = oneshot::channel();
        let (signals, receiver) = mpsc::unbounded_channel();
        signals.send(()).expect("open");
        signals.send(()).expect("open");
        let started = Instant::now();
        run_until_stopped(stuck(), stop, receiver, Duration::from_secs(60))
            .await
            .expect("ok");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn the_grace_period_ends_the_wait() {
        let (stop, _stopped) = oneshot::channel();
        let (signals, receiver) = mpsc::unbounded_channel();
        signals.send(()).expect("open");
        let started = Instant::now();
        run_until_stopped(stuck(), stop, receiver, Duration::from_millis(50))
            .await
            .expect("ok");
        assert!(started.elapsed() >= Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn the_first_signal_asks_the_server_to_stop() {
        let (stop, stopped) = oneshot::channel::<()>();
        let (signals, receiver) = mpsc::unbounded_channel();
        let server = async move {
            let _ = stopped.await;
            Ok(())
        };
        signals.send(()).expect("open");
        // The server ends by itself after the request, well before the grace period.
        let started = Instant::now();
        run_until_stopped(server, stop, receiver, Duration::from_secs(60))
            .await
            .expect("ok");
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
