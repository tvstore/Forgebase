//! Workflows (M8.1, ADR 0015) against a real PostgreSQL (see `common`): the authorization of
//! transitions, the pipelines of the four docstatus moves, atomicity with the log and the jobs,
//! what changes for a DocType with a workflow, the schema and the backfill of `migrate`.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{TestDb, registry};
use forgebase_core::document::{Changes, DocStatus, Document, ValidationError};
use forgebase_core::hooks::{DocTypeHooks, HookContext, HookError, Operation};
use forgebase_core::meta::Registry;
use forgebase_core::permission::{Caller, Right};
use forgebase_core::value::Value;
use forgebase_db::{
    Conflict, DbError, Filter, FilterOp, ListQuery, Sort, StoredDocument, SyncOptions, Warning,
    amend, available_transitions, cancel, delete, get, history, insert, list, migrate, submit,
    transition, update,
};

const ORDER: &str = r#"
name = "order"
label = "Order"
submittable = true

[[fields]]
name = "title"
type = "data"
required = true
[[fields]]
name = "total"
type = "int"
[[fields]]
name = "remarks"
type = "text"
allow_on_submit = true

# Clerk writes anyone's drafts; Manager does everything; Auditor has the
# submit and cancel rights without being a role of any transition but Void; Reviewer is a role
# of Approve but has no right to submit; Self Service may change only its own documents; Spy
# reads only its own documents.
[[permissions]]
role = "Clerk"
read = true
create = true
write = true
[[permissions]]
role = "Manager"
read = true
create = true
write = true
delete = true
submit = true
cancel = true
amend = true
[[permissions]]
role = "Auditor"
read = true
submit = true
cancel = true
[[permissions]]
role = "Reviewer"
read = true
write = true
[[permissions]]
role = "Self Service"
read = true
[[permissions]]
role = "Self Service"
create = true
write = true
if_owner = true
[[permissions]]
role = "Spy"
read = true
write = true
submit = true
if_owner = true

[workflow]
[[workflow.states]]
name = "Draft"
docstatus = 0
[[workflow.states]]
name = "Pending"
docstatus = 0
style = "warning"
edit_roles = ["Manager"]
[[workflow.states]]
name = "Approved"
docstatus = 1
style = "success"
[[workflow.states]]
name = "Archived"
docstatus = 1
[[workflow.states]]
name = "Cancelled"
docstatus = 2
style = "danger"

[[workflow.transitions]]
action = "Send"
from = "Draft"
to = "Pending"
roles = ["Clerk", "Self Service", "Spy"]
[[workflow.transitions]]
action = "Reject"
from = "Pending"
to = "Draft"
roles = ["Manager"]
[[workflow.transitions]]
action = "Approve"
from = "Pending"
to = "Approved"
roles = ["Manager", "Reviewer", "Spy"]
[[workflow.transitions]]
action = "Archive"
from = "Approved"
to = "Archived"
roles = ["Manager", "Reviewer"]
[[workflow.transitions]]
action = "Cancel"
from = "Approved"
to = "Cancelled"
roles = ["Manager"]
[[workflow.transitions]]
action = "Void"
from = "Approved"
to = "Cancelled"
roles = ["Auditor"]
"#;

/// The same DocType before it had a workflow (for the backfill): no `[workflow]`, same rules.
fn order_without_workflow() -> String {
    ORDER
        .split("[workflow]")
        .next()
        .expect("text before the workflow")
        .to_owned()
}

type Log = Arc<Mutex<Vec<String>>>;

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn order(title: &str) -> Document {
    Document {
        name: None,
        values: BTreeMap::from([("title".to_owned(), text(title))]),
        tables: BTreeMap::new(),
    }
}

fn set(field: &str, value: Value) -> Changes {
    Changes {
        values: BTreeMap::from([(field.to_owned(), value)]),
        tables: BTreeMap::new(),
    }
}

fn user(name: &str, roles: &[&str]) -> Caller {
    Caller::user(name, roles.iter().map(|r| (*r).to_owned()).collect())
}

fn system() -> Caller {
    Caller::system("setup")
}

fn taken(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().expect("log"))
}

/// Records `<hook>:<operation>[:<action>]` for the hooks of a transition; fails or changes the
/// document where it is told to.
#[derive(Default)]
struct Recorder {
    log: Log,
    /// Veto `before_transition` with this message.
    veto: Option<&'static str>,
    /// Veto `validate` for this operation.
    veto_validate: Option<Operation>,
    /// Set these values in `before_transition` (only for this action, when given).
    set: BTreeMap<String, Value>,
    set_on: Option<&'static str>,
    /// Enqueue this job in `before_transition`.
    job: Option<&'static str>,
}

impl Recorder {
    fn note(&self, hook: &str, ctx: &HookContext<'_>) {
        let action = ctx
            .transition
            .map(|t| format!(":{}:{}>{}", t.action, t.from, t.to));
        self.log.lock().expect("log").push(format!(
            "{hook}:{:?}{}",
            ctx.operation,
            action.unwrap_or_default()
        ));
    }
}

impl DocTypeHooks for Recorder {
    fn before_transition(
        &self,
        ctx: &HookContext<'_>,
        doc: &mut Document,
    ) -> Result<(), HookError> {
        self.note("before_transition", ctx);
        if self
            .set_on
            .is_none_or(|action| ctx.transition.is_some_and(|t| t.action == action))
        {
            doc.values.extend(self.set.clone());
        }
        if let Some(job) = self.job {
            ctx.enqueue(job, serde_json::json!({}))?;
        }
        match self.veto {
            Some(message) => Err(HookError::new(message)),
            None => Ok(()),
        }
    }
    fn validate(&self, ctx: &HookContext<'_>, _doc: &mut Document) -> Result<(), HookError> {
        self.note("validate", ctx);
        if self.veto_validate == Some(ctx.operation) {
            return Err(HookError::new("validate says no"));
        }
        Ok(())
    }
    fn before_submit(&self, ctx: &HookContext<'_>, _doc: &mut Document) -> Result<(), HookError> {
        self.note("before_submit", ctx);
        Ok(())
    }
    fn before_cancel(&self, ctx: &HookContext<'_>, _doc: &Document) -> Result<(), HookError> {
        self.note("before_cancel", ctx);
        Ok(())
    }
}

struct World {
    db: TestDb,
    registry: Registry,
}

async fn world() -> World {
    world_with(None).await
}

async fn world_with(recorder: Option<Recorder>) -> World {
    let db = TestDb::migrated().await;
    let mut registry = registry(&[ORDER]);
    registry.add_job("app.notify");
    if let Some(recorder) = recorder {
        registry
            .add_hooks("order", Arc::new(recorder))
            .expect("hooks");
    }
    db.sync(&registry).await.expect("sync");
    World { db, registry }
}

impl World {
    async fn insert(&self, title: &str, caller: &Caller) -> StoredDocument {
        let mut client = self.db.pool.get().await.expect("connection");
        insert(&mut **client, &self.registry, "order", order(title), caller)
            .await
            .expect("insert")
    }

    async fn take(
        &self,
        doc: &StoredDocument,
        action: &str,
        caller: &Caller,
    ) -> Result<StoredDocument, DbError> {
        let mut client = self.db.pool.get().await.expect("connection");
        transition(
            &mut **client,
            &self.registry,
            "order",
            &doc.name,
            action,
            doc.version,
            caller,
        )
        .await
    }

    /// Takes each action as the system context and returns the last document.
    async fn walk(&self, doc: StoredDocument, actions: &[&str]) -> StoredDocument {
        let mut doc = doc;
        for action in actions {
            doc = self.take(&doc, action, &system()).await.expect(action);
        }
        doc
    }

    async fn get(&self, name: &str) -> StoredDocument {
        let mut client = self.db.pool.get().await.expect("connection");
        get(&mut **client, &self.registry, "order", name, &system())
            .await
            .expect("get")
            .expect("document")
    }

    /// The state and the actions of the transitions `caller` may take now.
    async fn actions(
        &self,
        name: &str,
        caller: Caller,
    ) -> Result<(Option<String>, Vec<String>), DbError> {
        let mut client = self.db.pool.get().await.expect("connection");
        let available =
            available_transitions(&mut **client, &self.registry, "order", name, &caller).await?;
        let actions = available
            .transitions
            .into_iter()
            .map(|t| t.action)
            .collect();
        Ok((available.state, actions))
    }

    async fn log_rows(&self) -> Vec<Vec<String>> {
        self.db
            .rows(
                "SELECT doctype, name, action, from_state, to_state, actor FROM fb_workflow_log \
                 ORDER BY id",
            )
            .await
    }

    async fn count(&self, sql: &str) -> String {
        self.db.rows(sql).await.remove(0).remove(0)
    }
}

fn state(doc: &StoredDocument) -> &str {
    doc.workflow_state.as_deref().expect("a state")
}

// ---------------------------------------------------------------------------------------------
// Authorization
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_transition_needs_a_role_of_the_transition_and_the_right() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    let doc = w.walk(doc, &["Send"]).await;
    assert_eq!(state(&doc), "Pending");

    // The role without the right: Reviewer is a role of Approve but cannot submit.
    let error = w
        .take(&doc, "Approve", &user("rita", &["Reviewer"]))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, DbError::TransitionDenied { right: Right::Submit, action, .. } if action == "Approve"),
        "{error}"
    );
    // The right without the role: Auditor can submit but is not a role of Approve.
    let error = w
        .take(&doc, "Approve", &user("ada", &["Auditor"]))
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::TransitionDenied { .. }), "{error}");
    // A user with neither, and a user who cannot even read.
    let error = w
        .take(&doc, "Approve", &user("oto", &["Other"]))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            DbError::PermissionDenied {
                right: Right::Read,
                ..
            }
        ),
        "{error}"
    );
    // Nothing changed: the log has only the Send above.
    assert_eq!(w.get(&doc.name).await, doc);
    assert_eq!(w.log_rows().await.len(), 1);

    // The role and the right: the transition is taken, and the log has the actor.
    let approved = w
        .take(&doc, "Approve", &user("mia", &["Manager"]))
        .await
        .unwrap();
    assert_eq!(state(&approved), "Approved");
    assert_eq!(approved.docstatus, Some(DocStatus::Submitted));
    assert_eq!(approved.version, doc.version + 1);
    assert_eq!(approved.modified_by, "mia");
    // Void is for Auditor, who has the cancel right.
    let voided = w
        .take(&approved, "Void", &user("ada", &["Auditor"]))
        .await
        .unwrap();
    assert_eq!(
        (state(&voided), voided.docstatus),
        ("Cancelled", Some(DocStatus::Cancelled))
    );
}

#[tokio::test]
async fn administrator_and_the_system_context_pass_the_role_check() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    let doc = w
        .take(&doc, "Send", &user("root", &["Administrator"]))
        .await
        .unwrap();
    // Neither has a role of Approve nor any permission rule.
    let doc = w.take(&doc, "Approve", &system()).await.unwrap();
    let doc = w
        .take(&doc, "Cancel", &user("root", &["Administrator"]))
        .await
        .unwrap();
    assert_eq!(state(&doc), "Cancelled");
    let actors: Vec<String> = w
        .log_rows()
        .await
        .into_iter()
        .map(|r| r[5].clone())
        .collect();
    assert_eq!(actors, ["root", "setup", "root"]);
}

#[tokio::test]
async fn if_owner_applies_to_the_right_of_a_transition() {
    let w = world().await;
    let mine = w.insert("mine", &user("sam", &["Self Service"])).await;
    let theirs = w.insert("theirs", &user("tia", &["Self Service"])).await;

    // Own document: allowed. Somebody else's, which Self Service can read: refused, like update.
    let sent = w
        .take(&mine, "Send", &user("sam", &["Self Service"]))
        .await
        .unwrap();
    assert_eq!(state(&sent), "Pending");
    let error = w
        .take(&theirs, "Send", &user("sam", &["Self Service"]))
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::TransitionDenied { .. }), "{error}");
    let mut client = w.db.pool.get().await.unwrap();
    let error = update(
        &mut **client,
        &w.registry,
        "order",
        &theirs.name,
        theirs.version,
        &set("total", Value::Int(1)),
        &user("sam", &["Self Service"]),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::PermissionDenied { .. }), "{error}");
    drop(client);

    // A role whose read right is `if_owner` does not see the document of somebody else.
    let spy = user("sue", &["Spy"]);
    let error = w.take(&theirs, "Send", &spy).await.unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
    // Even with a stale version: not found comes first.
    let mut stale = theirs.clone();
    stale.version = 99;
    let error = w.take(&stale, "Send", &spy).await.unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
    // Its own document (the system context records `sue` as its owner): Send needs write and
    // Approve needs submit, which Spy has only `if_owner`.
    let own = w.insert("own", &Caller::system("sue")).await;
    let own = w.take(&own, "Send", &spy).await.unwrap();
    let own = w.take(&own, "Approve", &spy).await.unwrap();
    assert_eq!(state(&own), "Approved");
}

#[tokio::test]
async fn a_wrong_state_and_an_unknown_action_are_unknown_transitions() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    // Approve exists, but not from Draft.
    let error = w.take(&doc, "Approve", &system()).await.unwrap_err();
    let DbError::UnknownTransition { state, action, .. } = &error else {
        panic!("expected UnknownTransition, got {error}");
    };
    assert_eq!((state.as_str(), action.as_str()), ("Draft", "Approve"));
    assert_eq!(
        error.to_string(),
        format!(
            "order `{}` is in state `Draft`: no transition `Approve` from it",
            doc.name
        )
    );
    for action in ["Nope", "send", ""] {
        let error = w.take(&doc, action, &system()).await.unwrap_err();
        assert!(
            matches!(error, DbError::UnknownTransition { .. }),
            "{action}: {error}"
        );
    }
    // A document without a state has no transitions.
    w.db.exec(&format!(
        "UPDATE doc_order SET workflow_state = NULL WHERE name = '{}'",
        doc.name
    ))
    .await;
    let error = w.take(&doc, "Send", &system()).await.unwrap_err();
    assert!(
        matches!(&error, DbError::UnknownTransition { state, .. } if state == "(none)"),
        "{error}"
    );
    assert!(w.log_rows().await.is_empty());
}

#[tokio::test]
async fn a_stale_version_is_a_conflict_before_the_action_is_looked_at() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    let mut stale = doc.clone();
    stale.version = 7;
    for action in ["Send", "Nope"] {
        let error = w.take(&stale, action, &system()).await.unwrap_err();
        assert!(
            matches!(
                error,
                DbError::Conflict {
                    expected: 7,
                    current: 1,
                    ..
                }
            ),
            "{action}: {error}"
        );
    }
}

#[tokio::test]
async fn of_two_concurrent_transitions_one_wins_and_the_other_conflicts() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    let mut first = w.db.pool.get().await.unwrap();
    let mut second = w.db.pool.get().await.unwrap();

    // The first transaction takes the transition and has not committed yet.
    let raw: &mut forgebase_db::Client = &mut first;
    let mut tx = raw.transaction().await.unwrap();
    transition(
        &mut tx,
        &w.registry,
        "order",
        &doc.name,
        "Send",
        doc.version,
        &system(),
    )
    .await
    .unwrap();

    // The second holds the same version and waits for the row lock ...
    let caller = Caller::system("two");
    let error = {
        let late = transition(
            &mut **second,
            &w.registry,
            "order",
            &doc.name,
            "Send",
            doc.version,
            &caller,
        );
        tokio::pin!(late);
        assert!(
            tokio::time::timeout(Duration::from_millis(300), &mut late)
                .await
                .is_err(),
            "the second transition should wait for the first transaction"
        );
        // ... and once the first commits the version no longer matches.
        tx.commit().await.unwrap();
        late.await.unwrap_err()
    };
    assert!(
        matches!(
            error,
            DbError::Conflict {
                expected: 1,
                current: 2,
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(w.log_rows().await.len(), 1);
    assert_eq!(state(&w.get(&doc.name).await), "Pending");
}

// ---------------------------------------------------------------------------------------------
// The pipelines of the four moves
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_draft_to_draft_transition_updates_the_whole_document() {
    let log = Log::default();
    let recorder = Recorder {
        log: log.clone(),
        set: BTreeMap::from([("total".to_owned(), Value::Int(42))]),
        ..Recorder::default()
    };
    let w = world_with(Some(recorder)).await;
    let doc = w.insert("a", &system()).await;
    taken(&log);

    let sent = w.take(&doc, "Send", &system()).await.unwrap();
    assert_eq!(state(&sent), "Pending");
    assert_eq!(sent.docstatus, Some(DocStatus::Draft));
    assert_eq!(sent.version, 2);
    // The hook's change is validated and written with the state.
    assert_eq!(sent.values["total"], Value::Int(42));
    // `ctx.transition` is filled in for before_transition, and the validate hooks of a
    // draft-to-draft move run with Operation::Transition.
    assert_eq!(
        taken(&log),
        [
            "before_transition:Transition:Send:Draft>Pending",
            "validate:Transition:Send:Draft>Pending",
        ]
    );
    assert_eq!(
        w.log_rows().await,
        [[
            "order",
            doc.name.as_str(),
            "Send",
            "Draft",
            "Pending",
            "setup"
        ]]
    );
}

#[tokio::test]
async fn a_draft_to_submitted_transition_runs_the_submit_pipeline_after_before_transition() {
    let log = Log::default();
    let recorder = Recorder {
        log: log.clone(),
        ..Recorder::default()
    };
    let w = world_with(Some(recorder)).await;
    let doc = w.insert("a", &system()).await;
    let doc = w.take(&doc, "Send", &system()).await.unwrap();
    taken(&log);

    let approved = w.take(&doc, "Approve", &system()).await.unwrap();
    assert_eq!(approved.docstatus, Some(DocStatus::Submitted));
    assert_eq!(state(&approved), "Approved");
    // before_transition first, then validate and before_submit with Operation::Submit.
    assert_eq!(
        taken(&log),
        [
            "before_transition:Transition:Approve:Pending>Approved",
            "validate:Submit",
            "before_submit:Submit",
        ]
    );
}

#[tokio::test]
async fn a_submitted_to_submitted_transition_changes_allow_on_submit_fields_only() {
    // The hook changes `total`, which is locked once the document is submitted.
    let recorder = Recorder {
        set: BTreeMap::from([("total".to_owned(), Value::Int(5))]),
        set_on: Some("Archive"),
        ..Recorder::default()
    };
    let w = world_with(Some(recorder)).await;
    let doc = w.insert("a", &system()).await;
    let doc = w.walk(doc, &["Send", "Approve"]).await;
    let error = w.take(&doc, "Archive", &system()).await.unwrap_err();
    assert!(
        matches!(&error, DbError::FieldLocked { field, .. } if field == "total"),
        "{error}"
    );
    assert_eq!(w.get(&doc.name).await, doc);
    assert_eq!(w.log_rows().await.len(), 2, "Send and Approve only");

    // `remarks` is allow_on_submit: the change is written with the new state.
    let recorder = Recorder {
        set: BTreeMap::from([("remarks".to_owned(), text("checked"))]),
        set_on: Some("Archive"),
        ..Recorder::default()
    };
    let w = world_with(Some(recorder)).await;
    let doc = w.insert("a", &system()).await;
    let doc = w.walk(doc, &["Send", "Approve"]).await;
    let archived = w.take(&doc, "Archive", &system()).await.unwrap();
    assert_eq!(state(&archived), "Archived");
    assert_eq!(archived.docstatus, Some(DocStatus::Submitted));
    assert_eq!(archived.values["remarks"], text("checked"));
    assert_eq!(archived.values["title"], text("a"));
    assert_eq!(archived.version, doc.version + 1);
}

#[tokio::test]
async fn a_submitted_to_cancelled_transition_runs_the_cancel_pipeline() {
    let log = Log::default();
    let recorder = Recorder {
        log: log.clone(),
        ..Recorder::default()
    };
    let w = world_with(Some(recorder)).await;
    let doc = w.insert("a", &system()).await;
    let doc = w.walk(doc, &["Send", "Approve"]).await;
    taken(&log);

    let cancelled = w.take(&doc, "Cancel", &system()).await.unwrap();
    assert_eq!(cancelled.docstatus, Some(DocStatus::Cancelled));
    assert_eq!(state(&cancelled), "Cancelled");
    assert_eq!(
        taken(&log),
        [
            "before_transition:Transition:Cancel:Approved>Cancelled",
            "before_cancel:Cancel",
        ]
    );
}

#[tokio::test]
async fn before_transition_may_change_only_allow_on_submit_fields_of_a_cancel() {
    let recorder = Recorder {
        set: BTreeMap::from([("total".to_owned(), Value::Int(5))]),
        set_on: Some("Cancel"),
        ..Recorder::default()
    };
    let w = world_with(Some(recorder)).await;
    let doc = w.insert("a", &system()).await;
    let doc = w.walk(doc, &["Send", "Approve"]).await;
    let error = w.take(&doc, "Cancel", &system()).await.unwrap_err();
    assert!(
        matches!(&error, DbError::FieldLocked { field, .. } if field == "total"),
        "{error}"
    );
    assert_eq!(w.get(&doc.name).await, doc);

    let recorder = Recorder {
        set: BTreeMap::from([("remarks".to_owned(), text("why"))]),
        set_on: Some("Cancel"),
        ..Recorder::default()
    };
    let w = world_with(Some(recorder)).await;
    let doc = w.insert("a", &system()).await;
    let doc = w.walk(doc, &["Send", "Approve"]).await;
    let cancelled = w.take(&doc, "Cancel", &system()).await.unwrap();
    assert_eq!(cancelled.docstatus, Some(DocStatus::Cancelled));
    assert_eq!(cancelled.values["remarks"], text("why"));
}

// ---------------------------------------------------------------------------------------------
// Atomicity
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_veto_leaves_the_document_the_log_and_the_jobs_unchanged() {
    let recorder = Recorder {
        veto: Some("not today"),
        job: Some("app.notify"),
        ..Recorder::default()
    };
    let w = world_with(Some(recorder)).await;
    let doc = w.insert("a", &system()).await;
    let error = w.take(&doc, "Send", &system()).await.unwrap_err();
    assert!(
        matches!(&error, DbError::Hook { hook: "before_transition", message, .. } if message == "not today"),
        "{error}"
    );
    assert_eq!(w.get(&doc.name).await, doc);
    assert!(w.log_rows().await.is_empty());
    assert_eq!(w.count("SELECT count(*)::text FROM fb_job").await, "0");
}

#[tokio::test]
async fn a_failure_after_the_hook_rolls_everything_back() {
    // before_transition enqueues a job, then a validate hook refuses the draft-to-draft move.
    let recorder = Recorder {
        job: Some("app.notify"),
        veto_validate: Some(Operation::Transition),
        ..Recorder::default()
    };
    let w = world_with(Some(recorder)).await;
    let doc = w.insert("a", &system()).await;
    let error = w.take(&doc, "Send", &system()).await.unwrap_err();
    assert!(
        matches!(
            error,
            DbError::Hook {
                hook: "validate",
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(w.get(&doc.name).await, doc);
    assert!(w.log_rows().await.is_empty());
    assert_eq!(w.count("SELECT count(*)::text FROM fb_job").await, "0");

    // The framework validation after the hook: the hook clears a required field.
    let recorder = Recorder {
        job: Some("app.notify"),
        set: BTreeMap::from([("title".to_owned(), Value::Null)]),
        ..Recorder::default()
    };
    let w = world_with(Some(recorder)).await;
    let doc = w.insert("a", &system()).await;
    let error = w.take(&doc, "Send", &system()).await.unwrap_err();
    let DbError::Validation(errors) = &error else {
        panic!("expected Validation, got {error}");
    };
    assert_eq!(
        errors,
        &[ValidationError::Required {
            field: "title".into()
        }]
    );
    assert_eq!(w.get(&doc.name).await, doc);
    assert!(w.log_rows().await.is_empty());
    assert_eq!(w.count("SELECT count(*)::text FROM fb_job").await, "0");
}

#[tokio::test]
async fn a_committed_transition_has_exactly_one_log_row_and_its_jobs() {
    let recorder = Recorder {
        job: Some("app.notify"),
        ..Recorder::default()
    };
    let w = world_with(Some(recorder)).await;
    let doc = w.insert("a", &system()).await;
    w.take(&doc, "Send", &system()).await.unwrap();
    assert_eq!(w.log_rows().await.len(), 1);
    assert_eq!(w.count("SELECT count(*)::text FROM fb_job").await, "1");

    // In a caller-owned transaction that is rolled back, the log row goes with it.
    let doc = w.get(&doc.name).await;
    let mut client = w.db.pool.get().await.unwrap();
    let raw: &mut forgebase_db::Client = &mut client;
    let mut tx = raw.transaction().await.unwrap();
    transition(
        &mut tx,
        &w.registry,
        "order",
        &doc.name,
        "Approve",
        doc.version,
        &system(),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    drop(client);
    assert_eq!(w.log_rows().await.len(), 1);
    assert_eq!(w.count("SELECT count(*)::text FROM fb_job").await, "1");
    assert_eq!(state(&w.get(&doc.name).await), "Pending");
}

// ---------------------------------------------------------------------------------------------
// What changes for a DocType with a workflow
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn plain_submit_and_cancel_are_refused_before_anything_is_looked_up() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    let doc = w.walk(doc, &["Send"]).await;
    let mut client = w.db.pool.get().await.unwrap();
    // The system context included, for a draft, for a document that does not exist and with a
    // stale version: the answer never depends on the document.
    for name in [doc.name.as_str(), "nobody"] {
        for version in [doc.version, 99] {
            let error = submit(
                &mut **client,
                &w.registry,
                "order",
                name,
                version,
                &system(),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(&error, DbError::WorkflowRequired { operation: "submit", doctype } if doctype == "order"),
                "{error}"
            );
            let error = cancel(
                &mut **client,
                &w.registry,
                "order",
                name,
                version,
                &system(),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(
                    &error,
                    DbError::WorkflowRequired {
                        operation: "cancel",
                        ..
                    }
                ),
                "{error}"
            );
        }
    }
    // Also for a caller who may not even read the DocType.
    let error = submit(
        &mut **client,
        &w.registry,
        "order",
        &doc.name,
        doc.version,
        &user("oto", &["Other"]),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::WorkflowRequired { .. }), "{error}");
    assert_eq!(
        error.to_string(),
        "DocType `order` has a workflow; use a transition (cannot submit)"
    );
    drop(client);
    assert_eq!(w.get(&doc.name).await, doc);
}

#[tokio::test]
async fn an_update_in_a_state_with_edit_roles_needs_one_of_them() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    let mut client = w.db.pool.get().await.unwrap();
    let clerk = user("cleo", &["Clerk"]);
    // Draft has no edit_roles: the write right alone decides.
    let doc = update(
        &mut **client,
        &w.registry,
        "order",
        &doc.name,
        1,
        &set("total", Value::Int(1)),
        &clerk,
    )
    .await
    .unwrap();
    let pending = w.take(&doc, "Send", &clerk).await.unwrap();
    assert_eq!(state(&pending), "Pending");

    // Pending has edit_roles = [Manager]: a Clerk with the write right is refused ...
    let error = update(
        &mut **client,
        &w.registry,
        "order",
        &pending.name,
        pending.version,
        &set("total", Value::Int(2)),
        &clerk,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::EditDenied { state, .. } if state == "Pending"),
        "{error}"
    );
    // ... also a role that has the right and no edit role, but a Manager, an administrator and
    // the system context pass.
    for (who, expected) in [
        (user("mia", &["Manager"]), 3),
        (user("root", &["Administrator"]), 4),
        (system(), 5),
    ] {
        let current = w.get(&pending.name).await;
        let updated = update(
            &mut **client,
            &w.registry,
            "order",
            &current.name,
            current.version,
            &set("total", Value::Int(expected)),
            &who,
        )
        .await
        .unwrap();
        assert_eq!(updated.values["total"], Value::Int(expected));
        assert_eq!(
            state(&updated),
            "Pending",
            "an update never changes the state"
        );
    }
}

#[tokio::test]
async fn insert_and_amend_start_in_the_initial_state() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    assert_eq!(state(&doc), "Draft");
    assert_eq!(doc.docstatus, Some(DocStatus::Draft));
    assert_eq!(
        w.db.rows("SELECT workflow_state FROM doc_order").await,
        [["Draft"]]
    );

    let cancelled = w.walk(doc, &["Send", "Approve", "Cancel"]).await;
    assert_eq!(state(&cancelled), "Cancelled");
    let mut client = w.db.pool.get().await.unwrap();
    let amended = amend(
        &mut **client,
        &w.registry,
        "order",
        &cancelled.name,
        cancelled.version,
        &user("mia", &["Manager"]),
    )
    .await
    .unwrap();
    assert_eq!(state(&amended), "Draft");
    assert_eq!(amended.docstatus, Some(DocStatus::Draft));
    assert_eq!(
        amended.amended_from.as_deref(),
        Some(cancelled.name.as_str())
    );
    drop(client);
    // The history stays with the cancelled document; the amendment starts with none.
    let log = w.log_rows().await;
    assert!(log.iter().all(|row| row[1] == cancelled.name), "{log:?}");
}

#[tokio::test]
async fn deleting_a_document_deletes_its_log_rows_in_the_same_transaction() {
    let w = world().await;
    let kept = w.insert("kept", &system()).await;
    w.walk(kept.clone(), &["Send"]).await;
    let gone = w.insert("gone", &system()).await;
    let gone = w.walk(gone, &["Send", "Reject"]).await;
    assert_eq!(w.log_rows().await.len(), 3);

    let mut client = w.db.pool.get().await.unwrap();
    // A refused delete keeps the log (a stale version).
    let error = delete(
        &mut **client,
        &w.registry,
        "order",
        &gone.name,
        1,
        &user("mia", &["Manager"]),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Conflict { .. }), "{error}");
    assert_eq!(w.log_rows().await.len(), 3);

    delete(
        &mut **client,
        &w.registry,
        "order",
        &gone.name,
        gone.version,
        &user("mia", &["Manager"]),
    )
    .await
    .unwrap();
    drop(client);
    let log = w.log_rows().await;
    assert_eq!(log.len(), 1);
    assert_eq!(log[0][1], kept.name);

    // A name can be reused: the new document has a clean history.
    w.db.exec(&format!(
        "UPDATE doc_order SET name = 'reused' WHERE name = '{}'",
        kept.name
    ))
    .await;
    w.db.exec(&format!(
        "UPDATE fb_workflow_log SET name = 'reused' WHERE name = '{}'",
        kept.name
    ))
    .await;
    let reused = w.get("reused").await;
    let mut client = w.db.pool.get().await.unwrap();
    let entries = history(&mut **client, &w.registry, "order", "reused", &system())
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    // Reject it back to draft and delete it: the log goes too.
    drop(client);
    let reused = w.take(&reused, "Reject", &system()).await.unwrap();
    let mut client = w.db.pool.get().await.unwrap();
    delete(
        &mut **client,
        &w.registry,
        "order",
        "reused",
        reused.version,
        &system(),
    )
    .await
    .unwrap();
    assert!(w.log_rows().await.is_empty());
}

#[tokio::test]
async fn a_client_cannot_set_the_workflow_state() {
    let w = world().await;
    let mut client = w.db.pool.get().await.unwrap();
    let mut doc = order("a");
    doc.values
        .insert("workflow_state".to_owned(), text("Approved"));
    let error = insert(&mut **client, &w.registry, "order", doc, &system())
        .await
        .unwrap_err();
    let DbError::Validation(errors) = &error else {
        panic!("expected Validation, got {error}");
    };
    assert_eq!(
        errors,
        &[ValidationError::UnknownField {
            field: "workflow_state".into()
        }]
    );
    assert_eq!(w.count("SELECT count(*)::text FROM doc_order").await, "0");

    let doc = w.insert("a", &system()).await;
    let error = update(
        &mut **client,
        &w.registry,
        "order",
        &doc.name,
        doc.version,
        &set("workflow_state", text("Approved")),
        &system(),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::Validation(_)), "{error}");
    drop(client);
    assert_eq!(w.get(&doc.name).await, doc);
}

#[tokio::test]
async fn a_doctype_without_a_workflow_has_no_state_column_and_no_transitions() {
    let db = TestDb::migrated().await;
    let registry = registry(&[&order_without_workflow()]);
    db.sync(&registry).await.unwrap();
    let columns: Vec<String> = db
        .columns("doc_order")
        .await
        .into_iter()
        .map(|c| c.0)
        .collect();
    assert!(
        !columns.contains(&"workflow_state".to_owned()),
        "{columns:?}"
    );
    let mut client = db.pool.get().await.unwrap();
    let doc = insert(&mut **client, &registry, "order", order("a"), &system())
        .await
        .unwrap();
    assert_eq!(doc.workflow_state, None);

    let error = transition(
        &mut **client,
        &registry,
        "order",
        &doc.name,
        "Send",
        1,
        &system(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::NoWorkflow { doctype } if doctype == "order"),
        "{error}"
    );
    let error = available_transitions(&mut **client, &registry, "order", &doc.name, &system())
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::NoWorkflow { .. }), "{error}");
    let error = history(&mut **client, &registry, "order", &doc.name, &system())
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::NoWorkflow { .. }), "{error}");
    // Plain submit and cancel work as before, and the state is not a column to filter by.
    let submitted = submit(&mut **client, &registry, "order", &doc.name, 1, &system())
        .await
        .unwrap();
    assert_eq!(submitted.docstatus, Some(DocStatus::Submitted));
    let query = ListQuery {
        filters: vec![Filter::new("workflow_state", FilterOp::IsNull(true))],
        ..ListQuery::default()
    };
    let error = list(&**client, &registry, "order", &system(), &query)
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::InvalidQuery(_)), "{error}");
}

// ---------------------------------------------------------------------------------------------
// The list of transitions, the history, list filters
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn available_transitions_follow_the_role_the_right_and_if_owner() {
    let w = world().await;
    let doc = w.insert("a", &Caller::system("sue")).await;
    let doc = w.walk(doc, &["Send"]).await;
    let mut client = w.db.pool.get().await.unwrap();
    // Pending: Reject (Manager) and Approve (Manager, Reviewer, Spy).
    let (state, list) = w
        .actions(&doc.name, user("mia", &["Manager"]))
        .await
        .unwrap();
    assert_eq!(state.as_deref(), Some("Pending"));
    assert_eq!(list, ["Reject", "Approve"]);
    // The role without the right gives nothing; so does the right without the role.
    assert!(
        w.actions(&doc.name, user("rita", &["Reviewer"]))
            .await
            .unwrap()
            .1
            .is_empty()
    );
    assert!(
        w.actions(&doc.name, user("ada", &["Auditor"]))
            .await
            .unwrap()
            .1
            .is_empty()
    );
    // Spy: submit if_owner, and the document is sue's.
    assert_eq!(
        w.actions(&doc.name, user("sue", &["Spy"])).await.unwrap().1,
        ["Approve"]
    );
    assert!(
        w.actions(&doc.name, user("sam", &["Spy"])).await.is_err(),
        "not readable: not found"
    );
    let error = w
        .actions(&doc.name, user("sam", &["Spy"]))
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
    // Administrator and the system context may take everything from the state.
    assert_eq!(
        w.actions(&doc.name, user("root", &["Administrator"]))
            .await
            .unwrap()
            .1,
        ["Reject", "Approve"]
    );
    assert_eq!(
        w.actions(&doc.name, system()).await.unwrap().1,
        ["Reject", "Approve"]
    );
    // Not readable at all: a permission error, as for get.
    let error = w
        .actions(&doc.name, user("oto", &["Other"]))
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::PermissionDenied { .. }), "{error}");
    // A missing document.
    let error = available_transitions(&mut **client, &w.registry, "order", "nobody", &system())
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
    // No state, no transitions.
    w.db.exec("UPDATE doc_order SET workflow_state = NULL")
        .await;
    assert_eq!(
        w.actions(&doc.name, system()).await.unwrap(),
        (None, vec![])
    );
}

#[tokio::test]
async fn the_history_is_oldest_first_and_needs_read_access() {
    let w = world().await;
    let doc = w.insert("a", &Caller::system("sue")).await;
    let doc = w.walk(doc, &["Send"]).await;
    let doc = w
        .take(&doc, "Reject", &user("mia", &["Manager"]))
        .await
        .unwrap();
    w.take(&doc, "Send", &user("cleo", &["Clerk"]))
        .await
        .unwrap();

    let mut client = w.db.pool.get().await.unwrap();
    let entries = history(
        &mut **client,
        &w.registry,
        "order",
        &doc.name,
        &user("mia", &["Manager"]),
    )
    .await
    .unwrap();
    let rows: Vec<(&str, &str, &str, &str)> = entries
        .iter()
        .map(|e| {
            (
                e.action.as_str(),
                e.from.as_str(),
                e.to.as_str(),
                e.actor.as_str(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("Send", "Draft", "Pending", "setup"),
            ("Reject", "Pending", "Draft", "mia"),
            ("Send", "Draft", "Pending", "cleo"),
        ]
    );
    // `at` is the transaction time: not decreasing.
    assert!(entries.windows(2).all(|pair| pair[0].at <= pair[1].at));
    // A reader who cannot see the document gets not found, and a DocType-level refusal.
    let error = history(
        &mut **client,
        &w.registry,
        "order",
        &doc.name,
        &user("sam", &["Spy"]),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
    let error = history(
        &mut **client,
        &w.registry,
        "order",
        &doc.name,
        &user("oto", &["Other"]),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, DbError::PermissionDenied { .. }), "{error}");
    let error = history(&mut **client, &w.registry, "order", "nobody", &system())
        .await
        .unwrap_err();
    assert!(matches!(error, DbError::NotFound { .. }), "{error}");
}

#[tokio::test]
async fn the_history_is_capped_at_the_newest_500_rows() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    w.db.exec(&format!(
        "INSERT INTO fb_workflow_log (doctype, name, action, from_state, to_state, actor) \
         SELECT 'order', '{}', 'A' || n, 'x', 'y', 'bulk' FROM generate_series(1, 502) n",
        doc.name
    ))
    .await;
    // Another document's rows are not mixed in.
    w.db.exec(
        "INSERT INTO fb_workflow_log (doctype, name, action, from_state, to_state, actor) \
         VALUES ('order', 'other', 'Z', 'x', 'y', 'bulk')",
    )
    .await;
    let mut client = w.db.pool.get().await.unwrap();
    let entries = history(&mut **client, &w.registry, "order", &doc.name, &system())
        .await
        .unwrap();
    assert_eq!(entries.len(), 500);
    assert_eq!(entries[0].action, "A3");
    assert_eq!(entries[499].action, "A502");
}

#[tokio::test]
async fn the_state_can_be_filtered_and_sorted_in_a_list() {
    let w = world().await;
    let a = w.insert("a", &system()).await;
    let b = w.insert("b", &system()).await;
    w.insert("c", &system()).await;
    w.walk(a.clone(), &["Send", "Approve"]).await;
    w.walk(b.clone(), &["Send"]).await;
    let client = w.db.pool.get().await.unwrap();
    let names = |docs: Vec<StoredDocument>| -> Vec<String> {
        docs.into_iter()
            .map(|d| d.values["title"].clone())
            .map(|v| match v {
                Value::Text(t) => t,
                other => panic!("{other:?}"),
            })
            .collect()
    };
    let query = ListQuery {
        filters: vec![Filter::new("workflow_state", FilterOp::Eq(text("Pending")))],
        ..ListQuery::default()
    };
    let found = list(&**client, &w.registry, "order", &system(), &query)
        .await
        .unwrap();
    assert_eq!(names(found), ["b"]);
    let query = ListQuery {
        filters: vec![Filter::new(
            "workflow_state",
            FilterOp::In(vec![text("Pending"), text("Draft")]),
        )],
        sort: vec![Sort::desc("workflow_state")],
        ..ListQuery::default()
    };
    let found = list(&**client, &w.registry, "order", &system(), &query)
        .await
        .unwrap();
    assert_eq!(names(found), ["b", "c"]);
    let query = ListQuery {
        filters: vec![Filter::new(
            "workflow_state",
            FilterOp::Like("App%".to_owned()),
        )],
        ..ListQuery::default()
    };
    let found = list(&**client, &w.registry, "order", &system(), &query)
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].workflow_state.as_deref(), Some("Approved"));
}

// ---------------------------------------------------------------------------------------------
// Schema and backfill
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_table_of_a_workflow_doctype_has_a_nullable_state_column() {
    let w = world().await;
    let columns = w.db.columns("doc_order").await;
    let tail: Vec<&(String, String)> = columns.iter().skip(7).take(3).collect();
    assert_eq!(
        tail,
        [
            &("docstatus".to_owned(), "smallint/NO".to_owned()),
            &("amended_from".to_owned(), "text/YES".to_owned()),
            &("workflow_state".to_owned(), "text/YES".to_owned()),
        ]
    );
    // In sync: nothing to do. Drift in the column is a conflict, like the docstatus columns.
    assert!(w.db.sync(&w.registry).await.unwrap().steps.is_empty());
    w.db.exec("ALTER TABLE doc_order ALTER COLUMN workflow_state TYPE varchar(10)")
        .await;
    let error = w.db.sync(&w.registry).await.unwrap_err();
    let DbError::SyncConflicts(conflicts) = &error else {
        panic!("expected conflicts, got {error}");
    };
    assert!(
        matches!(&conflicts[..], [Conflict::ColumnMismatch { column, .. }] if column == "workflow_state"),
        "{conflicts:?}"
    );
    w.db.exec("ALTER TABLE doc_order ALTER COLUMN workflow_state TYPE text")
        .await;
    w.db.exec("ALTER TABLE doc_order ALTER COLUMN workflow_state SET NOT NULL")
        .await;
    assert!(matches!(
        w.db.sync(&w.registry).await,
        Err(DbError::SyncConflicts(_))
    ));
}

#[tokio::test]
async fn edit_roles_apply_to_drafts_only() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    let doc = w.walk(doc, &["Send", "Approve"]).await;
    assert_eq!(doc.docstatus, Some(DocStatus::Submitted));
    // A stale state (a workflow edited after the document was written) must not block an
    // allow_on_submit update: Pending has edit_roles, but the document is not a draft.
    w.db.exec(&format!(
        "UPDATE doc_order SET workflow_state = 'Pending' WHERE name = '{}'",
        doc.name
    ))
    .await;
    let mut client = w.db.pool.get().await.unwrap();
    let updated = update(
        &mut **client,
        &w.registry,
        "order",
        &doc.name,
        doc.version,
        &set("remarks", text("late note")),
        &user("cleo", &["Clerk"]),
    )
    .await
    .unwrap();
    assert_eq!(updated.values["remarks"], text("late note"));
}

#[tokio::test]
async fn available_transitions_skip_those_the_docstatus_disagrees_with() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    let doc = w.walk(doc, &["Send"]).await;
    let (_, before) = w.actions(&doc.name, system()).await.unwrap();
    assert_eq!(before, ["Reject", "Approve"]);
    // The stored docstatus disagrees with the state Pending (draft): `transition` would refuse
    // these, so they are not offered.
    w.db.exec(&format!(
        "UPDATE doc_order SET docstatus = 1 WHERE name = '{}'",
        doc.name
    ))
    .await;
    let (state, after) = w.actions(&doc.name, system()).await.unwrap();
    assert_eq!(state.as_deref(), Some("Pending"));
    assert!(after.is_empty(), "{after:?}");
}

#[tokio::test]
async fn deleting_removes_the_log_rows_also_after_the_workflow_was_removed() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    let doc = w.walk(doc, &["Send", "Reject"]).await;
    assert_eq!(w.log_rows().await.len(), 2);
    let plain = registry(&[&order_without_workflow()]);
    let mut client = w.db.pool.get().await.unwrap();
    delete(
        &mut **client,
        &plain,
        "order",
        &doc.name,
        doc.version,
        &system(),
    )
    .await
    .unwrap();
    drop(client);
    assert!(w.log_rows().await.is_empty());
}

#[tokio::test]
async fn removing_the_workflow_keeps_the_column_and_its_data_with_a_warning() {
    let w = world().await;
    let doc = w.insert("a", &system()).await;
    let doc = w.walk(doc, &["Send"]).await;
    let plain = registry(&[&order_without_workflow()]);
    let report = w.db.sync(&plain).await.unwrap();
    assert!(report.steps.is_empty());
    assert_eq!(
        report.warnings,
        [Warning::ColumnNotInMetadata {
            table: "doc_order".to_owned(),
            column: "workflow_state".to_owned()
        }]
    );
    assert_eq!(
        w.db.rows(&format!(
            "SELECT workflow_state FROM doc_order WHERE name = '{}'",
            doc.name
        ))
        .await,
        [["Pending"]]
    );
    // Without the workflow the document behaves as before: no state in the document, and a
    // plain submit is possible again.
    let mut client = w.db.pool.get().await.unwrap();
    let stored = get(&mut **client, &plain, "order", &doc.name, &system())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.workflow_state, None);
    submit(
        &mut **client,
        &plain,
        "order",
        &doc.name,
        stored.version,
        &system(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn migrate_gives_existing_documents_their_state_and_is_idempotent() {
    let db = TestDb::new().await;
    let before = registry(&[&order_without_workflow()]);
    let options = SyncOptions::default();
    migrate(&db.pool, &before, &options).await.unwrap();
    let mut client = db.pool.get().await.unwrap();
    insert(&mut **client, &before, "order", order("draft"), &system())
        .await
        .unwrap();
    let submitted = insert(
        &mut **client,
        &before,
        "order",
        order("submitted"),
        &system(),
    )
    .await
    .unwrap();
    submit(
        &mut **client,
        &before,
        "order",
        &submitted.name,
        1,
        &system(),
    )
    .await
    .unwrap();
    let cancelled = insert(
        &mut **client,
        &before,
        "order",
        order("cancelled"),
        &system(),
    )
    .await
    .unwrap();
    let cancelled = submit(
        &mut **client,
        &before,
        "order",
        &cancelled.name,
        1,
        &system(),
    )
    .await
    .unwrap();
    cancel(
        &mut **client,
        &before,
        "order",
        &cancelled.name,
        cancelled.version,
        &system(),
    )
    .await
    .unwrap();
    drop(client);

    // The workflow is added later: the column is added and the documents get the first state
    // with their docstatus. The first state of docstatus 1 is Approved, of docstatus 2 Cancelled.
    let after = registry(&[ORDER]);
    let report = migrate(&db.pool, &after, &options).await.unwrap();
    assert!(report.workflow_warnings.is_empty());
    let first = db
        .rows("SELECT title, docstatus::text, workflow_state, version::text FROM doc_order ORDER BY title COLLATE \"C\"")
        .await;
    assert_eq!(
        first,
        [
            ["cancelled", "2", "Cancelled", "3"],
            ["draft", "0", "Draft", "1"],
            ["submitted", "1", "Approved", "2"],
        ],
        "versions are not touched"
    );

    // Again: nothing changes.
    let report = migrate(&db.pool, &after, &options).await.unwrap();
    assert!(report.sync.expect("sync").steps.is_empty());
    assert!(report.workflow_warnings.is_empty());
    let second = db
        .rows("SELECT title, docstatus::text, workflow_state, version::text FROM doc_order ORDER BY title COLLATE \"C\"")
        .await;
    assert_eq!(first, second);
    // A dry run backfills nothing either (the column of a dry run does not exist yet).
    let dry = SyncOptions {
        dry_run: true,
        ..options.clone()
    };
    migrate(&db.pool, &after, &dry).await.unwrap();

    // A state the workflow no longer defines is reported once per DocType, and left alone.
    db.exec("UPDATE doc_order SET workflow_state = 'Gone' WHERE title IN ('draft', 'submitted')")
        .await;
    let report = migrate(&db.pool, &after, &options).await.unwrap();
    assert_eq!(
        report.workflow_warnings,
        [
            "DocType `order`: 2 documents are in a workflow state that no longer exists; no transition applies to them"
        ]
    );
    db.exec("UPDATE doc_order SET workflow_state = 'Draft' WHERE title = 'draft'")
        .await;
    let report = migrate(&db.pool, &after, &options).await.unwrap();
    assert_eq!(
        report.workflow_warnings,
        [
            "DocType `order`: 1 document is in a workflow state that no longer exists; no transition applies to them"
        ]
    );
    assert_eq!(
        db.rows("SELECT workflow_state FROM doc_order WHERE title = 'submitted'")
            .await,
        [["Gone"]]
    );
    // Such a document has no transitions.
    let mut client = db.pool.get().await.unwrap();
    let stuck = get(&mut **client, &after, "order", &submitted.name, &system())
        .await
        .unwrap()
        .unwrap();
    let error = transition(
        &mut **client,
        &after,
        "order",
        &stuck.name,
        "Archive",
        stuck.version,
        &system(),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, DbError::UnknownTransition { state, .. } if state == "Gone"),
        "{error}"
    );
}
