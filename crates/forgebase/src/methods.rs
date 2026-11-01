//! App method endpoints (M9.1, ADR 0019): [`Method`], [`MethodHandler`], [`MethodContext`] and
//! [`MethodError`], and the dispatcher that runs a method for `forgebase-http`.
//!
//! An app registers a method with [`App::method`](crate::App::method); a client calls it with
//! `POST /api/v1/method/<app>.<method>`. A method runs **as the caller**, in **one database
//! transaction**.

use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use forgebase_core::meta::Registry;
use forgebase_core::method::{MethodAccess, MethodDef};
use forgebase_core::permission::Caller;
use forgebase_db::jobs::JobConnection;
use forgebase_db::{DbError, Pool, Transaction};
use forgebase_http::{BoxFuture, MethodDispatch, MethodFailure};

use crate::documents::{DocError, Documents};

/// The future a [`MethodHandler`] returns. It must be `Send`: the method runs on its own task.
pub type MethodFuture<'a> =
    Pin<Box<dyn Future<Output = Result<serde_json::Value, MethodError>> + Send + 'a>>;

/// What an app method does. Register it with [`App::method`](crate::App::method), wrapped in a
/// [`Method`].
///
/// There is no async-trait crate in Forgebase, so a handler returns a boxed future:
///
/// ```ignore
/// use forgebase::{App, Method, MethodContext, MethodError, MethodFuture, MethodHandler};
/// use forgebase::serde_json::json;
///
/// struct OrderCount;
///
/// impl MethodHandler for OrderCount {
///     fn call<'a>(&'a self, ctx: &'a mut MethodContext<'_>) -> MethodFuture<'a> {
///         Box::pin(async move {
///             let Some(customer) = ctx.body()["customer"].as_str().map(str::to_owned) else {
///                 return Err(MethodError::bad_request("Say which customer."));
///             };
///             let query = forgebase::ListQuery::new().filter(
///                 "customer",
///                 forgebase::FilterOp::Eq,
///                 forgebase::Value::Text(customer),
///             );
///             let orders = ctx.documents().list("sales_order", &query).await?;
///             Ok(json!({ "orders": orders.len() }))
///         })
///     }
/// }
///
/// pub fn app() -> App {
///     App::new("sales").method("order_count", Method::new(OrderCount).roles(&["Sales User"]))
/// }
/// ```
///
/// # Rules (ADR 0019)
///
/// - **As the caller.** [`MethodContext::documents`] acts as the authenticated user, so the
///   permission rules of the DocTypes (`if_owner`, read rules, read-only fields, workflow roles)
///   are the only decision. A method cannot act as the system context.
/// - **One transaction.** Everything the handler does through the context commits when it
///   returns `Ok`, and is rolled back when it returns an error or panics (the server survives,
///   the client gets a generic 500). A document write that is refused leaves the transaction
///   usable, so the handler may handle the error and go on.
/// - **Errors.** [`MethodError::bad_request`] and [`MethodError::unprocessable`] carry a message
///   written for users. A [`DocError`] (`?` on a [`Documents`] call) keeps the status and code
///   of the document API. Anything else is a generic 500.
/// - **No time limit** is applied to the handler in this version; keep it short.
pub trait MethodHandler: Send + Sync {
    /// Runs the method. `Ok(json)` commits and is sent as `{"data": json}`.
    fn call<'a>(&'a self, ctx: &'a mut MethodContext<'_>) -> MethodFuture<'a>;
}

/// A method and who may call it, for [`App::method`](crate::App::method). Exactly one of
/// [`Method::roles`] and [`Method::any_user`] is required; **there are no guest methods**.
///
/// A method with neither, with an empty role list, or with both is a startup error, like the
/// other mistakes of an app.
#[derive(Clone)]
pub struct Method {
    pub(crate) handler: Arc<dyn MethodHandler>,
    /// `None` until `.roles(..)` is called; `Some(empty)` is `.roles(&[])`.
    pub(crate) roles: Option<Vec<String>>,
    pub(crate) any_user: bool,
}

impl fmt::Debug for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Method")
            .field("roles", &self.roles)
            .field("any_user", &self.any_user)
            .finish_non_exhaustive()
    }
}

impl Method {
    /// A method run by `handler`. Say who may call it with [`Method::roles`] or
    /// [`Method::any_user`].
    pub fn new(handler: impl MethodHandler + 'static) -> Self {
        Self {
            handler: Arc::new(handler),
            roles: None,
            any_user: false,
        }
    }

    /// Adds the roles that may call the method. The Administrator role always may. Calling it
    /// again adds more roles; an empty list is a startup error.
    pub fn roles(mut self, roles: &[&str]) -> Self {
        self.roles
            .get_or_insert_with(Vec::new)
            .extend(roles.iter().map(|r| (*r).to_owned()));
        self
    }

    /// Any authenticated user may call the method, whatever their roles. Use it deliberately:
    /// the documents the method touches are still checked for the caller.
    pub fn any_user(mut self) -> Self {
        self.any_user = true;
        self
    }

    /// The definition for `app` and `name` and everything wrong with it, one message per
    /// problem: how access was given, then [`MethodDef::problems`]. A definition with problems
    /// is not registered.
    pub(crate) fn definition(&self, app: &str, name: &str) -> (MethodDef, Vec<String>) {
        let mut problems = Vec::new();
        let access = match (&self.roles, self.any_user) {
            (Some(roles), true) => {
                problems
                    .push("both .roles(..) and .any_user() are given; use exactly one".to_owned());
                MethodAccess::Roles(roles.clone())
            }
            (None, false) => {
                problems.push(
                    "no access given: name the roles that may call it with .roles(&[..]), or use .any_user(); there are no guest methods"
                        .to_owned(),
                );
                MethodAccess::AnyUser
            }
            (Some(roles), false) => MethodAccess::Roles(roles.clone()),
            (None, true) => MethodAccess::AnyUser,
        };
        let def = MethodDef {
            app: app.to_owned(),
            name: name.to_owned(),
            access,
        };
        problems.extend(def.problems());
        (def, problems)
    }
}

/// The user who called a method: the name and the roles, as the database has them now. Read-only.
#[derive(Debug, Clone, Copy)]
pub struct MethodCaller<'a> {
    username: &'a str,
    roles: &'a [String],
}

impl<'a> MethodCaller<'a> {
    /// The username (recorded as `owner` and `modified_by` of what the method writes).
    pub fn username(&self) -> &'a str {
        self.username
    }

    /// The user's roles.
    pub fn roles(&self) -> &'a [String] {
        self.roles
    }

    /// Whether the user has `role`.
    pub fn has_role(&self, role: &str) -> bool {
        self.roles.iter().any(|r| r == role)
    }
}

/// What a running method knows and may do. The framework builds it; a handler gets it by `&mut`.
///
/// It owns the transaction the method runs in: [`MethodContext::documents`] and
/// [`MethodContext::enqueue`] work inside it, so their effects commit together, or not at all.
#[non_exhaustive]
pub struct MethodContext<'c> {
    tx: Transaction<'c>,
    registry: &'c Registry,
    caller: Caller,
    name: String,
    body: serde_json::Value,
}

impl fmt::Debug for MethodContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MethodContext")
            .field("name", &self.name)
            .field("caller", &self.caller.name())
            .finish_non_exhaustive()
    }
}

impl<'c> MethodContext<'c> {
    fn new(
        tx: Transaction<'c>,
        registry: &'c Registry,
        caller: Caller,
        name: String,
        body: serde_json::Value,
    ) -> Self {
        Self {
            tx,
            registry,
            caller,
            name,
            body,
        }
    }

    /// The transaction, back to the dispatcher after the handler returned.
    fn into_transaction(self) -> Transaction<'c> {
        self.tx
    }

    /// The full name of the method, `<app>.<method>`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The user who called the method.
    pub fn caller(&self) -> MethodCaller<'_> {
        match &self.caller {
            Caller::User {
                username, roles, ..
            } => MethodCaller { username, roles },
            // Never built from a request: the dispatcher refuses it. Kept total instead of
            // panicking.
            Caller::System { actor } => MethodCaller {
                username: actor,
                roles: &[],
            },
        }
    }

    /// The request body: a JSON object (`{}` when the request had none).
    pub fn body(&self) -> &serde_json::Value {
        &self.body
    }

    /// Reads and writes documents **as the caller**: the permission rules of the DocTypes decide
    /// every call. See [`Documents`].
    pub fn documents(&mut self) -> Documents<'_> {
        Documents::new(&mut self.tx, self.registry, &self.caller)
    }

    /// Enqueues a background job (`<app>.<job>`) in this method's transaction: it exists if and
    /// only if the method succeeds. The job runs later, in the worker, as the system context,
    /// like a job a hook enqueued. An unknown name or a payload over 64 KiB of JSON is an error
    /// of the app (a generic 500 for the client). Never put secrets in a payload: it is stored in
    /// clear text.
    pub async fn enqueue(
        &mut self,
        name: &str,
        payload: serde_json::Value,
    ) -> Result<(), DocError> {
        // A savepoint, like `JobContext::enqueue`: a refused enqueue leaves the transaction
        // usable.
        let savepoint = self
            .tx
            .transaction()
            .await
            .map_err(|e| DocError::new(e.into()))?;
        forgebase_db::jobs::enqueue(
            &savepoint,
            self.registry,
            name,
            &payload,
            self.caller.name(),
        )
        .await
        .map_err(DocError::new)?;
        savepoint
            .commit()
            .await
            .map_err(|e| DocError::new(e.into()))
    }
}

/// Why a method failed. [`MethodError::bad_request`] and [`MethodError::unprocessable`] are for
/// the app's own checks; a [`DocError`] converts with `?` and keeps the status and code the
/// document API gives the same error. **Every error rolls the transaction back.**
#[derive(Debug)]
pub struct MethodError {
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    Handler { status: u16, message: String },
    Doc(DocError),
}

impl MethodError {
    /// `400` with the code `method_error` and this message, which is sent to the client as it
    /// is: write it for users, and put nothing internal in it.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            kind: Kind::Handler {
                status: 400,
                message: message.into(),
            },
        }
    }

    /// `422` with the code `method_error` and this message, which is sent to the client as it
    /// is: write it for users, and put nothing internal in it.
    pub fn unprocessable(message: impl Into<String>) -> Self {
        Self {
            kind: Kind::Handler {
                status: 422,
                message: message.into(),
            },
        }
    }

    fn into_failure(self) -> MethodFailure {
        match self.kind {
            Kind::Handler { status, message } => MethodFailure::Handler {
                status,
                code: "method_error",
                message,
            },
            Kind::Doc(error) => MethodFailure::Db(error.into_db()),
        }
    }
}

impl From<DocError> for MethodError {
    fn from(error: DocError) -> Self {
        Self {
            kind: Kind::Doc(error),
        }
    }
}

impl fmt::Display for MethodError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.kind {
            Kind::Handler { message, .. } => f.write_str(message),
            Kind::Doc(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for MethodError {}

/// The handlers of all the apps of the binary, by full name. Which methods a site has is the
/// registry's business; this only runs the one it is asked for.
#[derive(Clone, Default)]
pub(crate) struct Methods {
    handlers: BTreeMap<String, Arc<dyn MethodHandler>>,
}

impl Methods {
    pub fn new(handlers: BTreeMap<String, Arc<dyn MethodHandler>>) -> Self {
        Self { handlers }
    }
}

impl MethodDispatch for Methods {
    fn call<'a>(
        &'a self,
        pool: &'a Pool,
        registry: &'a Arc<Registry>,
        name: &'a str,
        caller: Caller,
        body: serde_json::Value,
    ) -> BoxFuture<'a, Result<serde_json::Value, MethodFailure>> {
        Box::pin(async move {
            let Some(handler) = self.handlers.get(name).cloned() else {
                return Err(MethodFailure::Internal(format!(
                    "method {name}: the registry lists it but the binary has no handler"
                )));
            };
            if !matches!(caller, Caller::User { .. }) {
                return Err(MethodFailure::Internal(format!(
                    "method {name}: called with a system caller"
                )));
            }
            let (pool, registry, name) = (pool.clone(), Arc::clone(registry), name.to_owned());
            // Its own task: a panic in the handler ends the task, not the server, and the
            // transaction finishes (or is dropped with its connection) even if the client goes
            // away before the answer.
            let task = tokio::spawn(async move {
                run(&pool, &registry, handler.as_ref(), name, caller, body).await
            });
            match task.await {
                Ok(result) => result,
                Err(error) => Err(MethodFailure::Internal(describe_join_error(error))),
            }
        })
    }
}

fn describe_join_error(error: tokio::task::JoinError) -> String {
    if error.is_panic() {
        let payload = error.into_panic();
        let text = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned());
        match text {
            Some(text) => format!("the method panicked: {text}"),
            None => "the method panicked".to_owned(),
        }
    } else {
        "the method task was cancelled".to_owned()
    }
}

/// One call: the handler and the commit in one transaction.
async fn run(
    pool: &Pool,
    registry: &Registry,
    handler: &dyn MethodHandler,
    name: String,
    caller: Caller,
    body: serde_json::Value,
) -> Result<serde_json::Value, MethodFailure> {
    // If anything ends this function early or the task is aborted (a panic in the handler), the
    // guard closes the connection instead of returning it to the pool with a transaction open.
    let mut connection = JobConnection::get(pool).await.map_err(MethodFailure::Db)?;
    let tx = connection
        .client()
        .transaction()
        .await
        .map_err(|e| MethodFailure::Db(DbError::from(e)))?;
    let mut ctx = MethodContext::new(tx, registry, caller, name, body);
    let result = handler.call(&mut ctx).await;
    let tx = ctx.into_transaction();
    match result {
        Ok(value) => {
            tx.commit()
                .await
                .map_err(|e| MethodFailure::Db(DbError::from(e)))?;
            connection.finish();
            Ok(value)
        }
        Err(error) => {
            // A failed rollback leaves the connection to be closed by the guard.
            if tx.rollback().await.is_ok() {
                connection.finish();
            }
            Err(error.into_failure())
        }
    }
}
