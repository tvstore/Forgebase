//! Executes the commands.

use std::fmt::Write as _;
use std::io::{BufRead, Read, Write};
use std::time::Duration;

use forgebase_core::document::{Changes, Document};
use forgebase_core::meta::Registry;
use forgebase_core::permission::Caller;
use forgebase_db::auth;
use forgebase_db::{DbError, ListQuery, StoredDocument, SyncOptions};

use std::sync::Arc;

use forgebase_http::{AppConfig, AppState, Sites};

use crate::app::App;
use crate::args::{
    ActorArg, Cli, Command, CustomizeCommand, DataCommand, DocCommand, DocTypesDir, FieldCommand,
    SiteCommand, TokenCommand, UserCommand, VersionArg, WebhookCommand,
};
use crate::backup::{site_backup, site_restore};
use crate::compose::{Composed, compose};
use crate::config::{Config, Site, SiteContext, host_problem};
use crate::customize_cmd::{field as customize_field, read_field};
use crate::error::CliError;
use crate::job_cmd::{job, read_payload};
use crate::new_app::new_app;
use crate::report::migrate_report;
use crate::webhook_cmd::{read_secret, webhook};
use crate::worker::{Worker, WorkerConfig, run_worker, worker_id};
use forgebase_http::json::{
    Input, document_to_json, documents_to_json, parse_filters, parse_input, parse_sort,
};

/// The system actor of commands that have no `--actor` (`doc get`, `doc list`): they only read.
const DEFAULT_ACTOR: &str = "cli";

/// The environment variable with the path of the config file (the option `--config` wins).
const CONFIG_VAR: &str = "FORGEBASE_CONFIG";

/// Runs one command with the apps compiled into the binary. Every command first checks that
/// the apps (and the `--doctypes` directory) fit together; a startup error ends it with exit
/// code 1.
///
/// A database command works on exactly one site (`serve` on one or all), chosen from the config
/// file and `--site` before anything else is done. The site gives the command its pool; there is
/// no other way to reach a database.
pub async fn run(cli: Cli, apps: &[App]) -> Result<(), CliError> {
    let Cli {
        config,
        site,
        command,
    } = cli;
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
    let config = load_config(config, apps)?;
    let site = site.as_deref();
    match command {
        Command::Serve { doctypes } => return serve(&config, site, &doctypes, apps).await,
        Command::Site(command) => return site_command(&config, site, command, apps).await,
        Command::NewApp { .. } => unreachable!("handled above"),
        _ => {}
    }
    let site = config.select(site)?;
    match command {
        Command::Migrate {
            doctypes,
            dry_run,
            lock_timeout_ms,
        } => {
            let composed = site_composed(site, apps, &doctypes)?;
            let options = SyncOptions {
                dry_run,
                lock_timeout: Duration::from_millis(lock_timeout_ms),
            };
            write_stdout(&migrate_site(site, &composed, &options).await?)
        }
        Command::Doc(command) => {
            // The DocType is looked up in the apps (before the database is opened, so an unknown
            // one never touches it) and the input is read; the input is then checked against the
            // registry of the site context, which has the site's enabled custom fields.
            let (dir, name) = target(&command);
            let base = Arc::new(find_doctype(site, apps, dir, name)?);
            let text = read_doc_input(&command)?;
            // Syntax errors fail before the database is opened; field names and values are
            // checked later against the registry of the site context.
            check_doc_syntax(&command, text.as_deref())?;
            let ctx = site.context(base, None).await?;
            let operation = prepare(command, &ctx.registry, text)?;
            execute(&ctx, operation).await
        }
        Command::Data(command) => {
            let (DataCommand::Export {
                doctype, doctypes, ..
            }
            | DataCommand::Import {
                doctype, doctypes, ..
            }) = &command;
            let base = Arc::new(find_doctype(site, apps, doctypes, doctype)?);
            // The file is read before the database is opened, and checked against the registry
            // of the site context (enabled custom fields included) after.
            let bytes = crate::data_cmd::read_input(&command)?;
            let ctx = site.context(base, None).await?;
            let prepared = crate::data_cmd::prepare(command, &ctx.registry, bytes)?;
            crate::data_cmd::execute(&ctx, prepared).await
        }
        Command::User(command) => {
            let registry = site_registry(site, apps)?;
            // Standard input is read before the database is opened.
            let password = read_user_password(&command)?;
            let ctx = site.context(registry, None).await?;
            user(&ctx, command, &password).await
        }
        Command::Token(command) => {
            let ctx = site.context(site_registry(site, apps)?, None).await?;
            token(&ctx, command).await
        }
        Command::Webhook(command) => {
            // `add` checks the DocType against the `--doctypes` directory too.
            let registry = match &command {
                WebhookCommand::Add { doctypes, .. } => {
                    Arc::new(site_composed(site, apps, doctypes)?.registry)
                }
                _ => site_registry(site, apps)?,
            };
            // Standard input is read before the database is opened.
            let secret = read_secret(&command)?;
            let ctx = site.context(registry, None).await?;
            webhook(&ctx, command, secret).await
        }
        Command::Customize(CustomizeCommand::Field(command)) => {
            // The field is read and checked before the database is opened. `add` checks it
            // against the DocTypes of the site, the `--doctypes` directory included.
            let base = match &command {
                FieldCommand::Add { doctypes, .. } => {
                    Arc::new(site_composed(site, apps, doctypes)?.registry)
                }
                _ => site_registry(site, apps)?,
            };
            let field = read_field(&command)?;
            let ctx = site.context(Arc::clone(&base), None).await?;
            customize_field(&ctx, &base, command, field).await
        }
        Command::Worker {
            doctypes,
            concurrency,
        } => worker(site, &doctypes, apps, concurrency).await,
        Command::Job(command) => {
            let registry = site_registry(site, apps)?;
            // The payload (standard input too) is read and checked before the database is opened.
            let payload = read_payload(&command)?;
            let ctx = site.context(registry, None).await?;
            job(&ctx, command, payload).await
        }
        Command::Serve { .. } | Command::Site(_) | Command::NewApp { .. } => {
            unreachable!("handled above")
        }
    }
}

/// Migrates `site` with its composed apps and returns the report. The apps recorded in the
/// database that are not installed on the site get one warning line each on standard error: their
/// tables and data are kept (there is no uninstall).
async fn migrate_site(
    site: &Site,
    composed: &Composed,
    options: &SyncOptions,
) -> Result<String, CliError> {
    let ctx = site
        .context(Arc::new(composed.registry.clone()), None)
        .await?;
    let mut report = composed.migrate(&ctx.pool, options).await?;
    let client = ctx.pool.get().await.map_err(DbError::from)?;
    for name in forgebase_db::recorded_apps(&client).await? {
        if !composed.has_app(&name) {
            let _ = writeln!(
                std::io::stderr(),
                "warning: site `{}`: app `{name}` is recorded in the database but not installed; \
                 its tables and data are kept",
                ctx.site.name
            );
        }
    }
    // The documents of a workflow that are in a state it no longer defines: kept as they are.
    for warning in &report.workflow_warnings {
        let _ = writeln!(
            std::io::stderr(),
            "warning: site `{}`: {warning}",
            ctx.site.name
        );
    }
    let mut text = migrate_report(&report, options.dry_run, composed.app_count());
    // What was applied is reported first (the migrations are committed), then the fixtures of an
    // app that were refused, which were rolled back.
    if let Some(error) = report.fixtures_failure.take() {
        write_stdout(&text)?;
        return Err(error.into());
    }
    // The reports read the tables the migrations create, so they are checked after them, and
    // never in a dry run (nothing was applied, so the tables may not be there).
    let reports = composed.report_count();
    if reports > 0 && !options.dry_run {
        let problems = composed.check_reports(&ctx.pool).await?;
        if !problems.is_empty() {
            // What was applied is reported first: the migrations are committed.
            write_stdout(&text)?;
            return Err(DbError::ReportCheck(problems).into());
        }
        let _ = writeln!(text, "reports: {reports} checked");
    }
    Ok(text)
}

/// Runs a `site` command. They take the site as an argument, so `--site` is refused.
async fn site_command(
    config: &Config,
    site: Option<&str>,
    command: SiteCommand,
    apps: &[App],
) -> Result<(), CliError> {
    if site.is_some() {
        return Err(CliError::Usage(
            "`site` commands do not use --site; `site create` takes the site name as an argument"
                .to_owned(),
        ));
    }
    match command {
        SiteCommand::List => write_stdout(&site_list(config)),
        SiteCommand::Create {
            name,
            maintenance_db,
            empty,
            doctypes,
        } => {
            let site = config.find(&name)?;
            // Everything that can be checked without a database is checked before the database
            // is created. `--empty` uses no apps, so there is nothing to check.
            let composed = if empty {
                None
            } else {
                Some(site_composed(site, apps, &doctypes)?)
            };
            if !site.create_database(Some(&maintenance_db)).await? {
                return Err(CliError::Config(format!(
                    "database for site `{name}` already exists; run `forgebase migrate --site {name}`"
                )));
            }
            write_stdout(&format!("created the database of site `{name}`\n"))?;
            let Some(composed) = composed else {
                return write_stdout(&format!(
                    "the database is empty; restore into it with `forgebase site restore {name} --from FILE`\n"
                ));
            };
            let report = migrate_site(site, &composed, &SyncOptions::default())
                .await
                .map_err(|error| {
                    CliError::Config(format!(
                        "the database of site `{name}` was created, but migrate failed: {error}\n\
                         the database is kept; fix the problem and run `forgebase migrate --site {name}`"
                    ))
                })?;
            write_stdout(&report)
        }
        SiteCommand::Backup { name, out } => {
            write_stdout(&site_backup(config.find(&name)?, &out).await?)
        }
        SiteCommand::Restore { name, from } => {
            write_stdout(&site_restore(config.find(&name)?, &from).await?)
        }
    }
}

/// The table `site list` prints: name, hosts, apps and the name of the URL variable, in the
/// layout of the other list outputs.
fn site_list(config: &Config) -> String {
    let header = ["SITE", "HOSTS", "APPS", "DATABASE_URL_ENV"];
    let rows: Vec<[String; 4]> = config
        .sites()
        .iter()
        .map(|site| {
            let hosts = if config.is_configured() {
                site.hosts.join(", ")
            } else {
                // The implicit site answers to FORGEBASE_ALLOWED_HOSTS (see `serve`).
                env_or("FORGEBASE_ALLOWED_HOSTS", "localhost,127.0.0.1")
                    .split(',')
                    .map(str::trim)
                    .filter(|h| !h.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let apps = match &site.apps {
                None => "all".to_owned(),
                Some(list) if list.is_empty() => "(none)".to_owned(),
                Some(list) => list.join(", "),
            };
            [
                site.name.clone(),
                hosts,
                apps,
                site.database_url_var().to_owned(),
            ]
        })
        .collect();
    let widths: Vec<usize> = (0..header.len())
        .map(|i| {
            rows.iter()
                .map(|row| row[i].chars().count())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut text = String::new();
    let mut line = |cells: &[&str]| {
        let mut out = String::new();
        for (i, cell) in cells.iter().enumerate() {
            let _ = write!(out, "{cell:<width$}  ", width = widths[i]);
        }
        text.push_str(out.trim_end());
        text.push('\n');
    };
    line(&header);
    for row in &rows {
        line(&row.each_ref().map(String::as_str));
    }
    text
}

/// The config file named by `--config` or `FORGEBASE_CONFIG` (the option wins), else the
/// implicit single site. The current directory is never searched.
fn load_config(option: Option<std::path::PathBuf>, apps: &[App]) -> Result<Config, CliError> {
    let path = option.or_else(|| {
        std::env::var_os(CONFIG_VAR)
            .filter(|v| !v.is_empty())
            .map(Into::into)
    });
    match path {
        Some(path) => Config::load(&path, apps),
        None => Ok(Config::implicit()),
    }
}

/// Checks that the apps fit together (every command does) and returns the registry of the apps
/// installed on `site`, for the commands that need no DocType directory.
fn site_registry(site: &Site, apps: &[App]) -> Result<Arc<Registry>, CliError> {
    let none = DocTypesDir { doctypes: None };
    Ok(Arc::new(site_composed(site, apps, &none)?.registry))
}

/// A document operation with its input already read and checked: the DocType, the files and the
/// syntax of the JSON are checked before the database is opened, and the field names and values
/// against the registry of the site context (custom fields included) right after. It runs against
/// that registry.
enum Operation {
    Insert {
        doctype: String,
        input: Input,
        actor: String,
    },
    Get {
        doctype: String,
        name: String,
    },
    List {
        doctype: String,
        query: ListQuery,
    },
    Update {
        doctype: String,
        name: String,
        expected_version: i64,
        input: Input,
        actor: String,
    },
    Delete {
        doctype: String,
        name: String,
        expected_version: i64,
        actor: String,
    },
    /// A workflow transition: the action of the transition from the document's state.
    Transition {
        doctype: String,
        name: String,
        action: String,
        expected_version: i64,
        actor: String,
    },
    /// Submit, cancel or amend: a document operation without data.
    Status {
        kind: StatusKind,
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

/// The DocType directory option and the DocType name of a `doc` command.
fn target(command: &DocCommand) -> (&DocTypesDir, &str) {
    match command {
        DocCommand::Insert {
            doctype, doctypes, ..
        }
        | DocCommand::Get {
            doctype, doctypes, ..
        }
        | DocCommand::List {
            doctype, doctypes, ..
        }
        | DocCommand::Update {
            doctype, doctypes, ..
        }
        | DocCommand::Submit {
            doctype, doctypes, ..
        }
        | DocCommand::Cancel {
            doctype, doctypes, ..
        }
        | DocCommand::Transition {
            doctype, doctypes, ..
        }
        | DocCommand::Amend {
            doctype, doctypes, ..
        }
        | DocCommand::Delete {
            doctype, doctypes, ..
        } => (doctypes, doctype),
    }
}

/// Checks a `doc` command against `registry`, the registry of the site context (enabled custom
/// fields included), with the `--data` text that `read_doc_input` read, and returns the operation.
fn prepare(
    command: DocCommand,
    registry: &Registry,
    text: Option<String>,
) -> Result<Operation, CliError> {
    let (_, doctype) = target(&command);
    let found = registry
        .get(doctype)
        .cloned()
        .ok_or_else(|| unknown_doctype(registry, doctype))?;
    let doctype = found.name.clone();
    let operation = match command {
        DocCommand::Insert { actor, .. } => Operation::Insert {
            doctype,
            input: parse_input(registry, &found, text.as_deref().unwrap_or_default())?,
            actor: actor.actor,
        },
        DocCommand::Get { name, .. } => Operation::Get { doctype, name },
        DocCommand::List {
            filter,
            sort,
            limit,
            offset,
            ..
        } => Operation::List {
            doctype,
            query: ListQuery {
                filters: match filter {
                    Some(text) => parse_filters(&found, &text)?,
                    None => Vec::new(),
                },
                sort: parse_sort(&sort)?,
                limit,
                offset,
            },
        },
        DocCommand::Update {
            name,
            version,
            actor,
            ..
        } => Operation::Update {
            doctype,
            name,
            expected_version: version.expected_version,
            input: parse_input(registry, &found, text.as_deref().unwrap_or_default())?,
            actor: actor.actor,
        },
        DocCommand::Submit {
            name,
            version,
            actor,
            ..
        } => status(StatusKind::Submit, doctype, name, version, actor),
        DocCommand::Cancel {
            name,
            version,
            actor,
            ..
        } => status(StatusKind::Cancel, doctype, name, version, actor),
        DocCommand::Transition {
            name,
            action,
            version,
            actor,
            ..
        } => Operation::Transition {
            doctype,
            name,
            action,
            expected_version: version.expected_version,
            actor: actor.actor,
        },
        DocCommand::Amend {
            name,
            version,
            actor,
            ..
        } => status(StatusKind::Amend, doctype, name, version, actor),
        DocCommand::Delete {
            name,
            version,
            actor,
            ..
        } => Operation::Delete {
            doctype,
            name,
            expected_version: version.expected_version,
            actor: actor.actor,
        },
    };
    Ok(operation)
}

/// The syntax-only checks of a `doc` command: `--data` is a JSON object and `--filter` is a list
/// of filters. Nothing here needs a registry.
fn check_doc_syntax(command: &DocCommand, text: Option<&str>) -> Result<(), CliError> {
    if let Some(text) = text {
        forgebase_http::json::check_input_syntax(text)?;
    }
    if let DocCommand::List {
        filter: Some(filter),
        ..
    } = command
    {
        forgebase_http::json::check_filters_syntax(filter)?;
    }
    Ok(())
}

/// The `--data` text of an `insert` or `update` (standard input for `-`), read before the
/// database is opened.
fn read_doc_input(command: &DocCommand) -> Result<Option<String>, CliError> {
    match command {
        DocCommand::Insert { data, .. } | DocCommand::Update { data, .. } => {
            read_data(&data.data).map(Some)
        }
        _ => Ok(None),
    }
}

fn status(
    kind: StatusKind,
    doctype: String,
    name: String,
    version: VersionArg,
    actor: ActorArg,
) -> Operation {
    Operation::Status {
        kind,
        doctype,
        name,
        expected_version: version.expected_version,
        actor: actor.actor,
    }
}

async fn execute(site: &SiteContext, operation: Operation) -> Result<(), CliError> {
    let registry = &*site.registry;
    let mut client = site.pool.get().await.map_err(DbError::from)?;
    let client = &mut **client;
    let stored = match operation {
        Operation::List { doctype, query } => {
            let caller = Caller::system(DEFAULT_ACTOR);
            let documents =
                forgebase_db::list(&*client, registry, &doctype, &caller, &query).await?;
            let json = documents_to_json(&documents)?;
            let text = serde_json::to_string_pretty(&json)
                .map_err(forgebase_http::json::JsonError::Serialize)?;
            return write_stdout(&format!("{text}\n"));
        }
        Operation::Insert {
            doctype,
            input,
            actor,
        } => {
            let document = Document {
                name: None,
                values: input.values,
                tables: input.tables,
            };
            forgebase_db::insert(client, registry, &doctype, document, &Caller::system(actor))
                .await?
        }
        Operation::Get { doctype, name } => forgebase_db::get(
            client,
            registry,
            &doctype,
            &name,
            &Caller::system(DEFAULT_ACTOR),
        )
        .await?
        .ok_or(DbError::NotFound { doctype, name })?,
        Operation::Update {
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
                registry,
                &doctype,
                &name,
                expected_version,
                &changes,
                &Caller::system(actor),
            )
            .await?
        }
        Operation::Transition {
            doctype,
            name,
            action,
            expected_version,
            actor,
        } => {
            forgebase_db::transition(
                client,
                registry,
                &doctype,
                &name,
                &action,
                expected_version,
                &Caller::system(actor),
            )
            .await?
        }
        Operation::Status {
            kind,
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
                    forgebase_db::submit(client, registry, doctype, name, expected_version, &caller)
                        .await?
                }
                StatusKind::Cancel => {
                    forgebase_db::cancel(client, registry, doctype, name, expected_version, &caller)
                        .await?
                }
                StatusKind::Amend => {
                    forgebase_db::amend(client, registry, doctype, name, expected_version, &caller)
                        .await?
                }
            }
        }
        Operation::Delete {
            doctype,
            name,
            expected_version,
            actor,
        } => {
            return Ok(forgebase_db::delete(
                client,
                registry,
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

/// The password of a `user` command that takes one, read from standard input (empty for the
/// others). Called before the site's pool is opened, so a bad input never touches the database.
fn read_user_password(command: &UserCommand) -> Result<String, CliError> {
    match command {
        UserCommand::Create { .. } | UserCommand::SetPassword { .. } => read_password(),
        _ => Ok(String::new()),
    }
}

/// Runs a `user` command with the `password` already read from standard input; it is never
/// printed.
async fn user(site: &SiteContext, command: UserCommand, password: &str) -> Result<(), CliError> {
    let mut client = site.pool.get().await.map_err(DbError::from)?;
    let client = &mut **client;
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

/// Runs `serve`: the HTTP API until Ctrl-C or SIGTERM. Prints the listening address (never a
/// database URL or any secret). It does not run migrations.
///
/// Without a config file it serves the implicit site `default` for the hosts of
/// `FORGEBASE_ALLOWED_HOSTS`. With one it serves every site (or only `--site`), one pool per
/// site, and routes each request by its `Host` to the site that owns the host.
///
/// The first signal stops accepting connections and lets running requests finish, for at most
/// [`SHUTDOWN_GRACE`]; a second signal, or the end of the grace period, exits at once.
async fn serve(
    config: &Config,
    site: Option<&str>,
    doctypes: &DocTypesDir,
    apps: &[App],
) -> Result<(), CliError> {
    let sites = config.serve_set(site)?;
    let listen = env_or("FORGEBASE_LISTEN", "127.0.0.1:8000");
    let insecure = std::env::var("FORGEBASE_DEV_INSECURE_COOKIES").is_ok_and(|v| v == "1");
    if insecure {
        let _ = writeln!(
            std::io::stderr(),
            "warning: FORGEBASE_DEV_INSECURE_COOKIES=1: the session cookie has no Secure attribute; \
             use this for local development over plain HTTP only"
        );
    }
    let desk_dir = desk_dir()?;
    let app_config = |allowed_hosts: Vec<String>| AppConfig {
        allowed_hosts,
        secure_cookies: !insecure,
        desk_dir: desk_dir.clone(),
        ..AppConfig::default()
    };
    let mut routing;
    let mut lines = Vec::new();
    if config.is_configured() {
        if std::env::var("FORGEBASE_ALLOWED_HOSTS").is_ok_and(|v| !v.trim().is_empty()) {
            let _ = writeln!(
                std::io::stderr(),
                "warning: FORGEBASE_ALLOWED_HOSTS is ignored: the hosts come from the config file"
            );
        }
        routing = Sites::configured();
        // Every site's registry is composed before any pool is opened, so a configuration or
        // composition error is reported without touching a database. Each site gets the
        // registry of its own apps.
        let mut registries = Vec::new();
        for site in sites {
            let composed = site_composed(site, apps, doctypes)?;
            registries.push((
                site,
                Arc::new(composed.registry),
                Arc::new(composed.methods),
            ));
        }
        for (site, registry, methods) in registries {
            let ctx = site.context(registry, None).await?;
            let state = AppState::with_holder(
                ctx.pool,
                ctx.holder,
                app_config(site.hosts.clone()),
                Some(methods),
            );
            routing = routing.site(&site.hosts, state).map_err(CliError::Config)?;
            lines.push(format!("site {}: {}\n", site.name, site.hosts.join(", ")));
        }
    } else {
        let hosts = env_or("FORGEBASE_ALLOWED_HOSTS", "localhost,127.0.0.1");
        let allowed_hosts = parse_allowed_hosts(&hosts)?;
        let [site] = sites.as_slice() else {
            unreachable!("the implicit config has one site");
        };
        let composed = site_composed(site, apps, doctypes)?;
        let methods = Arc::new(composed.methods);
        let ctx = site.context(Arc::new(composed.registry), None).await?;
        routing = Sites::implicit(AppState::with_holder(
            ctx.pool,
            ctx.holder,
            app_config(allowed_hosts),
            Some(methods),
        ));
    }
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| CliError::Config(format!("cannot listen on {listen}: {e}")))?;
    let address = listener.local_addr().map_err(CliError::Serve)?;
    let signals = stop_signals().map_err(CliError::Serve)?;
    // The server's own announcements: a closed standard output must not stop a running server.
    announce(&format!("listening on http://{address}\n"));
    for line in lines {
        announce(&line);
    }
    if let Some(dir) = &desk_dir {
        announce(&format!(
            "serving the desk from {} at /desk/\n",
            dir.display()
        ));
    }
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = forgebase_http::serve(listener, routing, async {
        let _ = stopped.await;
    });
    run_until_stopped(server, stop, signals, SHUTDOWN_GRACE)
        .await
        .map_err(CliError::Serve)
}

/// Runs `worker`: the jobs of the apps installed on the site until Ctrl-C or SIGTERM (see
/// `crate::worker`). One worker process serves one site, and the scheduler runs in it, so only
/// the schedules of the site's apps tick.
///
/// The pool has `concurrency + 2` connections: each running job holds one for its transaction,
/// and a claim or the record of a failure needs one briefly while the handlers hold theirs.
async fn worker(
    site: &Site,
    doctypes: &DocTypesDir,
    apps: &[App],
    concurrency: u32,
) -> Result<(), CliError> {
    let composed = site_composed(site, apps, doctypes)?;
    let size = usize::try_from(concurrency)
        .unwrap_or(usize::MAX)
        .saturating_add(2);
    // The pool of the worker is sized by its concurrency, not by the `pool_size` of the site.
    let ctx = site
        .context(Arc::new(composed.registry.clone()), Some(size))
        .await?;
    let signals = stop_signals().map_err(CliError::Serve)?;
    let worker = Worker::new(
        ctx.pool,
        ctx.holder,
        composed.jobs,
        composed.schedules,
        WorkerConfig::default(),
    );
    if run_worker(Arc::new(worker), concurrency, &worker_id(), signals).await {
        Ok(())
    } else {
        Err(CliError::Config(
            "a worker slot panicked (see the log above)".to_owned(),
        ))
    }
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
        if let Some(problem) = host_problem(host) {
            return Err(CliError::Config(format!(
                "FORGEBASE_ALLOWED_HOSTS entry {host:?} {problem}"
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
async fn token(site: &SiteContext, command: TokenCommand) -> Result<(), CliError> {
    let client = site.pool.get().await.map_err(DbError::from)?;
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
pub(crate) fn read_password() -> Result<String, CliError> {
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

/// The apps installed on `site` plus the `--doctypes` directory (the app `local`, which depends
/// on those apps), composed into the registry and the jobs of that site. It reuses
/// [`compose`]: first on all the apps of the binary, so a problem in the binary or the directory
/// is reported once, as for every command; then, when the site installs only some of the apps,
/// on those.
fn site_composed(site: &Site, apps: &[App], dir: &DocTypesDir) -> Result<Composed, CliError> {
    let local = dir.doctypes.as_deref();
    let all = compose(apps, local)?;
    if site.apps.is_none() {
        return Ok(all);
    }
    Ok(compose(&site.installed(apps), local).map_err(|e| e.for_site(&site.name))?)
}

/// The registry of the site's apps and the directory (without the site's custom fields), checked
/// to have a DocType called `name`.
fn find_doctype(
    site: &Site,
    apps: &[App],
    dir: &DocTypesDir,
    name: &str,
) -> Result<Registry, CliError> {
    let registry = site_composed(site, apps, dir)?.registry;
    if registry.get(name).is_some() {
        Ok(registry)
    } else {
        Err(unknown_doctype(&registry, name))
    }
}

fn unknown_doctype(registry: &Registry, name: &str) -> CliError {
    CliError::UnknownDocType {
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

/// Writes a line of a long-running command (`serve`) to standard output and ignores a failure,
/// so that a closed pipe never takes the server down.
fn announce(text: &str) {
    let _ = write_stdout(text);
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
