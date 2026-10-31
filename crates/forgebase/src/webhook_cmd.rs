//! The `webhook` commands (M9.2, ADR 0020): add, list, disable, enable, remove, rotate-secret and
//! test. Operators use the command line only; there is no desk screen for webhooks.
//!
//! Secrets: `add` and `rotate-secret` print the secret, once. Nothing else prints it: not `list`,
//! not an error, not the log of the worker.

use std::fmt::Write as _;

use forgebase_core::meta::Registry;
use forgebase_db::webhooks::{self, NewWebhook, WebhookInfo};
use forgebase_db::{DbError, Uuid};
use serde_json::json;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::args::WebhookCommand;
use crate::config::SiteContext;
use crate::error::CliError;
use crate::run::{read_password, write_stdout};
use crate::webhook_client::{Failure, Outgoing, parse_url, send};

/// The secret of `add` or `rotate-secret --secret-stdin`, read from standard input; `None` for
/// the other commands and without the flag. Called before the site's pool is opened, so a bad
/// input never touches the database.
pub(crate) fn read_secret(command: &WebhookCommand) -> Result<Option<String>, CliError> {
    match command {
        WebhookCommand::Add {
            secret_stdin: true, ..
        }
        | WebhookCommand::RotateSecret {
            secret_stdin: true, ..
        } => Ok(Some(read_password()?)),
        _ => Ok(None),
    }
}

/// Checks everything of `add` that needs no database: the URL, and (by `webhooks::add`, later)
/// the rest. The error never contains the URL.
fn check_add_url(url: &str, insecure_http: bool) -> Result<(), CliError> {
    parse_url(url, insecure_http)
        .map(drop)
        .map_err(|e| CliError::Webhook(format!("invalid URL: {e}")))
}

fn db(error: DbError) -> CliError {
    match error {
        DbError::Webhook(message) => CliError::Webhook(message),
        other => other.into(),
    }
}

/// Runs a `webhook` command; `secret` is what [`read_secret`] returned. `registry` is the
/// site's, with the `--doctypes` directory for `add`.
pub(crate) async fn webhook(
    site: &SiteContext,
    command: WebhookCommand,
    secret: Option<String>,
) -> Result<(), CliError> {
    let registry: &Registry = &site.registry;
    if let WebhookCommand::Add {
        url, insecure_http, ..
    } = &command
    {
        check_add_url(url, *insecure_http)?;
    }
    let client = site.pool.get().await.map_err(DbError::from)?;
    let client = &**client;
    match command {
        WebhookCommand::Add {
            name,
            doctype,
            events,
            url,
            allow_private,
            insecure_http,
            ..
        } => {
            let new = NewWebhook {
                name: &name,
                doctype: &doctype,
                events: &events,
                url: &url,
                allow_private,
                insecure_http,
            };
            let given = secret.is_some();
            let secret = webhooks::add(client, registry, &new, secret.as_deref())
                .await
                .map_err(db)?;
            let mut text = format!("added webhook {name}\n");
            if insecure_http {
                text.push_str(
                    "warning: --insecure-http: the payload and its signature travel unencrypted; development only\n",
                );
            }
            if allow_private {
                text.push_str(
                    "warning: --allow-private: the URL may reach loopback, private and link-local addresses\n",
                );
            }
            if given {
                // The operator has it already: it is not printed back.
                text.push_str("the secret is the one read from standard input\n");
            } else {
                // Printed once; it is not shown by any other command.
                let _ = writeln!(text, "secret: {secret}");
                text.push_str("store the secret now: it cannot be shown again\n");
            }
            write_stdout(&text)
        }
        WebhookCommand::List => write_stdout(&table(&webhooks::list(client).await.map_err(db)?)),
        WebhookCommand::Disable { name } => {
            webhooks::set_enabled(client, &name, false)
                .await
                .map_err(db)?;
            write_stdout(&format!("disabled webhook {name}\n"))
        }
        WebhookCommand::Enable { name } => {
            webhooks::set_enabled(client, &name, true)
                .await
                .map_err(db)?;
            write_stdout(&format!("enabled webhook {name}\n"))
        }
        WebhookCommand::Remove { name } => {
            webhooks::remove(client, &name).await.map_err(db)?;
            write_stdout(&format!("removed webhook {name}\n"))
        }
        WebhookCommand::RotateSecret { name, .. } => {
            let given = secret.is_some();
            let secret = webhooks::rotate_secret(client, &name, secret.as_deref())
                .await
                .map_err(db)?;
            if given {
                write_stdout(&format!(
                    "changed the secret of webhook {name} to the one read from standard input\n"
                ))
            } else {
                write_stdout(&format!(
                    "new secret of webhook {name}: {secret}\nstore the secret now: it cannot be shown again\n"
                ))
            }
        }
        WebhookCommand::Test { name } => test(client, &name).await,
    }
}

/// `webhook test`: one synchronous delivery with `event` `test`.
async fn test(client: &forgebase_db::Client, name: &str) -> Result<(), CliError> {
    let Some(hook) = webhooks::get(client, name).await.map_err(db)? else {
        return Err(CliError::Webhook(format!("no webhook named `{name}`")));
    };
    let label = parse_url(&hook.url, hook.insecure_http)
        .map(|t| t.label())
        .map_err(|e| CliError::Webhook(format!("invalid URL: {e}")))?;
    let delivery = format!("test-{}", Uuid::now_v7());
    let occurred_at = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    let body = serde_json::to_vec(&json!({
        "event": "test",
        "doctype": hook.doctype,
        "name": null,
        "data": null,
        "occurred_at": occurred_at,
        "delivery": delivery,
    }))
    .map_err(|e| CliError::Webhook(e.to_string()))?;
    let sent = send(Outgoing {
        url: &hook.url,
        secret: &hook.secret,
        allow_private: hook.allow_private,
        insecure_http: hook.insecure_http,
        event: "test",
        delivery: &delivery,
        body,
    })
    .await;
    match sent {
        Ok(status @ 200..=299) => write_stdout(&format!(
            "webhook {name}: {label} answered HTTP {status} (delivery {delivery})\n"
        )),
        Ok(status) => {
            write_stdout(&format!("webhook {name}: {label} answered HTTP {status}\n"))?;
            Err(CliError::Webhook(if (300..400).contains(&status) {
                format!(
                    "the test delivery was not accepted: HTTP {status} (redirects are not followed)"
                )
            } else {
                format!("the test delivery was not accepted: HTTP {status}")
            }))
        }
        Err(Failure::Refused(message) | Failure::Transport(message)) => Err(CliError::Webhook(
            format!("the test delivery failed: {message}"),
        )),
    }
}

/// The table of `list`: the URL column shows only scheme, host and port.
fn table(hooks: &[WebhookInfo]) -> String {
    let header = [
        "NAME",
        "DOCTYPE",
        "EVENTS",
        "URL",
        "ENABLED",
        "ALLOW_PRIVATE",
        "INSECURE_HTTP",
    ];
    let rows: Vec<[String; 7]> = hooks
        .iter()
        .map(|hook| {
            let url =
                parse_url(&hook.url, true).map_or_else(|_| "(invalid)".to_owned(), |t| t.label());
            let flag = |value: bool| if value { "yes" } else { "no" }.to_owned();
            [
                hook.name.clone(),
                hook.doctype.clone(),
                hook.events.join(","),
                url,
                flag(hook.enabled),
                flag(hook.allow_private),
                flag(hook.insecure_http),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_shows_the_host_only_and_no_secret() {
        let hook = WebhookInfo {
            name: "orders".into(),
            doctype: "sales_order".into(),
            events: vec!["insert".into(), "submit".into()],
            url: "https://hooks.example.com:8443/services/T0/B0/TOKEN?k=v".into(),
            enabled: true,
            allow_private: false,
            insecure_http: false,
            created_at: OffsetDateTime::UNIX_EPOCH,
        };
        let text = table(&[hook]);
        assert!(text.contains("https://hooks.example.com:8443"), "{text}");
        assert!(!text.contains("TOKEN"), "{text}");
        assert!(!text.contains("services"), "{text}");
        assert!(text.contains("insert,submit"), "{text}");
    }

    #[test]
    fn a_bad_url_error_does_not_repeat_the_url() {
        let error = check_add_url("https://user:pw@example.com/TOKEN", false).expect_err("bad");
        let text = error.to_string();
        assert!(!text.contains("TOKEN") && !text.contains("pw@"), "{text}");
    }
}
