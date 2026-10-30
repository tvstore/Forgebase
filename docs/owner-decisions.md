# Owner decisions and thinking

This file records the project owner's decisions, comments and way of thinking. Every agent and contributor
should read it **before planning**. It helps shape options and recommendations; it never replaces the owner's
approval for owner-level decisions ([AGENTS.md §8 and §10.4](../AGENTS.md#8-final-rule)). It does not replace the detailed records: each entry links to the ADR or blueprint section that
holds the details. If an entry and a linked Accepted record disagree, the linked record wins, and the
mismatch is reported to the owner.

**How to keep it current.** Add every new owner decision or comment here, in the same pull request that
records it elsewhere. Rules for entries:

- quote the owner's words (Indonesian) where they matter;
- give the date;
- link to the detailed record;
- never rewrite an old entry. When the owner changes a decision, add a new entry that names the old one as
  superseded, and mark the old one "(superseded)".

The information session keeps this file. A working session adds decisions the owner gives it directly and
reports them to the information session ([AGENTS.md, section 10](../AGENTS.md#10-operating-model)).

## 1. How the owner thinks

Distilled from the decisions below. Use these to choose within an approved plan and to shape recommendations, and say
that you did. An owner-level decision ([AGENTS.md §10.4](../AGENTS.md#104-deviations-pdca)) still goes to the owner.

1. **Longevity over speed.** A framework that the owner, other people and other agents can maintain for
   years. The owner said, when approving M5: *"pikirkan matang-matang ... framework akan digunakan jangka
   panjang, harus mudah di maintenance olehmu, dan jika agents lain maintenance"*. So:
   - prefer the simplest design that will still hold;
   - keep a small public API;
   - write explicit code, with no magic;
   - write down every decision.
2. **A base that others extend, like Frappe and ERPNext.** *"open to extension dengan mudah, jadi ini
   sebagai base nya, jadi nanti bisa ditambahkan apps lain, persis pola frappe bisa menambahkan erpnext"*.
   So:
   - apps are separate crates stacked on the framework and on each other;
   - an app never needs a change in `crates/`;
   - example apps prove this.
3. **Frappe is a behavioural reference, never a code source.** Intentional differences are fine when they
   are simpler, safer or easier to maintain.
4. **Trusts well-argued recommendations and decides fast.** The owner usually answers "setuju semua" when
   each option comes with a clear recommendation and reason. *"untuk keputusan saya mengikuti plan anda,
   klo menurut anda sesuai lanjutkan"*. So present options with a recommendation first. That trust is not
   permission to skip the approval gates: plans and owner-level decisions still need an explicit "yes".
5. **Quality gates are not negotiable.**
   - Independent review is required.
   - CI must be green.
   - Dependencies need licence checks.
   - Owner-level choices (new requirements, architecture, significant dependencies, Accepted ADRs, starting
     a milestone) come back to the owner.
6. **Wants to see real results.** The owner asked how to see Forgebase in the browser, and then asked for UI
   work. Show working software (screenshots, a runnable quick start), not only reports.
7. **The UI must suit business users.** It must be simple, clean, elegant, corporate and user friendly:
   *"UI nya simple and clean, lalu UX yg user friendly"*; *"clean, simpel, elegan dan cocok untuk
   corporate"*. From UX.3 on, navy #1B2A5C (the logo background) is the brand colour of the UI and yellow stays only in the logo
   (owner decision 2026-10-30, below; recorded in [ADR 0017](adr/0017-desk-styling-tailwind.md)).
8. **Communication: results only.** Use Indonesian with the owner and English in code and docs. In the
   information session, report only finished results (merged pull requests, milestones Done) and decisions
   that are needed. Never send "waiting" or "no new results" messages. The history must read cleanly.
   Push notifications go to Android for results.
9. **Changes decisions openly.** A later decision supersedes an earlier one explicitly. An example is
   deleting `dead` jobs after 7 days, which superseded keeping them.

## 2. Standing instructions

| Date | Instruction | Recorded in |
|---|---|---|
| 2026-10-28 | Four-stage workflow (Plan, Doing, Review, Confirmation) with approval gates. Agent separation. Git safety: no direct changes to `main`, no merge or release without approval, no force push, no deleting without permission. Do not claim the project is open source while the repository is private. Do not copy Frappe code. | [AGENTS.md](../AGENTS.md) §3–5 |
| 2026-10-28 | Model Selection Policy: Opus plans and reviews, Sonnet implements, Sonnet or Haiku confirms, Fable is for escalation. | [AGENTS.md §9](../AGENTS.md#9-model-selection-policy), [ADR 0008](adr/0008-model-selection-policy.md) |
| 2026-10-28/29 | Operating model: the information session holds plans and results, and a working session executes with standing merge authorization. Deviations follow PDCA. *"conversation ini hanya untuk information, hasil plan dan hasil task ... untuk pengerjaan, merge, dll PR, pindahkan di session working, dan allow otomatis, dan otomasi merge ... jika tidak sesuai plan, selalu PDCA"* | [AGENTS.md §10](../AGENTS.md#10-operating-model) |
| 2026-10-29 | Results-only reporting in the information session. Android push notifications for results. *"hanya hasil yang kamu update jgn ada informasi menunggu kerjaan"*; *"jangan muncul kan tidak ada hasil baru"* | this file, §1.8 |
| 2026-10-30 | Record every owner decision and comment here, tidied up, so that other agents can share the owner's thinking. *"setiap putusan saya atau yang saya berikan informasi simpan ke memory ... agar nanti bisa share ke agent lain"* | this file |

## 3. Decision log

Most plan approvals were "setuju semua" (accepting every recommendation). Each row lists the decision and
where its details live.

### Blueprint (2026-10-28)

| Decision | Recorded in |
|---|---|
| Approve the research and blueprint plan. Network access to the documentation sites was allowed. Frappe claims stay labelled *Source-indexed* or *Unverified* until they are read directly. | [feature matrix](blueprint/feature-matrix.md) |
| Licence: keep **MIT**, intended to be open source. | [ADR 0007](adr/0007-license-mit.md), [LICENSE](../LICENSE) |
| Documents in English; communication with the owner in Indonesian. | [AGENTS.md §7](../AGENTS.md#7-communication-style) |
| Research and decisions by Opus; writing documents by Sonnet. | [AGENTS.md §9](../AGENTS.md#9-model-selection-policy) |
| Fix the optional review findings; accept ADRs 0002–0006 (modular monolith, PostgreSQL only, database access layer, and the others); re-verify Frappe later; open a pull request and start M0. | [ADR index](adr/README.md) |

### M0 Foundations (2026-10-28)

| Decision | Recorded in |
|---|---|
| A new branch and pull request per piece of work (option a). | information session (not recorded elsewhere); see [AGENTS.md §5](../AGENTS.md#5-git-and-repository-safety) |
| MSRV = pinned toolchain minus 2 minor versions (1.97). | [maintenance policy](blueprint/maintenance-policy.md) |
| Fix the M0 minor findings; add CONTRIBUTING and CHANGELOG to AGENTS.md §6; Frappe verification later. | [AGENTS.md §6](../AGENTS.md#6-documentation-structure) |

### M1 Metadata and storage (2026-10-28, "setuju semua")

| Decision | Recorded in |
|---|---|
| Three pull requests: M1.1, M1.2 and M1.3. | [milestones](blueprint/milestones.md) |
| DocType files in **TOML**. | [architecture §3](blueprint/architecture.md#3-metadata-doctype-model-proposed) |
| `time` for dates and times. `rust_decimal` for decimals. | [dependency evaluation](blueprint/dependency-evaluation.md) |
| PostgreSQL **16** is the minimum. | [maintenance policy](blueprint/maintenance-policy.md), [ADR 0003](adr/0003-postgresql-only.md) |
| Integration tests fail clearly without a database and are never skipped. | [CONTRIBUTING](../CONTRIBUTING.md) |
| `clap` (derive) for the command line. | [dependency evaluation](blueprint/dependency-evaluation.md) |
| UUID v7 primary keys, generated in the application. | [architecture §3](blueprint/architecture.md#3-metadata-doctype-model-proposed) |
| tokio-postgres and deadpool were confirmed by the spike. | [ADR 0004](adr/0004-database-access-layer.md) |

### M2 Relationships and lifecycle (2026-10-28, "setuju semua")

| Decision | Recorded in |
|---|---|
| A Link is a foreign key to `id`. | [architecture §4](blueprint/architecture.md) |
| Hooks are synchronous and have no database access. | [ADR 0009](adr/0009-lifecycle-pipeline-and-hooks.md) |
| A save replaces all child rows. | [architecture §4](blueprint/architecture.md) |
| An amended document is named `<name>-<n>`. | [architecture §6](blueprint/architecture.md) |
| Naming series are gapless and counted in the same transaction. DocTypes that share a prefix share the counter (option a, 2026-10-29: *"untuk keputusan saya mengikuti plan anda"*). | [architecture §6](blueprint/architecture.md) |
| Optimistic concurrency with `version`. | [architecture §6](blueprint/architecture.md) |

### M3 Auth, permissions, REST API (2026-10-29, "keputusan 1-12 setuju semua")

| Decision | Recorded in |
|---|---|
| Users and roles; Argon2id passwords; opaque session cookies; API tokens; lockout. | [ADR 0010](adr/0010-authentication-and-sessions.md) |
| Permissions in the DocType file, deny by default, `if_owner`. A document the caller cannot read behaves exactly like a missing one. | [ADR 0011](adr/0011-permission-model-phase-1.md) |
| CSRF header; `Host` validation; no CORS. | [security model](blueprint/security-model.md) |

### Project Goal and roadmap (2026-10-29)

| Decision | Recorded in |
|---|---|
| Project Goal: a full-stack framework comparable to Frappe, with 10 capabilities and 7 success criteria. | [vision](blueprint/vision.md) |
| New roadmap from M4 to M10. | [milestones](blueprint/milestones.md) |
| Logo: yellow "F" mark. Navy #1B2A5C banner. Badges: Rust-powered, Metadata-driven, Full-stack. No "open source" badge. | [docs/assets](assets/) |

### M4 Minimal Desk (2026-10-29, "setuju semua")

| Decision | Recorded in |
|---|---|
| React + TypeScript; **no UI component library**; own router and plain `fetch`. | [architecture §11](blueprint/architecture.md#11-desk-reports-workflow) |
| The built desk is served from a directory. | [architecture §11](blueprint/architecture.md#11-desk-reports-workflow) |
| Vite, Vitest and a Playwright smoke test. Dev-only MPL-2.0 packages are allowed. | [dependency evaluation §12](blueprint/dependency-evaluation.md#12-frontend) |

### M5 App model (2026-10-29)

| Decision | Recorded in |
|---|---|
| Design the app model for the long term (see §1.1 and §1.2). | [ADR 0012](adr/0012-app-model-and-public-api-boundary.md) |
| One crate, `forgebase`, is the public API. | [ADR 0012](adr/0012-app-model-and-public-api-boundary.md) |
| DocTypes are embedded in the binary. | [ADR 0012](adr/0012-app-model-and-public-api-boundary.md) |
| Several apps in an explicit order, with `depends_on`. | [ADR 0012](adr/0012-app-model-and-public-api-boundary.md) |
| App SQL migrations. | [ADR 0012](adr/0012-app-model-and-public-api-boundary.md) |
| `new-app` scaffold that includes an AGENTS.md. | [ADR 0012](adr/0012-app-model-and-public-api-boundary.md) |
| `forgebase::testing` helpers. | [ADR 0012](adr/0012-app-model-and-public-api-boundary.md) |
| Example apps that touch no `crates/`. | [ADR 0012](adr/0012-app-model-and-public-api-boundary.md) |

### M6 Jobs and scheduler (2026-10-30, "setuju semua")

| Decision | Recorded in |
|---|---|
| PostgreSQL queue with no Redis; the outbox is filled from hooks. | [ADR 0013](adr/0013-jobs-scheduler-and-transactional-outbox.md) |
| A job and its completion commit in one transaction. | [ADR 0013](adr/0013-jobs-scheduler-and-transactional-outbox.md) |
| Jobs run as the system context. | [ADR 0013](adr/0013-jobs-scheduler-and-transactional-outbox.md) |
| A scheduler without a leader; UTC schedules only, no cron. | [ADR 0013](adr/0013-jobs-scheduler-and-transactional-outbox.md) |
| Retry defaults. | [ADR 0013](adr/0013-jobs-scheduler-and-transactional-outbox.md) |
| Built-in clean-up jobs. | [ADR 0013](adr/0013-jobs-scheduler-and-transactional-outbox.md) |
| CLI-only operations. | [ADR 0013](adr/0013-jobs-scheduler-and-transactional-outbox.md) |
| ~~P-1: keep `dead` jobs until an operator retries them (the first reading of decision 8)~~ (superseded, row below). | [ADR 0013](adr/0013-jobs-scheduler-and-transactional-outbox.md#implementation-details-m62) |
| **Delete `dead` jobs 7 days after they finish**, like `done` jobs: *"hapus juga job dead setelah 7 hari"* (given in the working session; supersedes P-1). | [ADR 0013](adr/0013-jobs-scheduler-and-transactional-outbox.md#implementation-details-m62) |
| P-2: ADR 0010 gets a status note about the clean-up job (delivered with UX.1). | [ADR 0010](adr/0010-authentication-and-sessions.md) |

### Milestone UX (2026-10-30)

| Decision | Recorded in |
|---|---|
| UX-1: UX before M7: *"UI nya simple and clean, lalu UX yg user friendly"*. | [milestones](blueprint/milestones.md) |
| UX-2: a field without `label` gets the humanized name (`order_date` gives "Order Date"). The DocType `label` stays required (M1 decision). | [architecture §3](blueprint/architecture.md#3-metadata-doctype-model-proposed) |
| UX-2: `read_only`, `title_field` and `list_fields` in metadata. | [architecture §3](blueprint/architecture.md#3-metadata-doctype-model-proposed) |
| UX-3: sidebar grouped by app, plus breadcrumbs. | [architecture §11](blueprint/architecture.md#11-desk-reports-workflow) |
| UX-4: still no UI component library. | [architecture §11](blueprint/architecture.md#11-desk-reports-workflow) |
| ~~UX-5: the Forgebase yellow as the accent~~ (superseded by T2, the UX.3 navy decision of 2026-10-30, below). | [architecture §11](blueprint/architecture.md#11-desk-reports-workflow) (UX-5 marked superseded there by UX.3) |

### M7 Site management (2026-10-30, "setuju semua")

| Decision | Recorded in |
|---|---|
| A database per site. | [ADR 0014](adr/0014-multi-site-database-per-site.md) |
| `forgebase.toml`, with database URLs read from environment variables. | [ADR 0014](adr/0014-multi-site-database-per-site.md) |
| Site chosen by `Host`. | [ADR 0014](adr/0014-multi-site-database-per-site.md) |
| Apps per site. | [ADR 0014](adr/0014-multi-site-database-per-site.md) |
| One worker per site. | [ADR 0014](adr/0014-multi-site-database-per-site.md) |
| Backup and restore with `pg_dump` and `pg_restore`. | [ADR 0014](adr/0014-multi-site-database-per-site.md) |

### UX.3 Tailwind and corporate palette, and M8 Workflow and reports (2026-10-30, "oke setuju semua dengan plan mu"; information session)

These decisions were approved in the information session. Their ADRs and milestone entries are written by the UX.3 and
M8 pull requests. T1 to T3 are recorded in [ADR 0017](adr/0017-desk-styling-tailwind.md) (UX.3,
[#31](https://github.com/agilma/Forgebase/pull/31)); T4 (UX.3 before M8.1) is recorded in ADR 0017 and the
[milestones](blueprint/milestones.md). The workflow row is recorded in [ADR 0015](adr/0015-workflow-states-and-transitions.md)
(M8.1, the pull request of branch `claude/m8-1-workflow`). The other M8 rows are still planned: until their pull
requests land, this table is their only record.

| Decision | Recorded in |
|---|---|
| T1: **Tailwind CSS v4** as a build-time styling tool. Still no component kits. *"UI pakai tailwind agar tampilan ui clean, simpel, elegan dan cocok untuk corporate"* | [ADR 0017](adr/0017-desk-styling-tailwind.md) |
| T2: **Navy #1B2A5C** (the logo background) is the primary UI colour (supersedes UX-5). Yellow is used only in the logo: *"jangan warna kuning pakai warna background dari logo"*. | [ADR 0017](adr/0017-desk-styling-tailwind.md) |
| T3: system font stack. | [ADR 0017](adr/0017-desk-styling-tailwind.md) |
| Workflow lives in the DocType file. A transition needs both the role and the underlying right. A DocType with a workflow changes state only through transitions. `before_transition` hook; transition log. | [ADR 0015](adr/0015-workflow-states-and-transitions.md) |
| Saved views: private per user; shared views only by Administrator. Group-by summary. | planned: the M8.3 pull request |
| Query reports: app-defined SQL, limited by role, run in a read-only transaction with limits. No CSV export and no charts in M8. | planned: the M8.4 ADR |
