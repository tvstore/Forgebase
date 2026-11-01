//! App method endpoints through the public API (M9.1, ADR 0019), with `forgebase::testing`:
//! the gate (authentication, CSRF, roles, apps that a site does not install), what a method may
//! do with documents as the caller, one transaction per call, the body and the errors, the
//! startup rules, and the documents handle of a job (still the system context).
//!
//! Each test gets its own throwaway database and fails loudly (never skips) when
//! `FORGEBASE_TEST_DATABASE_URL` is not set.

use forgebase::serde_json::{Value as Json, json};
use forgebase::testing::{HttpClient, Reply, TestDb, run_jobs};
use forgebase::{
    App, Changes, DocTypeFile, Document, FilterOp, Forgebase, JobContext, JobError, JobFuture,
    JobHandler, JobOptions, ListQuery, Method, MethodContext, MethodError, MethodFuture,
    MethodHandler, Value,
};

const NOTE: &str = r#"
name = "note"
label = "Note"
naming = { field = "code" }
[[fields]]
name = "code"
type = "data"
required = true
unique = true
[[fields]]
name = "qty"
type = "int"

# A clerk sees and changes only the notes they own; an auditor only reads; the boss does it all.
[[permissions]]
role = "Clerk"
read = true
write = true
create = true
if_owner = true

[[permissions]]
role = "Auditor"
read = true

[[permissions]]
role = "Boss"
read = true
write = true
create = true
delete = true
"#;

const TICKET: &str = r#"
name = "ticket"
label = "Ticket"
naming = { field = "code" }
[[fields]]
name = "code"
type = "data"
required = true
unique = true

[[permissions]]
role = "Agent"
read = true
write = true
create = true

[[permissions]]
role = "Closer"
read = true
write = true

[workflow]

[[workflow.states]]
name = "Open"
docstatus = 0

[[workflow.states]]
name = "Closed"
docstatus = 0

[[workflow.transitions]]
action = "Close"
from = "Open"
to = "Closed"
roles = ["Closer"]
"#;

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn doc(code: &str) -> Document {
    Document {
        values: [("code".to_owned(), text(code))].into(),
        ..Document::default()
    }
}

/// What a test method does. One handler type with a behaviour keeps the tests short.
#[derive(Clone, Copy)]
enum Behavior {
    /// `{"ok": true}`.
    Ping,
    /// The username and roles of the caller.
    Whoami,
    /// The request body.
    Echo,
    /// The `code`s of the notes the caller can list, in order.
    ListNotes,
    /// `{"found": bool}`: whether the caller can get the note named by `code`.
    ReadNote,
    /// Insert the note `code`; return its name and version.
    AddNote,
    /// Insert the note `code`, then fail with `bad_request`.
    AddThenBad,
    /// Insert the note `code`, then fail with `unprocessable`.
    AddThenUnprocessable,
    /// Insert the note `code`, then panic.
    AddThenPanic,
    /// Insert a note, update it, then update it with a stale version (a conflict).
    AddThenConflict,
    /// A refused insert (no `code`) is handled, then a note is inserted.
    RefusedThenAdd,
    /// Update the note `code` at the body's `version`: `qty` becomes 7.
    UpdateNote,
    /// Insert a note, ignore a read that the server refuses, insert another note.
    SwallowedRead,
    SwallowedGet,
    /// Insert the ticket `code`.
    AddTicket,
    /// Take the workflow action "Close" on the ticket `name` at `version`.
    CloseTicket,
    /// Enqueue `shop.log` with the `code`; fail with `bad_request` when `fail` is true.
    Enqueue,
    /// Enqueue a job that does not exist.
    EnqueueUnknown,
}

struct Shop(Behavior);

impl MethodHandler for Shop {
    fn call<'a>(&'a self, ctx: &'a mut MethodContext<'_>) -> MethodFuture<'a> {
        Box::pin(async move {
            let code = ctx.body()["code"].as_str().unwrap_or("none").to_owned();
            match self.0 {
                Behavior::Ping => Ok(json!({"ok": true})),
                Behavior::Whoami => {
                    let caller = ctx.caller();
                    Ok(json!({"user": caller.username(), "roles": caller.roles()}))
                }
                Behavior::Echo => Ok(ctx.body().clone()),
                Behavior::ListNotes => {
                    let query = ListQuery::new().order_by("code", false);
                    let notes = ctx.documents().list("note", &query).await?;
                    let codes: Vec<&str> = notes.iter().map(|n| n.name()).collect();
                    Ok(json!(codes))
                }
                Behavior::ReadNote => {
                    let found = ctx.documents().get("note", &code).await?;
                    Ok(json!({"found": found.is_some()}))
                }
                Behavior::AddNote => {
                    let stored = ctx.documents().insert("note", doc(&code)).await?;
                    Ok(json!({"name": stored.name(), "version": stored.version()}))
                }
                Behavior::AddThenBad => {
                    ctx.documents().insert("note", doc(&code)).await?;
                    Err(MethodError::bad_request("Pick another code."))
                }
                Behavior::AddThenUnprocessable => {
                    ctx.documents().insert("note", doc(&code)).await?;
                    Err(MethodError::unprocessable("The code is taken."))
                }
                Behavior::AddThenPanic => {
                    ctx.documents().insert("note", doc(&code)).await?;
                    panic!("handler bug: secret detail");
                }
                Behavior::AddThenConflict => {
                    let mut docs = ctx.documents();
                    let stored = docs.insert("note", doc(&code)).await?;
                    let changes = || Changes {
                        values: [("qty".to_owned(), Value::Int(1))].into(),
                        ..Changes::default()
                    };
                    docs.update("note", stored.name(), 1, changes()).await?;
                    docs.update("note", stored.name(), 1, changes()).await?;
                    Ok(Json::Null)
                }
                Behavior::RefusedThenAdd => {
                    let mut docs = ctx.documents();
                    let refused = docs.insert("note", Document::default()).await;
                    assert!(refused.is_err());
                    docs.insert("note", doc(&code)).await?;
                    Ok(json!({"refused": true}))
                }
                Behavior::UpdateNote => {
                    let version = ctx.body()["version"].as_i64().unwrap_or_default();
                    let changes = Changes {
                        values: [("qty".to_owned(), Value::Int(7))].into(),
                        ..Changes::default()
                    };
                    let stored = ctx
                        .documents()
                        .update("note", &code, version, changes)
                        .await?;
                    Ok(json!({"version": stored.version()}))
                }
                Behavior::SwallowedRead => {
                    swallowed_read(&mut ctx.documents(), &code).await?;
                    Ok(json!({"swallowed": true}))
                }
                Behavior::SwallowedGet => {
                    swallowed_get(&mut ctx.documents(), &code).await?;
                    Ok(json!({"swallowed": true}))
                }
                Behavior::AddTicket => {
                    let stored = ctx.documents().insert("ticket", doc(&code)).await?;
                    Ok(json!({"name": stored.name(), "version": stored.version()}))
                }
                Behavior::CloseTicket => {
                    let name = ctx.body()["name"].as_str().unwrap_or_default().to_owned();
                    let version = ctx.body()["version"].as_i64().unwrap_or_default();
                    let stored = ctx
                        .documents()
                        .transition("ticket", &name, "Close", version)
                        .await?;
                    Ok(json!({"version": stored.version()}))
                }
                Behavior::Enqueue => {
                    let fail = ctx.body()["fail"].as_bool().unwrap_or(false);
                    ctx.enqueue("shop.log", json!({"code": code})).await?;
                    if fail {
                        return Err(MethodError::bad_request("No."));
                    }
                    Ok(json!({"queued": true}))
                }
                Behavior::EnqueueUnknown => {
                    ctx.enqueue("shop.nobody", json!({})).await?;
                    Ok(Json::Null)
                }
            }
        })
    }
}

/// Inserts the note `<code>-1`, makes a read that PostgreSQL itself refuses (a `like` pattern
/// that ends with the escape character) and **ignores the error**, then inserts `<code>-2`.
async fn swallowed_read(
    docs: &mut forgebase::Documents<'_>,
    code: &str,
) -> Result<(), forgebase::DocError> {
    docs.insert("note", doc(&format!("{code}-1"))).await?;
    // PostgreSQL raises the error when the match reaches the trailing backslash, which it does
    // for the row that was just inserted (a pattern that fails on its first characters never
    // gets there).
    let pattern = format!("{code}-%\\");
    let refused = ListQuery::new().filter("code", FilterOp::Like, text(&pattern));
    assert!(
        docs.list("note", &refused).await.is_err(),
        "the server refuses the pattern"
    );
    docs.insert("note", doc(&format!("{code}-2"))).await?;
    Ok(())
}

/// Like [`swallowed_read`] with `get` of a DocType without Table fields (a single statement, given
/// its own savepoint by the documents handle): a name with a NUL character, which PostgreSQL
/// refuses, is read and the error ignored; then `<code>-2` is inserted.
async fn swallowed_get(
    docs: &mut forgebase::Documents<'_>,
    code: &str,
) -> Result<(), forgebase::DocError> {
    docs.insert("note", doc(&format!("{code}-1"))).await?;
    assert!(
        docs.get("note", "bad\0name").await.is_err(),
        "the server refuses a name with a NUL character"
    );
    docs.insert("note", doc(&format!("{code}-2"))).await?;
    Ok(())
}

/// `shop.swallow`: the same through the documents handle of a job.
struct Swallow;

impl JobHandler for Swallow {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            swallowed_read(&mut ctx.documents(), "job").await?;
            Ok::<(), JobError>(())
        })
    }
}

/// `shop.log`: inserts the note of the payload through the documents handle of a job, which is
/// the system context: the users have no say. `shop.bad` inserts an invalid note.
struct Log(bool);

impl JobHandler for Log {
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
        Box::pin(async move {
            let code = ctx.payload()["code"].as_str().unwrap_or("none").to_owned();
            let document = if self.0 {
                Document::default()
            } else {
                doc(&code)
            };
            ctx.documents().insert("note", document).await?;
            Ok::<(), JobError>(())
        })
    }
}

const BASE_FILES: &[DocTypeFile] = &[
    DocTypeFile::new("doctypes/note.toml", NOTE),
    DocTypeFile::new("doctypes/ticket.toml", TICKET),
];

fn base() -> App {
    App::new("base").doctypes(BASE_FILES)
}

fn shop() -> App {
    let m = |b: Behavior| Method::new(Shop(b));
    let writers = ["Clerk", "Auditor", "Boss"];
    App::new("shop")
        .depends_on(&["base"])
        .method("ping", m(Behavior::Ping).any_user())
        .method("whoami", m(Behavior::Whoami).any_user())
        .method("echo", m(Behavior::Echo).any_user())
        .method("boss_only", m(Behavior::Ping).roles(&["Boss"]))
        .method("list_notes", m(Behavior::ListNotes).roles(&writers))
        .method("read_note", m(Behavior::ReadNote).roles(&writers))
        .method("add_note", m(Behavior::AddNote).roles(&writers))
        .method("add_then_bad", m(Behavior::AddThenBad).roles(&writers))
        .method(
            "add_then_unprocessable",
            m(Behavior::AddThenUnprocessable).roles(&writers),
        )
        .method("add_then_panic", m(Behavior::AddThenPanic).roles(&writers))
        .method(
            "add_then_conflict",
            m(Behavior::AddThenConflict).roles(&writers),
        )
        .method(
            "refused_then_add",
            m(Behavior::RefusedThenAdd).roles(&writers),
        )
        .method("update_note", m(Behavior::UpdateNote).roles(&writers))
        .method("swallowed_read", m(Behavior::SwallowedRead).roles(&writers))
        .method("swallowed_get", m(Behavior::SwallowedGet).roles(&writers))
        .method("add_ticket", m(Behavior::AddTicket).roles(&["Agent"]))
        .method(
            "close_ticket",
            m(Behavior::CloseTicket).roles(&["Agent", "Closer"]),
        )
        .method("enqueue", m(Behavior::Enqueue).roles(&["Clerk"]))
        .method(
            "enqueue_unknown",
            m(Behavior::EnqueueUnknown).roles(&["Clerk"]),
        )
        .job_with("log", Log(false), JobOptions::new())
        .job_with("bad", Log(true), JobOptions::new())
        .job_with("swallow", Swallow, JobOptions::new())
}

fn binary() -> Forgebase {
    Forgebase::new().app(base()).app(shop())
}

async fn ready() -> TestDb {
    let binary = binary();
    let db = TestDb::new().await;
    db.migrate(&binary, false).await.expect("migrate");
    for (name, roles) in [
        ("ann", &["Clerk"][..]),
        ("bob", &["Clerk"]),
        ("ada", &["Auditor"]),
        ("boss", &["Boss"]),
        ("root", &["Administrator"]),
        ("gus", &[]),
        ("amy", &["Agent"]),
        ("cleo", &["Closer"]),
    ] {
        db.create_user(name, roles).await;
    }
    db
}

async fn login(db: &TestDb, user: &str) -> HttpClient {
    db.client(&binary()).login(user).await
}

fn call(method: &str) -> String {
    format!("/api/v1/method/shop.{method}")
}

async fn codes(db: &TestDb) -> Vec<String> {
    db.rows("SELECT code FROM doc_note ORDER BY code")
        .await
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

async fn job_count(db: &TestDb) -> String {
    db.rows("SELECT count(*)::text FROM fb_job WHERE name = 'shop.log'")
        .await
        .remove(0)
        .remove(0)
}

// ---- the gate -----------------------------------------------------------------------------

#[tokio::test]
async fn the_gate_is_authentication_csrf_and_the_roles() {
    let db = ready().await;
    let anon = db.client(&binary());
    let boss = anon.login("boss").await;

    // Not authenticated: 401, whatever the method.
    assert_eq!(anon.post(&call("ping"), json!({})).await.status, 401);
    assert_eq!(anon.post(&call("nothing"), json!({})).await.status, 401);

    // A cookie call needs the CSRF header; a token call does not.
    let no_csrf = boss.without_csrf().post(&call("ping"), json!({})).await;
    assert_eq!(no_csrf.status, 403);
    assert_eq!(no_csrf.error_code(), Some("csrf_failed"));
    let token = db.create_token("boss").await;
    let by_token = anon.with_token(&token).post(&call("ping"), json!({})).await;
    assert_eq!(by_token.status, 200, "{:?}", by_token.body);
    assert_eq!(by_token.data(), &json!({"ok": true}));
    let bad = anon.with_token("nokey:nosecret");
    assert_eq!(bad.post(&call("ping"), json!({})).await.status, 401);

    // The right role, the Administrator, and any user for an `any_user` method.
    let reply = boss.post(&call("boss_only"), json!({})).await;
    assert_eq!((reply.status, reply.data()), (200, &json!({"ok": true})));
    let root = anon.login("root").await;
    assert_eq!(root.post(&call("boss_only"), json!({})).await.status, 200);
    let gus = anon.login("gus").await;
    let reply = gus.post(&call("whoami"), json!({})).await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    assert_eq!(reply.data(), &json!({"user": "gus", "roles": []}));

    // The wrong role, an unknown method and a method name that is not `<app>.<method>` are one
    // and the same 404.
    let ann = anon.login("ann").await;
    let refused = ann.post(&call("boss_only"), json!({})).await;
    let unknown = ann.post(&call("nothing"), json!({})).await;
    let other_app = ann.post("/api/v1/method/nobody.ping", json!({})).await;
    let bare = ann.post("/api/v1/method/ping", json!({})).await;
    assert_eq!(refused.status, 404);
    assert_eq!(refused.error_code(), Some("not_found"));
    for reply in [&unknown, &other_app, &bare] {
        assert_eq!((reply.status, &reply.body), (404, &refused.body));
    }

    // Another HTTP method on the route is 405.
    assert_eq!(boss.get(&call("ping")).await.status, 405);
    assert_eq!(boss.patch(&call("ping"), json!({})).await.status, 405);
    assert_eq!(boss.delete(&call("ping")).await.status, 405);
    db.cleanup().await;
}

#[tokio::test]
async fn the_methods_of_an_app_that_is_not_installed_do_not_exist() {
    // A site that installs only `base` composes only base: the registry has no method of `shop`.
    let only_base = Forgebase::new().app(base());
    let db = ready().await;
    for who in ["boss", "root", "gus"] {
        let client = db.client(&only_base).login(who).await;
        let reply = client.post(&call("ping"), json!({})).await;
        assert_eq!(reply.status, 404, "{who}");
        assert_eq!(reply.error_code(), Some("not_found"));
    }
    let client = login(&db, "gus").await;
    assert_eq!(client.post(&call("ping"), json!({})).await.status, 200);
    db.cleanup().await;
}

// ---- permissions inside the handler -------------------------------------------------------

#[tokio::test]
async fn a_method_reads_and_lists_only_what_the_caller_may() {
    let db = ready().await;
    let (ann, bob, ada) = (
        login(&db, "ann").await,
        login(&db, "bob").await,
        login(&db, "ada").await,
    );
    let boss = login(&db, "boss").await;
    for (client, code) in [(&ann, "a1"), (&ann, "a2"), (&bob, "b1")] {
        let reply = client.post(&call("add_note"), json!({"code": code})).await;
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        assert_eq!(reply.data(), &json!({"name": code, "version": 1}));
    }
    // `if_owner`: a clerk lists and gets only their own notes.
    let listed = |client: &HttpClient| {
        let client = client.clone();
        async move { client.post(&call("list_notes"), json!({})).await }
    };
    assert_eq!(listed(&ann).await.data(), &json!(["a1", "a2"]));
    assert_eq!(listed(&bob).await.data(), &json!(["b1"]));
    // The auditor and the boss read everything.
    assert_eq!(listed(&ada).await.data(), &json!(["a1", "a2", "b1"]));
    assert_eq!(listed(&boss).await.data(), &json!(["a1", "a2", "b1"]));
    let read = |client: &HttpClient, code: &'static str| {
        let client = client.clone();
        async move {
            client
                .post(&call("read_note"), json!({"code": code}))
                .await
                .data()["found"]
                .as_bool()
        }
    };
    assert_eq!(read(&ann, "a1").await, Some(true));
    assert_eq!(read(&bob, "a1").await, Some(false));
    assert_eq!(read(&ada, "a1").await, Some(true));
    // The owner is the caller.
    assert_eq!(
        db.rows("SELECT owner FROM doc_note WHERE code = 'b1'")
            .await,
        vec![vec!["bob".to_owned()]]
    );
    db.cleanup().await;
}

#[tokio::test]
async fn a_write_without_the_right_is_permission_denied() {
    let db = ready().await;
    let ada = login(&db, "ada").await;
    let reply = ada.post(&call("add_note"), json!({"code": "x"})).await;
    assert_eq!(reply.status, 403, "{:?}", reply.body);
    assert_eq!(reply.error_code(), Some("permission_denied"));
    assert!(codes(&db).await.is_empty());
    db.cleanup().await;
}

/// M9.1 review, carry-over: an update that the caller may not make is refused with the document
/// API's status, and the row is untouched.
#[tokio::test]
async fn an_update_without_the_right_or_of_a_note_that_is_not_hers_is_refused() {
    let db = ready().await;
    let (ann, bob, ada, boss) = (
        login(&db, "ann").await,
        login(&db, "bob").await,
        login(&db, "ada").await,
        login(&db, "boss").await,
    );
    assert_eq!(
        bob.post(&call("add_note"), json!({"code": "b1"}))
            .await
            .status,
        200
    );
    let row =
        || db.rows("SELECT version::text, coalesce(qty::text, '-'), modified_by FROM doc_note");
    let before = row().await;
    assert_eq!(before, [["1", "-", "bob"]]);

    // The auditor reads but has no write right: 403 permission_denied.
    let denied = ada
        .post(&call("update_note"), json!({"code": "b1", "version": 1}))
        .await;
    assert_eq!(denied.status, 403, "{:?}", denied.body);
    assert_eq!(denied.error_code(), Some("permission_denied"));
    assert_eq!(row().await, before);

    // A clerk may write only her own notes (`if_owner`): another clerk's note is not found.
    let hidden = ann
        .post(&call("update_note"), json!({"code": "b1", "version": 1}))
        .await;
    assert_eq!(hidden.status, 404, "{:?}", hidden.body);
    assert_eq!(hidden.error_code(), Some("not_found"));
    assert_eq!(row().await, before);

    // The owner and the boss can.
    let own = bob
        .post(&call("update_note"), json!({"code": "b1", "version": 1}))
        .await;
    assert_eq!(own.status, 200, "{:?}", own.body);
    let all = boss
        .post(&call("update_note"), json!({"code": "b1", "version": 2}))
        .await;
    assert_eq!(all.status, 200, "{:?}", all.body);
    assert_eq!(row().await, [["3", "7", "boss"]]);
    db.cleanup().await;
}

/// M9.1 review, carry-over: a read that PostgreSQL refuses and the handler ignores must not lose
/// the handler's other writes. Reads run in a savepoint, like writes.
#[tokio::test]
async fn a_read_the_server_refuses_and_the_handler_ignores_loses_nothing() {
    let db = ready().await;
    let ann = login(&db, "ann").await;
    let reply = ann
        .post(&call("swallowed_read"), json!({"code": "n"}))
        .await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    assert_eq!(reply.data(), &json!({"swallowed": true}));
    // Both writes, the one before the failed read and the one after it, were committed.
    assert_eq!(codes(&db).await, ["n-1", "n-2"]);

    // The same through the documents handle of a job.
    db.exec(
        "INSERT INTO fb_job (id, name, payload, status, enqueued_by) \
         VALUES (gen_random_uuid(), 'shop.swallow', '{}', 'queued', 'test')",
    )
    .await;
    let run = run_jobs(&binary(), &db).await;
    assert_eq!((run.done(), run.retried(), run.dead()), (1, 0, 0));
    assert_eq!(codes(&db).await, ["job-1", "job-2", "n-1", "n-2"]);
    db.cleanup().await;
}

/// M9.2 review, carry-over: the same for `get` of a DocType without tables (the savepoint branch of
/// `Documents::get`): a read the server refuses and the handler ignores loses no write.
#[tokio::test]
async fn a_get_the_server_refuses_and_the_handler_ignores_loses_nothing() {
    let db = ready().await;
    let ann = login(&db, "ann").await;
    let reply = ann.post(&call("swallowed_get"), json!({"code": "g"})).await;
    assert_eq!(reply.status, 200, "{:?}", reply.body);
    assert_eq!(reply.data(), &json!({"swallowed": true}));
    assert_eq!(codes(&db).await, ["g-1", "g-2"]);
    db.cleanup().await;
}

#[tokio::test]
async fn a_workflow_transition_in_a_method_uses_the_callers_roles() {
    let db = ready().await;
    let (amy, cleo) = (login(&db, "amy").await, login(&db, "cleo").await);
    let made = amy.post(&call("add_ticket"), json!({"code": "t1"})).await;
    assert_eq!(made.status, 200, "{:?}", made.body);
    let body = json!({"name": "t1", "version": 1});
    // The agent has the write right but is not a Closer.
    let denied = amy.post(&call("close_ticket"), body.clone()).await;
    assert_eq!(denied.status, 403, "{:?}", denied.body);
    assert_eq!(denied.error_code(), Some("permission_denied"));
    assert_eq!(
        db.rows("SELECT workflow_state FROM doc_ticket").await,
        vec![vec!["Open".to_owned()]]
    );
    let closed = cleo.post(&call("close_ticket"), body).await;
    assert_eq!(closed.status, 200, "{:?}", closed.body);
    assert_eq!(closed.data(), &json!({"version": 2}));
    assert_eq!(
        db.rows("SELECT workflow_state FROM doc_ticket").await,
        vec![vec!["Closed".to_owned()]]
    );
    // The workflow log records the user.
    assert_eq!(
        db.rows("SELECT action, actor FROM fb_workflow_log").await,
        vec![vec!["Close".to_owned(), "cleo".to_owned()]]
    );
    db.cleanup().await;
}

// ---- one transaction ----------------------------------------------------------------------

#[tokio::test]
async fn an_error_rolls_everything_back_and_a_document_error_keeps_its_status() {
    let db = ready().await;
    let ann = login(&db, "ann").await;

    let bad = ann.post(&call("add_then_bad"), json!({"code": "n1"})).await;
    assert_eq!(bad.status, 400);
    assert_eq!(bad.error_code(), Some("method_error"));
    assert_eq!(bad.error_message(), Some("Pick another code."));
    let unprocessable = ann
        .post(&call("add_then_unprocessable"), json!({"code": "n2"}))
        .await;
    assert_eq!(unprocessable.status, 422);
    assert_eq!(unprocessable.error_code(), Some("method_error"));
    assert_eq!(unprocessable.error_message(), Some("The code is taken."));
    // A version conflict inside the handler is the document API's 409.
    let conflict = ann
        .post(&call("add_then_conflict"), json!({"code": "n3"}))
        .await;
    assert_eq!(conflict.status, 409, "{:?}", conflict.body);
    assert_eq!(conflict.error_code(), Some("conflict"));
    // Nothing was left behind by any of them.
    assert!(codes(&db).await.is_empty());

    // A refused write that the handler handles leaves the transaction usable: the next insert
    // commits.
    let handled = ann
        .post(&call("refused_then_add"), json!({"code": "n4"}))
        .await;
    assert_eq!(handled.status, 200, "{:?}", handled.body);
    assert_eq!(codes(&db).await, ["n4"]);
    db.cleanup().await;
}

#[tokio::test]
async fn an_enqueued_job_exists_only_if_the_method_commits() {
    let db = ready().await;
    let ann = login(&db, "ann").await;
    let failed = ann
        .post(&call("enqueue"), json!({"code": "j1", "fail": true}))
        .await;
    assert_eq!(failed.status, 400);
    assert_eq!(job_count(&db).await, "0");

    let queued = ann.post(&call("enqueue"), json!({"code": "j2"})).await;
    assert_eq!(queued.status, 200, "{:?}", queued.body);
    assert_eq!(job_count(&db).await, "1");
    assert_eq!(
        db.rows("SELECT enqueued_by FROM fb_job WHERE name = 'shop.log'")
            .await,
        vec![vec!["ann".to_owned()]]
    );
    // The job runs later as the system context: the user's rights are not its rights.
    let run = run_jobs(&binary(), &db).await;
    assert_eq!((run.done(), run.dead()), (1, 0));
    assert_eq!(codes(&db).await, ["j2"]);
    assert_eq!(
        db.rows("SELECT owner FROM doc_note").await,
        vec![vec!["job:shop.log".to_owned()]]
    );
    db.cleanup().await;
}

// ---- the body -----------------------------------------------------------------------------

#[tokio::test]
async fn the_body_is_a_json_object_and_an_empty_one_is_an_empty_object() {
    let db = ready().await;
    let gus = login(&db, "gus").await;
    let echo = call("echo");

    let given = gus.post(&echo, json!({"a": [1, 2], "b": null})).await;
    assert_eq!(given.data(), &json!({"a": [1, 2], "b": null}));
    // Empty, with or without a content type.
    for content_type in [None, Some("application/json")] {
        let reply = gus.post_raw(&echo, content_type, b"").await;
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        assert_eq!(reply.data(), &json!({}));
    }
    // Content types with parameters and `+json` are JSON.
    for content_type in ["application/json; charset=utf-8", "application/vnd.x+json"] {
        let reply = gus.post_raw(&echo, Some(content_type), br#"{"k":1}"#).await;
        assert_eq!(reply.data(), &json!({"k": 1}), "{content_type}");
    }
    // Not an object, or not JSON at all: 400 before the handler runs.
    for body in [
        &b"[1]"[..],
        b"\"x\"",
        b"null",
        b"1",
        b"{",
        b"not json",
        b" ",
    ] {
        let reply = gus.post_raw(&echo, Some("application/json"), body).await;
        assert_eq!(reply.status, 400, "{:?}", String::from_utf8_lossy(body));
        assert_eq!(reply.error_code(), Some("bad_request"));
    }
    // A body without a JSON content type is 415.
    for content_type in [None, Some("text/plain")] {
        let reply = gus.post_raw(&echo, content_type, b"{}").await;
        assert_eq!(reply.status, 415, "{content_type:?}");
        assert_eq!(reply.error_code(), Some("unsupported_media_type"));
    }
    // Over the body limit (1 MiB by default).
    let big = vec![b' '; 1024 * 1024 + 1];
    let reply = gus.post_raw(&echo, Some("application/json"), &big).await;
    assert_eq!(reply.status, 413);
    assert_eq!(reply.error_code(), Some("payload_too_large"));
    // Query parameters are refused, as elsewhere.
    let reply = gus.post(&format!("{echo}?x=1"), json!({})).await;
    assert_eq!(reply.status, 400);
    assert_eq!(reply.error_code(), Some("invalid_query"));
    db.cleanup().await;
}

// ---- errors -------------------------------------------------------------------------------

fn assert_generic_500(reply: &Reply) {
    assert_eq!(reply.status, 500, "{:?}", reply.body);
    assert_eq!(
        reply.body,
        json!({"error": {"code": "internal", "message": "internal server error"}})
    );
}

#[tokio::test]
async fn an_internal_error_is_a_generic_500_and_a_panic_does_not_stop_the_server() {
    let db = ready().await;
    let ann = login(&db, "ann").await;

    // An app that enqueues a job that does not exist: its mistake, not the caller's.
    assert_generic_500(&ann.post(&call("enqueue_unknown"), json!({})).await);

    // A panic: a generic 500 with nothing of the panic message, and nothing left behind.
    let panicked = ann
        .post(&call("add_then_panic"), json!({"code": "p1"}))
        .await;
    assert_generic_500(&panicked);
    assert!(!panicked.body.to_string().contains("secret"));
    assert!(codes(&db).await.is_empty());
    // The server and its pool still work, and the next call commits.
    let next = ann.post(&call("add_note"), json!({"code": "p2"})).await;
    assert_eq!(next.status, 200, "{:?}", next.body);
    assert_eq!(codes(&db).await, ["p2"]);
    db.cleanup().await;
}

// ---- startup rules ------------------------------------------------------------------------

fn problems(app: App) -> String {
    Forgebase::new()
        .app(app)
        .check()
        .expect_err("a startup error")
        .to_string()
}

#[test]
fn a_method_needs_exactly_one_kind_of_access_and_a_valid_unique_name() {
    let ping = || Method::new(Shop(Behavior::Ping));
    // Fine: both kinds, in two methods.
    let ok = App::new("a")
        .method("by_role", ping().roles(&["X"]))
        .method("any", ping().any_user())
        .method("more_roles", ping().roles(&["X"]).roles(&["Y"]));
    assert!(Forgebase::new().app(ok).check().is_ok());

    let text = problems(
        App::new("a")
            .method("neither", ping())
            .method("empty", ping().roles(&[]))
            .method("both", ping().roles(&["X"]).any_user())
            .method("Bad Name", ping().any_user())
            .method("dup", ping().any_user())
            .method("dup", ping().any_user())
            .method("bad_role", ping().roles(&[" "])),
    );
    for expected in [
        "app `a`: method `neither`: no access given",
        "app `a`: method `empty`: an empty role list",
        "app `a`: method `both`: both .roles(..) and .any_user() are given",
        "app `a`: method `Bad Name`: name \"Bad Name\"",
        "app `a`: method `dup` is registered more than once",
        "app `a`: method `bad_role`: invalid role",
    ] {
        assert!(text.contains(expected), "{expected} in {text}");
    }
}

// ---- jobs: the documents handle is still the system context --------------------------------

#[tokio::test]
async fn a_jobs_documents_handle_is_still_the_system_context() {
    let db = ready().await;
    // `ann` may write only her own notes, and the job is not her: it writes as the system
    // context, as `job:shop.log`.
    let queued = login(&db, "ann")
        .await
        .post(&call("enqueue"), json!({"code": "sys"}))
        .await;
    assert_eq!(queued.status, 200);
    assert_eq!(run_jobs(&binary(), &db).await.done(), 1);
    assert_eq!(codes(&db).await, ["sys"]);
    db.exec("INSERT INTO fb_job (id, name, payload, status, enqueued_by) VALUES (gen_random_uuid(), 'shop.bad', '{}', 'queued', 'test')")
        .await;
    // A refusal (validation) is permanent: the job is dead after one attempt, as before.
    let run = run_jobs(&binary(), &db).await;
    assert_eq!((run.done(), run.dead(), run.retried()), (0, 1, 0));
    // The retryable side of the mapping is the unit test in `src/documents.rs`.
    db.cleanup().await;
}
