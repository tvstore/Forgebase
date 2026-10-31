//! Outgoing webhooks, the storage and the outbox side (M9.2, ADR 0020).
//!
//! A webhook is a row of `fb_webhook` (migration 0010): the DocType and the events it listens to,
//! the URL, the secret that signs the deliveries and the two development flags. This module
//! stores them and decides **what is enqueued**; it never sends anything. The delivery is the
//! built-in job `forgebase.deliver_webhook` of the `forgebase` crate.
//!
//! # The outbox
//!
//! Every document operation of this crate ([`crate::insert`], [`crate::update`], [`crate::delete`],
//! [`crate::submit`], [`crate::cancel`], [`crate::amend`] and [`crate::transition`]) calls
//! [`enqueue`] inside its own transaction, after the hooks and before the commit. So the call is
//! in the one place that every writer passes through (the HTTP API, the CLI, the `Documents` of
//! jobs and methods, hooks that write), and a rolled-back operation enqueues nothing.
//!
//! | Operation | Events |
//! |---|---|
//! | insert | `insert` |
//! | update | `update` |
//! | delete | `delete` (the snapshot is the document as it was) |
//! | submit | `submit` |
//! | cancel | `cancel` |
//! | amend | `insert` (of the new draft) |
//! | transition | `transition`, and `submit` or `cancel` when the transition submits or cancels |
//!
//! For each enabled webhook of the DocType that listens to an event, one job is enqueued, as the
//! system actor of the operation's caller. The cost is one indexed `SELECT` per write (the index
//! is partial, on enabled webhooks); a site without webhooks pays that and nothing else.
//!
//! # The snapshot
//!
//! The payload carries the document as stored after the operation, in the JSON form of the
//! document API ([`crate::json::document_to_json`]): **all fields, child rows included, not
//! filtered by the permissions of the caller**. A webhook is configured by an operator, who
//! decides where the data goes. A snapshot that does not fit a job payload (64 KiB) is replaced
//! by `"data": null` and `"truncated": true`: a webhook never makes a business write fail
//! because of its size.

use forgebase_core::hooks::check_job_payload;
use forgebase_core::ident::validate_doctype_name;
use forgebase_core::meta::Registry;
use serde_json::json;
use time::OffsetDateTime;
use tokio_postgres::GenericClient;

use crate::document::{StoredDocument, document_doctype};
use crate::error::DbError;
use crate::json::{document_to_json, utc_rfc3339};

/// The full name of the delivery job.
pub const DELIVER_JOB: &str = "forgebase.deliver_webhook";

/// The events a webhook can listen to.
pub const EVENTS: [&str; 6] = [
    "insert",
    "update",
    "delete",
    "submit",
    "cancel",
    "transition",
];

/// The length of a generated secret in bytes (it is written as 64 hex characters), the same as
/// an API token secret.
const SECRET_BYTES: usize = 32;

/// A webhook as the delivery needs it, with its secret. It has no `Debug` output of the secret.
#[derive(Clone, PartialEq, Eq)]
pub struct Webhook {
    /// The name, the primary key.
    pub name: String,
    /// The DocType it listens to.
    pub doctype: String,
    /// The events, in the order they were given.
    pub events: Vec<String>,
    /// The URL deliveries are posted to.
    pub url: String,
    /// The secret that signs the deliveries (plain text: the signature needs it).
    pub secret: String,
    /// Whether it fires. A disabled webhook enqueues nothing and a queued delivery of it is
    /// dropped.
    pub enabled: bool,
    /// Whether the URL may resolve to a loopback, private or link-local address.
    pub allow_private: bool,
    /// Whether the URL may use `http://` (development only).
    pub insecure_http: bool,
}

impl std::fmt::Debug for Webhook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Webhook")
            .field("name", &self.name)
            .field("doctype", &self.doctype)
            .field("events", &self.events)
            .field("enabled", &self.enabled)
            .field("allow_private", &self.allow_private)
            .field("insecure_http", &self.insecure_http)
            .finish_non_exhaustive()
    }
}

/// A webhook as `list` shows it: everything but the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookInfo {
    /// The name.
    pub name: String,
    /// The DocType.
    pub doctype: String,
    /// The events.
    pub events: Vec<String>,
    /// The URL (callers print only its host).
    pub url: String,
    /// Whether it fires.
    pub enabled: bool,
    /// See [`Webhook::allow_private`].
    pub allow_private: bool,
    /// See [`Webhook::insecure_http`].
    pub insecure_http: bool,
    /// When it was added.
    pub created_at: OffsetDateTime,
}

/// What [`add`] needs to know about a new webhook. The URL is checked by the caller (the
/// `forgebase` crate), which parses URLs; this crate stores it.
#[derive(Debug, Clone, Copy)]
pub struct NewWebhook<'a> {
    /// The name: the rules of a DocType name.
    pub name: &'a str,
    /// A DocType of the registry that is not a child DocType.
    pub doctype: &'a str,
    /// At least one of [`EVENTS`].
    pub events: &'a [String],
    /// The URL.
    pub url: &'a str,
    /// See [`Webhook::allow_private`].
    pub allow_private: bool,
    /// See [`Webhook::insecure_http`].
    pub insecure_http: bool,
}

fn fail(message: impl Into<String>) -> DbError {
    DbError::Webhook(message.into())
}

/// Checks and normalises a list of events: known names only, at least one, no duplicate (the
/// first occurrence wins).
pub fn check_events(events: &[String]) -> Result<Vec<String>, DbError> {
    let mut checked: Vec<String> = Vec::new();
    for event in events {
        if !EVENTS.contains(&event.as_str()) {
            return Err(fail(format!(
                "unknown event `{event}` (use {})",
                EVENTS.join(", ")
            )));
        }
        if !checked.contains(event) {
            checked.push(event.clone());
        }
    }
    if checked.is_empty() {
        return Err(fail(format!(
            "at least one event is needed (use {})",
            EVENTS.join(", ")
        )));
    }
    Ok(checked)
}

/// Adds a webhook and returns its secret: `secret` if given, else 32 random bytes as hex. The
/// DocType must exist in `registry` and must not be a child DocType. The name must pass the
/// rules of a DocType name and be unused ([`DbError::Webhook`]). The secret is returned here and
/// can be read from the database only (the commands never print it again).
pub async fn add(
    client: &impl GenericClient,
    registry: &Registry,
    webhook: &NewWebhook<'_>,
    secret: Option<&str>,
) -> Result<String, DbError> {
    validate_doctype_name(webhook.name)
        .map_err(|e| fail(format!("the name {:?} {e}", webhook.name)))?;
    document_doctype(registry, webhook.doctype)?;
    let events = check_events(webhook.events)?;
    if webhook.url.is_empty() {
        return Err(fail("the URL is empty"));
    }
    let secret = match secret {
        Some("") => return Err(fail("the secret is empty")),
        Some(secret) => secret.to_owned(),
        None => crate::auth::random_hex(SECRET_BYTES)
            .map_err(|_| fail("cannot get random bytes from the operating system"))?,
    };
    let inserted = client
        .execute(
            "INSERT INTO fb_webhook (name, doctype, events, url, secret, allow_private, insecure_http) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (name) DO NOTHING",
            &[
                &webhook.name,
                &webhook.doctype,
                &events,
                &webhook.url,
                &secret,
                &webhook.allow_private,
                &webhook.insecure_http,
            ],
        )
        .await?;
    if inserted == 0 {
        return Err(fail(format!(
            "a webhook named `{}` exists already",
            webhook.name
        )));
    }
    Ok(secret)
}

/// Every webhook, in name order, without secrets.
pub async fn list(client: &impl GenericClient) -> Result<Vec<WebhookInfo>, DbError> {
    let rows = client
        .query(
            "SELECT name, doctype, events, url, enabled, allow_private, insecure_http, created_at \
             FROM fb_webhook ORDER BY name COLLATE \"C\"",
            &[],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(WebhookInfo {
                name: row.try_get(0)?,
                doctype: row.try_get(1)?,
                events: row.try_get(2)?,
                url: row.try_get(3)?,
                enabled: row.try_get(4)?,
                allow_private: row.try_get(5)?,
                insecure_http: row.try_get(6)?,
                created_at: row.try_get(7)?,
            })
        })
        .collect()
}

/// The webhook with this name, secret included, or `None`.
pub async fn get(client: &impl GenericClient, name: &str) -> Result<Option<Webhook>, DbError> {
    let row = client
        .query_opt(
            "SELECT name, doctype, events, url, secret, enabled, allow_private, insecure_http \
             FROM fb_webhook WHERE name = $1",
            &[&name],
        )
        .await?;
    row.map(|row| {
        Ok(Webhook {
            name: row.try_get(0)?,
            doctype: row.try_get(1)?,
            events: row.try_get(2)?,
            url: row.try_get(3)?,
            secret: row.try_get(4)?,
            enabled: row.try_get(5)?,
            allow_private: row.try_get(6)?,
            insecure_http: row.try_get(7)?,
        })
    })
    .transpose()
}

/// Enables or disables a webhook. [`DbError::Webhook`] when there is none with this name.
pub async fn set_enabled(
    client: &impl GenericClient,
    name: &str,
    enabled: bool,
) -> Result<(), DbError> {
    let changed = client
        .execute(
            "UPDATE fb_webhook SET enabled = $2, modified_at = now() WHERE name = $1",
            &[&name, &enabled],
        )
        .await?;
    found(changed, name)
}

/// Removes a webhook. Deliveries already queued for it are dropped when they run.
/// [`DbError::Webhook`] when there is none with this name.
pub async fn remove(client: &impl GenericClient, name: &str) -> Result<(), DbError> {
    let changed = client
        .execute("DELETE FROM fb_webhook WHERE name = $1", &[&name])
        .await?;
    found(changed, name)
}

/// Gives the webhook a new secret (`secret` if given, else random) and returns it.
pub async fn rotate_secret(
    client: &impl GenericClient,
    name: &str,
    secret: Option<&str>,
) -> Result<String, DbError> {
    let secret = match secret {
        Some("") => return Err(fail("the secret is empty")),
        Some(secret) => secret.to_owned(),
        None => crate::auth::random_hex(SECRET_BYTES)
            .map_err(|_| fail("cannot get random bytes from the operating system"))?,
    };
    let changed = client
        .execute(
            "UPDATE fb_webhook SET secret = $2, modified_at = now() WHERE name = $1",
            &[&name, &secret],
        )
        .await?;
    found(changed, name)?;
    Ok(secret)
}

fn found(changed: u64, name: &str) -> Result<(), DbError> {
    if changed == 0 {
        Err(fail(format!("no webhook named `{name}`")))
    } else {
        Ok(())
    }
}

/// Enqueues the deliveries a document operation causes: one job per enabled webhook of the
/// DocType and event. Runs in the operation's transaction (see the module documentation).
///
/// `events` are the events of the operation in order, `doc` is the document after it (before it,
/// for a delete). The query is one indexed `SELECT`; with no match nothing else is done, not even
/// the serialisation of the document.
pub(crate) async fn enqueue(
    client: &impl GenericClient,
    registry: &Registry,
    events: &[&str],
    doc: &StoredDocument,
    doctype: &str,
    enqueued_by: &str,
) -> Result<(), DbError> {
    let wanted: Vec<&str> = events.to_vec();
    let rows = client
        .query(
            "SELECT name, events, now() FROM fb_webhook \
             WHERE enabled AND doctype = $1 AND events && $2::text[] ORDER BY name COLLATE \"C\"",
            &[&doctype, &wanted],
        )
        .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let snapshot =
        document_to_json(doc).map_err(|e| DbError::Job(format!("webhook snapshot: {e}")))?;
    for row in &rows {
        let name: String = row.try_get(0)?;
        let listens: Vec<String> = row.try_get(1)?;
        let now: OffsetDateTime = row.try_get(2)?;
        let occurred_at =
            utc_rfc3339(&now).map_err(|e| DbError::Job(format!("webhook timestamp: {e}")))?;
        for event in events.iter().filter(|e| listens.iter().any(|l| l == **e)) {
            let mut payload = json!({
                "webhook": name,
                "event": event,
                "doctype": doctype,
                "name": doc.name,
                "data": snapshot,
                "occurred_at": occurred_at,
            });
            if check_job_payload(&payload).is_err() {
                payload["data"] = serde_json::Value::Null;
                payload["truncated"] = serde_json::Value::Bool(true);
            }
            crate::jobs::enqueue(client, registry, DELIVER_JOB, &payload, enqueued_by).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_checked_and_deduplicated() {
        let given =
            |events: &[&str]| -> Vec<String> { events.iter().map(|e| (*e).to_owned()).collect() };
        assert_eq!(
            check_events(&given(&["submit", "insert", "submit"])).expect("valid"),
            ["submit", "insert"]
        );
        assert!(check_events(&[]).is_err());
        assert!(check_events(&given(&["insert", "save"])).is_err());
        assert!(check_events(&given(&["Insert"])).is_err());
    }

    #[test]
    fn the_secret_is_not_in_debug_output() {
        let hook = Webhook {
            name: "n".into(),
            doctype: "d".into(),
            events: vec!["insert".into()],
            url: "https://example.com/x".into(),
            secret: "very-secret".into(),
            enabled: true,
            allow_private: false,
            insecure_http: false,
        };
        assert!(!format!("{hook:?}").contains("very-secret"));
    }
}
