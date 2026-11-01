//! Workflows (M8.1, ADR 0015): taking a transition, the list of what a caller may take, the
//! history of a document and the backfill that `migrate` runs.
//!
//! The rules (states, docstatus moves, who may take a transition) are in
//! [`forgebase_core::workflow`] and [`forgebase_core::permission::transition_access`]; this
//! module runs them in a transaction. A transition does not have a pipeline of its own: after
//! the lock, the checks and the `before_transition` hooks it continues in the update, submit or
//! cancel pipeline of `document.rs`, which writes the new state together with the document.

use forgebase_core::document::DocStatus;
use forgebase_core::hooks::{EnqueuedJobs, HookContext, Operation, TransitionInfo};
use forgebase_core::meta::{DocType, Registry};
use forgebase_core::permission::{Access, Caller, Right, transition_access};
use forgebase_core::workflow::{Move, Transition, Workflow};
use time::OffsetDateTime;
use tokio_postgres::{Client, GenericClient};

use crate::document::{
    Pipeline, StoredDocument, Target, as_document, cancel_tail, check_actor, check_not_locked,
    document_doctype, get, levels_of, lock_current, read_grant, require_status, run_hooks,
    submit_tail, update_tail, write_jobs, write_webhooks,
};
use crate::error::DbError;
use crate::field_levels::{redact, scrub};
use crate::ident::quote_ident;
use forgebase_core::ident::table_name;

/// The most rows [`history`] returns: the newest ones.
pub const MAX_HISTORY: i64 = 500;

/// Takes the transition `action` of a document of a DocType with a workflow and returns the
/// document as stored. The document must be at `expected_version`.
///
/// The caller needs the **read** right on the DocType and the document ([`DbError::NotFound`]
/// for one it cannot read, as everywhere), then one of the transition's roles **and** the right
/// its docstatus move needs: write for draft to draft and submitted to submitted, submit for
/// draft to submitted, cancel for submitted to cancelled. The right is decided by the
/// evaluator of [`forgebase_core::permission`] and `if_owner` works as for update, submit and
/// cancel; the role is checked by [`transition_access`]. The system context and an administrator
/// pass both ([`DbError::TransitionDenied`] otherwise).
///
/// **Pipeline.** One transaction, in this order:
///
/// 1. the DocType must have a workflow ([`DbError::NoWorkflow`]); the document is read and
///    locked (`FOR NO KEY UPDATE`): an unknown or unreadable name is [`DbError::NotFound`], a
///    `version` other than `expected_version` is [`DbError::Conflict`];
/// 2. the transition is found from the document's state and `action`
///    ([`DbError::UnknownTransition`]; a document with no state has none), then the right and
///    the role are checked;
/// 3. the `before_transition` hooks run on the whole document ([`Operation::Transition`], with
///    [`HookContext::transition`]);
/// 4. the pipeline of the docstatus move continues, and writes the new state with the rest:
///    draft to draft is an update of the whole document (`validate` hooks with
///    [`Operation::Transition`], framework validation, Links); draft to submitted is the submit
///    pipeline ([`Operation::Submit`]); submitted to submitted is an update of a submitted
///    document (only `allow_on_submit` fields may change, [`DbError::FieldLocked`]); submitted to
///    cancelled is the cancel pipeline ([`Operation::Cancel`]), where only `allow_on_submit`
///    fields may be changed by `before_transition`, too. `version` is incremented and
///    `modified_at` and `modified_by` are set;
/// 5. a row is added to `fb_workflow_log` and the jobs the hooks enqueued are written.
///
/// A hook error is [`DbError::Hook`], and a veto, a failed validation or any other error leaves
/// the document, its state, the log and the jobs unchanged: the log row exists if and only if
/// the transition committed. Of two concurrent transitions of the same version one wins and the
/// other gets [`DbError::Conflict`].
pub async fn transition(
    client: &mut impl GenericClient,
    registry: &Registry,
    doctype: &str,
    name: &str,
    action: &str,
    expected_version: i64,
    caller: &Caller,
) -> Result<StoredDocument, DbError> {
    let doctype = document_doctype(registry, doctype)?;
    let levels = levels_of(doctype, caller);
    let tx = client.transaction().await?;
    let jobs = EnqueuedJobs::new();
    let (stored, entry) = transition_in(
        &tx,
        registry,
        doctype,
        (name, action, expected_version),
        caller,
        &jobs,
    )
    .await
    .map_err(|e| scrub(e, doctype, levels))?;
    tx.execute(
        "INSERT INTO fb_workflow_log (doctype, name, action, from_state, to_state, actor) \
         VALUES ($1::text, $2::text, $3::text, $4::text, $5::text, $6::text)",
        &[
            &doctype.name,
            &stored.name,
            &entry.action,
            &entry.from,
            &entry.to,
            &caller.name(),
        ],
    )
    .await?;
    write_jobs(&tx, registry, &jobs, caller).await?;
    // A transition that submits or cancels the document is also that event.
    let events: &[&str] = match entry.docstatus_event {
        Some(event) => &["transition", event],
        None => &["transition"],
    };
    write_webhooks(&tx, registry, doctype, events, &stored, caller).await?;
    tx.commit().await?;
    Ok(redact(stored, doctype, levels))
}

/// What a transition logs.
struct Logged {
    action: String,
    from: String,
    to: String,
    /// `submit` or `cancel` when the transition moved the document's docstatus to 1 or 2.
    docstatus_event: Option<&'static str>,
}

async fn transition_in(
    client: &impl GenericClient,
    registry: &Registry,
    doctype: &DocType,
    (name, action, expected_version): (&str, &str, i64),
    caller: &Caller,
    jobs: &EnqueuedJobs,
) -> Result<(StoredDocument, Logged), DbError> {
    check_actor(caller.name())?;
    let workflow = workflow_of(doctype)?;
    let grant = read_grant(doctype, caller)?;
    // The right depends on the transition, which is found from the state in the document, so
    // the document is locked with the read grant first (and a hidden one is not found).
    let current = lock_current(
        client,
        registry,
        doctype,
        name,
        expected_version,
        "FOR NO KEY UPDATE",
        &grant,
    )
    .await?;
    let found = current
        .workflow_state
        .as_deref()
        .and_then(|state| workflow.transition(state, action));
    let Some(transition) = found else {
        return Err(DbError::UnknownTransition {
            doctype: doctype.name.clone(),
            name: current.name,
            state: current
                .workflow_state
                .unwrap_or_else(|| "(none)".to_owned()),
            action: action.to_owned(),
        });
    };
    let denied = |right| DbError::TransitionDenied {
        doctype: doctype.name.clone(),
        action: action.to_owned(),
        right,
    };
    let Some(movement) = workflow.move_of(transition) else {
        // A loaded workflow has no such transition; refused rather than half done.
        return Err(denied(Right::Write));
    };
    match transition_access(doctype, caller, transition) {
        Access::Denied => return Err(denied(movement.right())),
        Access::OwnerOnly if current.owner != caller.name() => {
            return Err(denied(movement.right()));
        }
        Access::OwnerOnly | Access::All => {}
    }
    // The state says which docstatus the document must have; a workflow edited after the
    // document was written can disagree. Refused, like submit of a submitted document.
    if doctype.submittable
        && let Some(from) = workflow.state(&transition.from)
    {
        require_status(doctype, &current, from.docstatus, "transition")?;
    }

    let info = TransitionInfo::new(action, &transition.from, &transition.to);
    let mut doc = as_document(&current);
    let ctx = HookContext::new(&doctype.name, caller.name(), Operation::Transition)
        .with_transition(&info)
        .with_jobs(jobs);
    run_hooks(registry, doctype, "before_transition", |hooks| {
        hooks.before_transition(&ctx, &mut doc)
    })?;

    let pipeline = Pipeline {
        registry,
        doctype,
        target: Target {
            name,
            expected_version,
            caller,
            read: grant.read.clone(),
        },
    };
    let state = Some(transition.to.as_str());
    let hooks =
        |operation| HookContext::new(&doctype.name, caller.name(), operation).with_jobs(jobs);
    let stored = match movement {
        Move::Edit => update_tail(client, &pipeline, &current, doc, &ctx, false, state).await?,
        Move::EditSubmitted => {
            update_tail(client, &pipeline, &current, doc, &ctx, true, state).await?
        }
        Move::Submit => {
            let ctx = hooks(Operation::Submit);
            submit_tail(client, &pipeline, &current, doc, &ctx, state).await?
        }
        Move::Cancel => {
            // Only `allow_on_submit` fields may differ, as for an update of a submitted document.
            check_not_locked(doctype, &current, &doc)?;
            let ctx = hooks(Operation::Cancel);
            cancel_tail(client, &pipeline, &current, doc, &ctx, state).await?
        }
    };
    let logged = Logged {
        action: action.to_owned(),
        from: transition.from.clone(),
        to: transition.to.clone(),
        docstatus_event: match movement {
            Move::Submit => Some("submit"),
            Move::Cancel => Some("cancel"),
            Move::Edit | Move::EditSubmitted => None,
        },
    };
    Ok((stored, logged))
}

fn workflow_of(doctype: &DocType) -> Result<&Workflow, DbError> {
    doctype
        .workflow
        .as_ref()
        .ok_or_else(|| DbError::NoWorkflow {
            doctype: doctype.name.clone(),
        })
}

/// The transitions a caller may take now, see [`available_transitions`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableTransitions {
    /// The state the document is in; `None` for a document that has none.
    pub state: Option<String>,
    /// The transitions from that state that the caller may take, in declaration order.
    pub transitions: Vec<Transition>,
}

/// The transitions from the document's state that `caller` may take now: one of the
/// transition's roles and the right of its docstatus move, with `if_owner` applied to this
/// document (the same decision as [`transition`]). The `before_transition` hooks are **not**
/// run, so a hook may still refuse one of them.
///
/// The caller needs the **read** right: a document it cannot read is [`DbError::NotFound`], as
/// for [`get`]. A DocType without a workflow is [`DbError::NoWorkflow`].
pub async fn available_transitions(
    client: &mut impl GenericClient,
    registry: &Registry,
    doctype: &str,
    name: &str,
    caller: &Caller,
) -> Result<AvailableTransitions, DbError> {
    let doctype = document_doctype(registry, doctype)?;
    let workflow = workflow_of(doctype)?;
    let stored = existing(client, registry, doctype, name, caller).await?;
    let transitions = match &stored.workflow_state {
        None => Vec::new(),
        Some(state) => workflow
            .transitions_from(state)
            // A transition whose from-state disagrees with the stored docstatus (a workflow
            // edited after the document was written) is refused by `transition`: not offered.
            .filter(|t| {
                !doctype.submittable
                    || workflow
                        .state(&t.from)
                        .is_none_or(|from| stored.docstatus == Some(from.docstatus))
            })
            .filter(|t| match transition_access(doctype, caller, t) {
                Access::All => true,
                Access::OwnerOnly => stored.owner == caller.name(),
                Access::Denied => false,
            })
            .cloned()
            .collect(),
    };
    Ok(AvailableTransitions {
        state: stored.workflow_state,
        transitions,
    })
}

/// One committed transition of a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// The action taken.
    pub action: String,
    /// The state before.
    pub from: String,
    /// The state after.
    pub to: String,
    /// Who took it: [`Caller::name`].
    pub actor: String,
    /// When: the start time of the transition's transaction.
    pub at: OffsetDateTime,
}

/// The transitions a document has taken, oldest first, at most [`MAX_HISTORY`] (the newest
/// ones when there are more). The caller needs the **read** right: a document it cannot read
/// is [`DbError::NotFound`]. A DocType without a workflow is [`DbError::NoWorkflow`].
///
/// The log is deleted with the document, and amend does not copy it: a new draft starts with an
/// empty history.
pub async fn history(
    client: &mut impl GenericClient,
    registry: &Registry,
    doctype: &str,
    name: &str,
    caller: &Caller,
) -> Result<Vec<HistoryEntry>, DbError> {
    let doctype = document_doctype(registry, doctype)?;
    workflow_of(doctype)?;
    let stored = existing(client, registry, doctype, name, caller).await?;
    let rows = client
        .query(
            "SELECT action, from_state, to_state, actor, at FROM \
               (SELECT id, action, from_state, to_state, actor, at FROM fb_workflow_log \
                WHERE doctype = $1::text AND name = $2::text ORDER BY id DESC LIMIT $3::bigint) newest \
             ORDER BY id",
            &[&doctype.name, &stored.name, &MAX_HISTORY],
        )
        .await?;
    rows.iter()
        .map(|row| {
            Ok(HistoryEntry {
                action: row.try_get(0)?,
                from: row.try_get(1)?,
                to: row.try_get(2)?,
                actor: row.try_get(3)?,
                at: row.try_get(4)?,
            })
        })
        .collect()
}

/// The document the caller may read, or [`DbError::NotFound`].
async fn existing(
    client: &mut impl GenericClient,
    registry: &Registry,
    doctype: &DocType,
    name: &str,
    caller: &Caller,
) -> Result<StoredDocument, DbError> {
    get(client, registry, &doctype.name, name, caller)
        .await?
        .ok_or_else(|| DbError::NotFound {
            doctype: doctype.name.clone(),
            name: name.to_owned(),
        })
}

/// Gives the documents that have no workflow state one, for every DocType with a workflow; run
/// by `migrate` after the schema sync, so it covers documents that existed before the workflow
/// and a workflow that was added later. A document with no state gets the first state with its
/// docstatus (a document whose docstatus no state has stays without one). Documents that have a
/// state are not touched, so it can run again and changes nothing the second time. It does not
/// change `version` or `modified_at`: nothing about the document changed, only the framework's
/// bookkeeping.
///
/// Returns one warning line per DocType that has documents in a state the workflow no longer
/// defines (a renamed or removed state), for example "DocType `order`: 3 documents are in a
/// workflow state that no longer exists; no transition applies to them". Nothing is done for
/// those documents.
pub(crate) async fn backfill_workflow_states(
    client: &mut Client,
    registry: &Registry,
) -> Result<Vec<String>, DbError> {
    let tx = client.transaction().await?;
    let mut warnings = Vec::new();
    for doctype in registry.iter().filter(|d| !d.is_child) {
        let Some(workflow) = &doctype.workflow else {
            continue;
        };
        let table = quote_ident(&table_name(&doctype.name));
        let statuses: &[DocStatus] = if doctype.submittable {
            &[DocStatus::Draft, DocStatus::Submitted, DocStatus::Cancelled]
        } else {
            &[DocStatus::Draft]
        };
        for status in statuses {
            let Some(state) = workflow.first_state_with(*status) else {
                continue;
            };
            // `docstatus` is a constant of the enum, never a value from outside.
            let by_status = if doctype.submittable {
                format!(" AND \"docstatus\" = {}", status.as_i16())
            } else {
                String::new()
            };
            tx.execute(
                &format!(
                    "UPDATE {table} SET \"workflow_state\" = $1::text \
                     WHERE \"workflow_state\" IS NULL{by_status}"
                ),
                &[&state.name],
            )
            .await?;
        }
        let names: Vec<&str> = workflow.states.iter().map(|s| s.name.as_str()).collect();
        let unknown: i64 = tx
            .query_one(
                &format!(
                    "SELECT count(*) FROM {table} WHERE \"workflow_state\" IS NOT NULL \
                     AND NOT (\"workflow_state\" = ANY($1::text[]))"
                ),
                &[&names],
            )
            .await?
            .try_get(0)?;
        if unknown > 0 {
            let documents = if unknown == 1 {
                "1 document is".to_owned()
            } else {
                format!("{unknown} documents are")
            };
            warnings.push(format!(
                "DocType `{}`: {documents} in a workflow state that no longer exists; no transition applies to them",
                doctype.name
            ));
        }
    }
    tx.commit().await?;
    Ok(warnings)
}
