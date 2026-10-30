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
(M8.1, [#32](https://github.com/agilma/Forgebase/pull/32)). The saved views and group-by summary row is recorded in
[architecture §11](blueprint/architecture.md#11-desk-reports-workflow) and [§7](blueprint/architecture.md#7-api-rest-resource-api-implemented-in-m33-metadata-and-session-routes-in-m41-the-rest-proposed)
(M8.3, [#35](https://github.com/agilma/Forgebase/pull/35)). The query reports row is recorded in
[ADR 0016](adr/0016-query-reports-read-only-execution.md) (M8.4, [#37](https://github.com/agilma/Forgebase/pull/37)). The other M8 rows are still planned:
until their pull requests land, this table is their only record.

| Decision | Recorded in |
|---|---|
| T1: **Tailwind CSS v4** as a build-time styling tool. Still no component kits. *"UI pakai tailwind agar tampilan ui clean, simpel, elegan dan cocok untuk corporate"* | [ADR 0017](adr/0017-desk-styling-tailwind.md) |
| T2: **Navy #1B2A5C** (the logo background) is the primary UI colour (supersedes UX-5). Yellow is used only in the logo: *"jangan warna kuning pakai warna background dari logo"*. | [ADR 0017](adr/0017-desk-styling-tailwind.md) |
| ~~T3: system font stack.~~ (superseded by U2, below: Inter is self-hosted; the system stack stays as the fallback.) | [ADR 0017](adr/0017-desk-styling-tailwind.md) |
| Workflow lives in the DocType file. A transition needs both the role and the underlying right. A DocType with a workflow changes state only through transitions. `before_transition` hook; transition log. | [ADR 0015](adr/0015-workflow-states-and-transitions.md) |
| Saved views: private per user; shared views only by Administrator. Group-by summary. | [architecture §11](blueprint/architecture.md#11-desk-reports-workflow), [§7](blueprint/architecture.md#7-api-rest-resource-api-implemented-in-m33-metadata-and-session-routes-in-m41-the-rest-proposed) (M8.3) |
| Query reports: app-defined SQL, limited by role, run in a read-only transaction with limits. No CSV export and no charts in M8. | [ADR 0016](adr/0016-query-reports-read-only-execution.md) |

### UX.4 desk polish (2026-10-30, "setuju semua rencana mu")

The owner asked for a smoother, more elegant desk: *"apa bisa ui dibuat lebih smoothy agar terkesan elegan high kualitas, seperti frappe? tapi tetap pakai warna dasar yang sama"*, and approved the plan (and the M9 plan, below) in the information session with *"setuju semua rencana mu"*. The base colours do not change (navy `#1B2A5C`, yellow only in the logo: T2 holds). Frappe is a visual and behavioural reference only: no Frappe code or asset is copied. The decisions are recorded in [ADR 0017](adr/0017-desk-styling-tailwind.md) (section "UX.4") and in [architecture §11](blueprint/architecture.md#11-desk-reports-workflow) and [§7](blueprint/architecture.md#7-api-rest-resource-api-implemented-in-m33-metadata-and-session-routes-in-m41-the-rest-proposed); the pull request is the one that implements them.

| Decision | Recorded in |
|---|---|
| U1: UX.4 runs between M8.1 and M8.2, so that every M8 desk screen is built in the polished style. | [ADR 0017](adr/0017-desk-styling-tailwind.md), [milestones](blueprint/milestones.md) |
| U2: **Inter, self-hosted**, from `@fontsource-variable/inter` (OFL-1.1, Latin subset); the OFL-1.1 licence is allowed **for fonts only**. Supersedes T3 (the system stack stays as the fallback). | [ADR 0017](adr/0017-desk-styling-tailwind.md), [dependency evaluation §12](blueprint/dependency-evaluation.md#12-frontend) |
| U3: a curated set of **Lucide SVG icons copied into the repository** (ISC, with the licence notice; not a dependency), plus an optional `icon` key on DocTypes (validated against one list). | [ADR 0017](adr/0017-desk-styling-tailwind.md), [architecture §3](blueprint/architecture.md#3-metadata-doctype-model-proposed) |
| U4: the look and the motion of the mockup: tokens, filled inputs, pill badges, 12 px cards, 150 to 200 ms CSS motion that `prefers-reduced-motion` turns off. | [ADR 0017](adr/0017-desk-styling-tailwind.md), [architecture §11](blueprint/architecture.md#11-desk-reports-workflow) |
| U5: a **command bar** on Ctrl/Cmd+K: navigation only, on the client, no new API, no storage. | [architecture §11](blueprint/architecture.md#11-desk-reports-workflow) |
| U6: form in cards plus a right-hand info panel (the workflow history joins it in M8.2). | [architecture §11](blueprint/architecture.md#11-desk-reports-workflow) |
| U7: **link titles**: a permission-aware addition to the list and read routes (`?link_titles=1`); a target the caller cannot read gives no title. | [architecture §7](blueprint/architecture.md#7-api-rest-resource-api-implemented-in-m33-metadata-and-session-routes-in-m41-the-rest-proposed), [security model](blueprint/security-model.md#the-http-layer-implemented-in-m33) |

Order of the work, as the information session set it: M8.1 (done), then UX.4, then M8.2 to M8.5, then M9.1 to M9.5.

**Licence notices with the build (2026-10-30, information session).** The owner approved the recommendation with *"setujui rekomendasi putusanmu"*: ship Inter's OFL-1.1 text and the Lucide ISC notice with the built desk (`dist/THIRD-PARTY-LICENSES.txt`), included in the M8.3 pull request, with a test that keeps the copy current. The same approval covers a cosmetic fix (a list's primary column header shows one label, the title field's label) and confirms that the native date input format stays as it is (no custom date picker).

| Decision | Recorded in |
|---|---|
| Ship `THIRD-PARTY-LICENSES.txt` (Inter OFL-1.1, Lucide) in `desk/dist/`; refreshed on a font or icon upgrade. | [ADR 0017](adr/0017-desk-styling-tailwind.md) (UX.4, "Licence notices"), [maintenance policy](blueprint/maintenance-policy.md), [dependency evaluation §12](blueprint/dependency-evaluation.md#12-frontend) |

### M9 Integration (2026-10-30, "setuju semua rencana mu"; information session)

The owner approved the M9 plan in the information session (the same *"setuju semua rencana mu"* as UX.4). **M9 starts after M8 is Done**; this section is the only record of its decisions until the M9 pull requests land (the plan itself is in the information session). They build on M3 (auth, API tokens, permissions), M5 (apps, the public API), M6 (jobs and the outbox), M7 (sites) and M8. The owner's thinking applies: a small explicit public API, deny by default, no runtime code, extension through apps only, and a significant dependency is an owner decision (decisions 6 and 9 are that approval). Not in M9: GraphQL, RPC by dotted path, guest endpoints, inbound webhook receivers, OAuth or SSO, a webhook UI in the desk, Excel, CSV import of child tables, sync between sites. M10 needs a new plan.

| Decision | Recorded in |
|---|---|
| 1: app method endpoints `<app>.<method>`, `POST /api/v1/method/...` with JSON in and out; roles are required or the method says `any_user()`; no guest methods. | planned: the M9.1 pull request (ADR 0018) |
| 2: methods run as the caller; the documents handle enforces the caller's permissions; one transaction per call. | planned: the M9.1 pull request (ADR 0018) |
| 3: webhooks are configured per site at runtime (a table and the `forgebase webhook` commands). | planned: the M9.2 pull request (ADR 0019) |
| 4: webhook delivery through the outbox, signed with HMAC-SHA256, at least once. | planned: the M9.2 pull request (ADR 0019) |
| 5: an SSRF guard (https only, no loopback, private, link-local or metadata addresses unless allowed per webhook, a timeout, no redirects, secrets never printed). | planned: the M9.2 pull request (ADR 0019) |
| 6: the HTTP client is `hyper-util` plus `hyper-rustls` (ring) and `rustls-native-certs`: **the owner approved these dependencies and their licences**. | planned: the M9.2 pull request (dependency evaluation) |
| 7: new rights `export` and `import` (ADR 0011 extended). | planned: the M9.3 pull request |
| 8: import is all-or-nothing, with a dry run, per-row errors, insert or an explicit upsert, CSV and JSON. | planned: the M9.3 pull request |
| 9: the `csv` crate reads CSV: **the owner approved this dependency**. | planned: the M9.3 pull request (dependency evaluation) |
| 10: fixtures are inserted if missing, updated only when the fixture's record changed, and never deleted (the answer to the open question 11 of the feature matrix). | planned: the M9.4 pull request (ADR 0012 surface update) |
| 11: M9 is split into M9.1 to M9.5, and M9.5 (the example apps) touches no file under `crates/`. | planned: [milestones](blueprint/milestones.md) |

### UX.5 corporate refinement (2026-10-30, "setuju D"; information session)

The owner looked at the desk after UX.4 and was happy with the UX but not with the look of the text fields: *"coba check dan plan kan ui yang cocok agar keliatan untuk UI untuk corporate saya melihat masih kurang sreg UI nya, kalau UX saya sudah suka ... kurang suka dengan text field nya"*. Shown the options, the owner added *"coba kamu check frappe 16 UI nya bagus, abu² text nya lebih dibuat lebih soft"* and chose variant D ("soft neutral") with *"setuju D"*. The information session recorded the approval; the working session delivers it as a small pull request right after M8.3 (#35, which already held its desk part when UX.5 was approved; UX.5 restyles those screens too) and before M8.4. The decision **refines UX-4/U4 field styling** (it does not supersede U4: the look, the motion and the rest of U4 stand), and **T2 is unchanged: navy `#1B2A5C` stays the only accent**, yellow only in the logo. Visual only: no UX, backend or dependency change. Frappe (the public `frappe-ui` package, MIT) was a reference for token values only; no code or asset is copied. The details, the token set, the contrast table and the deviation are in [ADR 0017](adr/0017-desk-styling-tailwind.md) (section "UX.5") and [architecture §11](blueprint/architecture.md#11-desk-reports-workflow).

| Decision | Recorded in |
|---|---|
| F1: **field style D**: borderless `#F3F3F3` fills (hover `#EDEDED`, white with a `#C7C7C7` border on focus, a visible keyboard ring, a pink error state), labels above the fields. The owner **accepted the WCAG 1.4.11 trade-off**: the boundary of a borderless filled field is about 1.1:1 (3:1 is asked); mitigated by labels, the fill, hover and focus states and a visible keyboard focus; text contrast stays AA. | [ADR 0017](adr/0017-desk-styling-tailwind.md) (UX.5), [architecture §11](blueprint/architecture.md#11-desk-reports-workflow) |
| F2 to F4 (as numbered in the information session's approved plan): the **neutral grey scale** that replaces the bluish slate (page `#F8F8F8`, flat cards with an `#EDEDED` border, softer text: body `#383838`, labels `#6B6B6B`), the **child table** as one bordered grid with plain, text-like cells (the active cell white with a `#C7C7C7` outline), and **buttons** (32 px, 8 px radius, secondary white with a `#E2E2E2` border, no shadow) with **neutral badges**. The plan file held by the working session does not number these items, so they are listed together here rather than guessing the numbering. | [ADR 0017](adr/0017-desk-styling-tailwind.md) (UX.5) |

**Numbering confirmed (2026-10-30, information session).** The information session gave the numbering of the approved items, which the row above lists together: **F1** field style D (soft neutral, in the Frappe 16 direction) with the documented WCAG 1.4.11 trade-off; **F2** the child table as a soft, spreadsheet-like grid; **F3** the neutral grey palette and softer text colours across the desk (navy unchanged); **F4** delivering it as a small UX.5 pull request before further desk work ([#36](https://github.com/agilma/Forgebase/pull/36)). The information session also accepted the two deviations of the implementation: `#6B6B6B` for table captions and placeholders (the plan's lighter greys fail AA for text) and a navy focus ring at 60 % opacity. Details: [ADR 0017](adr/0017-desk-styling-tailwind.md) (UX.5).

### CI cost (2026-10-30, "setuju C-1 sampai C-5"; information session)

The private repository hit GitHub's usage limit. The owner: *"maaf saya suruh hapus karena repo private kena limit usage $12 lebih saya kena peringatan github"*, and approved the plan with *"setuju C-1 sampai C-5"*. Measured before the change: one run was 7 jobs and about 24 job-minutes (check 14.0, e2e 3.2, deny 2.3, app-template 2.1, sites 1.4, msrv 0.8, desk 0.5), about 121 runs in 2.2 days, no dependency cache. Behaviour of the checks is unchanged; only when and how they run.

| Decision | Recorded in |
|---|---|
| C-1: cache the Cargo registry, git db and `target/` with the GitHub-owned `actions/cache` (pinned by SHA; a new CI action approved by this decision), `CARGO_INCREMENTAL=0`; the weekly run and `workflow_dispatch` on main warm the cache that pull requests share. | [maintenance policy](blueprint/maintenance-policy.md#ci), [dependency evaluation](blueprint/dependency-evaluation.md#11-supply-chain-tooling-and-msrv), [`ci.yml`](../.github/workflows/ci.yml) |
| C-2: draft pull requests skip CI; the working session opens drafts, checks locally and marks ready only after review PASS (section 10.3 unchanged: CI green on the head before merge). | [AGENTS.md section 10.2](../AGENTS.md#102-roles-in-the-working-session), [maintenance policy](blueprint/maintenance-policy.md#ci), [`ci.yml`](../.github/workflows/ci.yml) |
| C-3: no run on push to main; the schedule is weekly; `workflow_dispatch` stays. | [maintenance policy](blueprint/maintenance-policy.md#ci), [`ci.yml`](../.github/workflows/ci.yml) |
| C-4: path filters without a third-party action: a `changes` job and a tested classifier script; unknown paths count as Rust; a `docs` job checks links offline. | [maintenance policy](blueprint/maintenance-policy.md#ci), [`ci.yml`](../.github/workflows/ci.yml), [CONTRIBUTING](../CONTRIBUTING.md#local-checks) |
| C-5: `msrv` and `deny` become one job, `msrv-deny`, with one cache. | [maintenance policy](blueprint/maintenance-policy.md#ci), [`ci.yml`](../.github/workflows/ci.yml) |
