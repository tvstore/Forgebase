# 0013 Jobs, scheduler and transactional outbox

- Status: Accepted (the ten owner decisions of the M6 plan, 2026-10-30); implementation details of M6.1 are separated below and are not owner decisions. Extends [ADR 0009](0009-lifecycle-pipeline-and-hooks.md) and updates the public surface of [ADR 0012](0012-app-model-and-public-api-boundary.md).
- Date: 2026-10-30

## Context

Business applications need work that runs outside a request: sending a notification after a document is saved, a nightly clean-up, a retry of something that failed. [ADR 0002](0002-modular-monolith-and-workspace-layout.md) already decided that jobs are a module in `forgebase-db` plus a `worker` command of the same binary, and [ADR 0009](0009-lifecycle-pipeline-and-hooks.md) said that effects after commit belong to the transactional outbox, not to a callback that can fail after the data is committed. The [architecture](../blueprint/architecture.md#8-background-jobs-scheduler-realtime) listed a table queue on `FOR UPDATE SKIP LOCKED` as Proposed. The owner approved the M6 plan on 2026-10-30. M6 is split into M6.1 (the queue, the worker, the outbox; this ADR's implementation), M6.2 (the scheduler and the built-in clean-up jobs) and M6.3 (example apps and the guide).

## Decision

The decisions below are the owner's (M6 plan, 2026-10-30).

1. **Hooks may enqueue jobs.** The jobs are written in the same transaction as the document change: the outbox is the job table. This extends [ADR 0009](0009-lifecycle-pipeline-and-hooks.md): hooks still have no database access.
2. **A job handler runs in one transaction together with its completion.** What the handler writes and the mark "done" commit together or not at all.
3. **Jobs get a documents API** as the system context (new public surface, see [ADR 0012](0012-app-model-and-public-api-boundary.md#public-surface-update-m61)).
4. **The scheduler has no leader.** Each schedule row is claimed atomically (M6.2).
5. **Schedules are every N minutes, daily or weekly, UTC only, no cron** (M6.2).
6. **Delivery defaults:** at-least-once; 5 attempts; a backoff that starts at 30 seconds, doubles and is capped at 1 hour; a `dead` state; a 5 minute timeout.
7. **A worker claims only the job names it knows.**
8. **Built-in clean-up jobs:** finished jobs after 7 days, login failures, expired sessions (M6.2).
9. **Operations are through the CLI only** (`worker`, `job list|show|retry|enqueue`). There is no HTTP or desk interface for jobs.
10. **The work is split into M6.1, M6.2 and M6.3.** M6.3 touches no file under `crates/`.

## Implementation details (M6.1)

These choices were made while implementing M6.1 and are reviewed with that change; they are not owner decisions.

**Where things live.** `forgebase-db::jobs` is the queue storage and its SQL (enqueue, claim, complete, fail, release, list, show, retry); it never runs a handler. `forgebase-core` has the collector for hook enqueues (`EnqueuedJobs`, `HookContext::enqueue`) and the set of known job names in the `Registry` (names only). The `forgebase` crate has the handler API (`JobHandler`, `JobContext`, `Documents`, `JobError`, `JobOptions`), `App::job`, the worker loop, the commands and the `testing` helpers.

**The table (migration 0005).** `fb_job`: `id` (UUID v7), `name` (`<app>.<job>`), `payload` (jsonb), `status` (`queued`, `running`, `done`, `dead`), `attempts`, `run_at`, `locked_until`, `locked_by`, `last_error`, `enqueued_by`, `created_at`, `finished_at`. Two partial indexes: `(run_at, id) WHERE status = 'queued'` for claiming the oldest due job and `(locked_until) WHERE status = 'running'` for finding expired leases; both stay small while finished jobs pile up. No index for `job list` (an operator command with a limit that reads by primary key; finished jobs are removed after a week, M6.2). Every time is PostgreSQL `now()`, never a worker's clock. Payloads are bound as text and cast to `jsonb` in SQL, so the database driver needs no extra feature.

**The outbox.** One `EnqueuedJobs` collector (a `Mutex` inside, so futures stay `Send`) per document operation is given to every hook call through `HookContext::with_jobs`. After the hooks and before the commit, `forgebase-db` inserts the collected jobs in the operation's transaction, for all six operations. A refused or rolled-back operation leaves no job; an unknown job name (not in the registry) or an oversize payload fails the whole operation with the new `DbError::Job`, which the HTTP API reports as `500 internal` with the detail in the server log: it is a mistake of the app, not of the caller. `enqueued_by` is the caller of the operation. `HookContext` stays `Copy`; its equality compares the collector by identity. A context made by `HookContext::new` has no collector, and `enqueue` on it fails with a message that says so.

**Payload.** At most 64 KiB of serialized JSON and no NUL character (`jsonb` cannot store one), checked on every way of enqueueing. Payloads are stored in clear text: never put a secret in one. `forgebase::serde_json` is re-exported so apps and the framework use one version.

**The documents API.** `JobContext::documents()` runs as `Caller::System` with the actor `job:<app>.<job>`, with the same document functions as the CLI and HTTP API on the job's transaction. Each call is a savepoint, so a refused call leaves the job's transaction usable; the hooks of those calls run as usual and the jobs they enqueue are written in the same transaction. A database or connection error becomes a retryable `JobError`; a validation, permission, hook, not-found or unique error, a version conflict and every other refusal is permanent, because it would fail again. `Documents` has one lifetime parameter: the transaction type is hidden behind a private trait object, so the public type does not carry the transaction's lifetime.

**Claiming.** One statement (`UPDATE ... WHERE id = (SELECT ... FOR UPDATE SKIP LOCKED)`), committed at once. A job is eligible if its name is known to the worker and it is queued and due, or running with an expired lease. Expired leases are looked for first, in a statement of their own: running jobs are few, and one statement with `OR` could not use the ordered index of the queue. The claim counts the attempt, records the worker and sets the lease to the job's timeout plus a margin of 30 seconds.

**Running.** The handler runs in a spawned task on its own connection and transaction under `tokio::time::timeout`. On `Ok`, the same transaction runs the guarded completion (`WHERE id AND status = 'running' AND locked_by = me AND attempts = this attempt`); if it changes no row the lease was lost, the transaction is rolled back and the handler's effects are gone with it. An error, a timeout or a panic (`JoinError`) rolls back and is recorded in a new short transaction with the same guard: a retry after `min(base * 2^(attempt - 1), cap)`, or `dead` for a permanent error or when the attempts are used up. A timed-out or aborted attempt **closes its connection** instead of returning it to the pool, because a query may still be running. A job that was claimed after a crashed worker had used its last attempt is marked `dead` without running again.

**Options live in the binary**, not on the job row (`JobOptions`: attempts, timeout, backoff). The worker uses its own definition when it runs and retries a job. Invalid options, an invalid or duplicate job name are startup errors.

**Shutdown.** The first SIGTERM or Ctrl-C stops claiming and lets running jobs finish for 30 seconds; a job still running then is aborted (rolled back) and **released** (`queued`, the attempt not counted, due now). A second signal exits at once; the lease recovers what was running. Exit code 0 on a clean stop. The pool has `concurrency + 2` connections: one per running job, and a spare for claims and for recording failures.

**Delivery guarantees.** At-least-once. Database effects of a handler are exactly-once in effect (they commit with the completion). Effects outside the database are not covered: a job that crashes after sending an email may send it again.

## Alternatives considered

- **A leader-elected scheduler** (the architecture note of earlier drafts). A leader needs a dedicated connection, failover and a heartbeat; claiming each schedule row atomically needs none of them and fails safe (M6.2).
- **A separate queue (Redis, a message broker).** Rejected: a second system to run, no transactional outbox with the document change, against the modular-monolith principle.
- **Completion in a separate transaction from the handler.** Simpler, but an effect and its "done" could diverge; the guarded completion in the handler's transaction removes the window for database effects.
- **LISTEN/NOTIFY to wake workers.** Not needed: polling once per second is simple and enough; it can be added later without changing the table.
- **Named queues, priorities, a job UI, jobs running as a user, exactly-once external effects.** Not needed by an approved requirement.
- **A cron expression parser** (M6.2): rejected by decision 5 to avoid a dependency and a large surface.

## Consequences

- A job may run more than once; handlers must be safe to repeat for effects outside the database.
- Jobs and their hooks are part of the public API of apps: [ADR 0012](0012-app-model-and-public-api-boundary.md#public-surface-update-m61) lists the new surface.
- The `fb_job` payload and `last_error` are readable by anyone with database access and by the CLI; see the [security model](../blueprint/security-model.md).
- The scheduler and the clean-up jobs (decisions 4, 5 and 8) are implemented in M6.2 and may add implementation details here.
