//! Background jobs for apps: [`JobHandler`], [`JobContext`], [`JobError`] and [`JobOptions`]
//! (ADR 0013).
//!
//! An app registers a handler with [`App::job`](crate::App::job); a hook enqueues it with
//! [`HookContext::enqueue`](crate::HookContext::enqueue) (the job is written in the same
//! transaction as the document change), another job with [`JobContext::enqueue`], or an operator
//! with `forgebase job enqueue`. `forgebase worker` runs it.
//!
//! # Delivery
//!
//! - **At least once.** A job runs in **one database transaction together with its completion**:
//!   what the handler writes through [`JobContext::documents`] or [`JobContext::enqueue`]
//!   commits with the completion, or is rolled back and the job is tried again. Effects outside
//!   the database (an email, an HTTP call) are not covered: a job that crashes after sending may
//!   send again, so make such effects safe to repeat.
//! - **Retries.** A handler error is retried, up to [`JobOptions::max_attempts`] attempts (5 by
//!   default), after a delay that starts at 30 seconds and doubles up to 1 hour; then the job is
//!   `dead` until an operator runs `forgebase job retry`. [`JobError::permanent`] skips the
//!   retries. A handler that runs longer than its timeout (5 minutes by default) fails the same
//!   way, and a handler that panics is a retryable failure (the worker survives).
//! - **Options live in the binary.** They are not stored on the job row: the worker uses the
//!   definition in its own binary when it runs and retries a job, so a change takes effect for
//!   jobs that are already queued.
//! - **A worker claims only the jobs it knows**, by name. A job of an app that this binary does
//!   not have stays `queued` for a binary that has it.
//!
//! # Example
//!
//! ```ignore
//! use forgebase::{App, JobContext, JobError, JobFuture, JobHandler};
//!
//! struct Welcome;
//!
//! impl JobHandler for Welcome {
//!     fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
//!         Box::pin(async move {
//!             let customer = ctx.payload()["customer"].as_str().unwrap_or_default().to_owned();
//!             let found = ctx.documents().get("customer", &customer).await?;
//!             if found.is_none() {
//!                 return Err(JobError::permanent("the customer is gone"));
//!             }
//!             Ok(())
//!         })
//!     }
//! }
//!
//! pub fn app() -> App {
//!     App::new("crm").job("welcome", Welcome)
//! }
//! ```

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use forgebase_core::meta::Registry;
use forgebase_core::permission::Caller;
use forgebase_db::{DbError, Transaction, jobs};

use crate::documents::Documents;

/// The future a [`JobHandler`] returns. It must be `Send`: the worker runs it on a task.
pub type JobFuture<'a> = Pin<Box<dyn Future<Output = Result<(), JobError>> + Send + 'a>>;

/// What a background job does. Register it with [`App::job`](crate::App::job).
///
/// There is no async-trait crate in Forgebase, so a handler returns a boxed future:
///
/// ```ignore
/// impl JobHandler for Welcome {
///     fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a> {
///         Box::pin(async move { /* use ctx */ Ok(()) })
///     }
/// }
/// ```
///
/// The handler may run more than once for one job (at-least-once delivery, see the
/// [module documentation](self)). It runs inside a database transaction; if it returns an error
/// everything it did through `ctx` is rolled back.
pub trait JobHandler: Send + Sync {
    /// Runs the job. `Ok(())` completes it; an error is retried or, for
    /// [`JobError::permanent`], ends it.
    fn run<'a>(&'a self, ctx: &'a mut JobContext<'_>) -> JobFuture<'a>;
}

/// Why a job failed. [`JobError::new`] is retried (until the attempts are used up);
/// [`JobError::permanent`] goes straight to `dead`.
///
/// Errors of [`Documents`] become a `JobError` like this: a database or connection error is
/// **retryable** (the same call may well work later); a validation error, a permission error, a
/// hook that rejected, a document that was not found, a version conflict, a unique violation and
/// every other refusal is **permanent**, because it would fail again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobError {
    message: String,
    permanent: bool,
}

impl JobError {
    /// A failure that is worth retrying.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            permanent: false,
        }
    }

    /// A failure that retrying cannot fix: the job is `dead` after this attempt.
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            permanent: true,
        }
    }

    /// Whether the job is not retried.
    pub fn is_permanent(&self) -> bool {
        self.permanent
    }

    /// The message, stored as the job's `last_error` (cut to 2000 characters).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The error of a database function, as described in the type documentation.
    pub(crate) fn from_db(error: &DbError) -> Self {
        let message = crate::error::describe_db(error);
        match error {
            DbError::Postgres(_)
            | DbError::LockTimeout
            | DbError::Pool(_)
            | DbError::CreatePool(_)
            | DbError::ChangedWhileReading { .. } => Self::new(message),
            _ => Self::permanent(message),
        }
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JobError {}

/// How a job is run and retried. The defaults are 5 attempts, a 30 second delay that doubles up
/// to 1 hour, and a 5 minute timeout. Build with [`JobOptions::new`] and the methods below, and
/// give it to [`App::job_with`](crate::App::job_with); invalid values are a startup error.
///
/// The options are part of the binary, not of the job row (see the [module
/// documentation](self)).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct JobOptions {
    pub(crate) max_attempts: u32,
    pub(crate) timeout: Duration,
    backoff_base: Duration,
    backoff_cap: Duration,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            timeout: Duration::from_secs(5 * 60),
            backoff_base: Duration::from_secs(30),
            backoff_cap: Duration::from_secs(60 * 60),
        }
    }
}

impl JobOptions {
    /// The defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// The most attempts a job gets, the first one included. At least 1.
    pub fn max_attempts(mut self, attempts: u32) -> Self {
        self.max_attempts = attempts;
        self
    }

    /// How long a handler may run before the attempt is failed. More than zero.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The delay before the second attempt is `base`; each further one doubles it, up to `cap`.
    /// `base` is more than zero and `cap` is at least `base`.
    pub fn backoff(mut self, base: Duration, cap: Duration) -> Self {
        self.backoff_base = base;
        self.backoff_cap = cap;
        self
    }

    /// What is wrong with the options, one message per problem.
    pub(crate) fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.max_attempts < 1 {
            problems.push("max_attempts must be at least 1".to_owned());
        }
        if self.timeout.is_zero() {
            problems.push("timeout must be more than zero".to_owned());
        }
        if self.backoff_base.is_zero() {
            problems.push("the backoff base must be more than zero".to_owned());
        }
        if self.backoff_cap < self.backoff_base {
            problems.push("the backoff cap must be at least the backoff base".to_owned());
        }
        problems
    }

    /// The delay before the attempt after `attempt` (counted from 1):
    /// `min(base * 2^(attempt - 1), cap)`.
    pub(crate) fn backoff_after(&self, attempt: u32) -> Duration {
        let doublings = attempt.saturating_sub(1).min(31);
        self.backoff_base
            .checked_mul(1 << doublings)
            .map_or(self.backoff_cap, |delay| delay.min(self.backoff_cap))
    }
}

/// What a running job knows, and what it may do. The worker builds it; a handler gets it by
/// `&mut`.
///
/// It owns the transaction the job runs in. [`JobContext::documents`] and
/// [`JobContext::enqueue`] work inside it, so their effects commit together with the job's
/// completion, or not at all.
#[non_exhaustive]
pub struct JobContext<'c> {
    tx: Transaction<'c>,
    registry: &'c Registry,
    caller: Caller,
    name: String,
    id: String,
    attempt: u32,
    payload: serde_json::Value,
}

impl fmt::Debug for JobContext<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JobContext")
            .field("name", &self.name)
            .field("id", &self.id)
            .field("attempt", &self.attempt)
            .finish_non_exhaustive()
    }
}

impl<'c> JobContext<'c> {
    pub(crate) fn new(
        tx: Transaction<'c>,
        registry: &'c Registry,
        claimed: &forgebase_db::jobs::Claimed,
    ) -> Self {
        Self {
            tx,
            registry,
            caller: Caller::system(format!("job:{}", claimed.name)),
            name: claimed.name.clone(),
            id: claimed.id.to_string(),
            attempt: u32::try_from(claimed.attempt).unwrap_or(u32::MAX),
            payload: claimed.payload.clone(),
        }
    }

    /// The transaction, back to the worker after the handler returned.
    pub(crate) fn into_transaction(self) -> Transaction<'c> {
        self.tx
    }

    /// The full name of the job, `<app>.<job>`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The id of the job (a UUID, as text). It is the same for every attempt.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The number of this attempt, counted from 1.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The payload the job was enqueued with.
    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }

    /// Enqueues another job (`<app>.<job>`) in this job's transaction: it exists if and only if
    /// this job completes. An unknown name or a payload over 64 KiB of JSON is a permanent error
    /// (it would fail again). Never put secrets in a payload: it is stored in clear text.
    pub async fn enqueue(
        &mut self,
        name: &str,
        payload: serde_json::Value,
    ) -> Result<(), JobError> {
        // A savepoint: a refused enqueue leaves the job's transaction usable.
        let savepoint = self
            .tx
            .transaction()
            .await
            .map_err(|e| JobError::from_db(&e.into()))?;
        jobs::enqueue(
            &savepoint,
            self.registry,
            name,
            &payload,
            self.caller.name(),
        )
        .await
        .map_err(|e| JobError::from_db(&e))?;
        savepoint
            .commit()
            .await
            .map_err(|e| JobError::from_db(&e.into()))
    }

    /// Reads and writes documents as the trusted system context (no permission rules), with the
    /// actor `job:<app>.<job>`. See [`Documents`].
    pub fn documents(&mut self) -> Documents<'_> {
        Documents::new(&mut self.tx, self.registry, &self.caller)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let options = JobOptions::new();
        assert_eq!(options.max_attempts, 5);
        assert_eq!(options.timeout, Duration::from_secs(300));
        assert!(options.problems().is_empty());
    }

    #[test]
    fn backoff_doubles_from_the_base_and_is_capped() {
        let options = JobOptions::new();
        let seconds: Vec<u64> = (1..=9)
            .map(|a| options.backoff_after(a).as_secs())
            .collect();
        assert_eq!(seconds, [30, 60, 120, 240, 480, 960, 1920, 3600, 3600]);
        // No overflow, however many attempts.
        assert_eq!(options.backoff_after(u32::MAX), Duration::from_secs(3600));
        let big = JobOptions::new().backoff(Duration::from_secs(u64::MAX / 2), Duration::MAX);
        assert_eq!(big.backoff_after(40), Duration::MAX);
        // attempt 0 is not used, but must not panic.
        assert_eq!(options.backoff_after(0), Duration::from_secs(30));
    }

    #[test]
    fn invalid_options_are_listed() {
        let options = JobOptions::new()
            .max_attempts(0)
            .timeout(Duration::ZERO)
            .backoff(Duration::from_secs(10), Duration::from_secs(5));
        assert_eq!(options.problems().len(), 3);
        assert_eq!(
            JobOptions::new()
                .backoff(Duration::ZERO, Duration::ZERO)
                .problems()
                .len(),
            1
        );
    }

    #[test]
    fn database_errors_retry_and_refusals_do_not() {
        assert!(!JobError::from_db(&DbError::LockTimeout).is_permanent());
        for error in [
            DbError::NotFound {
                doctype: "a".into(),
                name: "b".into(),
            },
            DbError::Conflict {
                doctype: "a".into(),
                name: "b".into(),
                expected: 1,
                current: 2,
            },
            DbError::NoChanges,
            DbError::Validation(vec![]),
            DbError::Job("x".into()),
        ] {
            assert!(JobError::from_db(&error).is_permanent(), "{error}");
        }
        assert_eq!(JobError::permanent("x").message(), "x");
        assert_eq!(JobError::new("y").to_string(), "y");
    }
}
