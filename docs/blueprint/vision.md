# Vision

Status: Proposed (awaiting owner review). Workflow and model policy: [AGENTS.md](../../AGENTS.md).

## Purpose

Forgebase is a Rust framework for building business applications from metadata: describe a document type (DocType) once, and get storage, validation, permissions, an API and an admin UI from that description. It takes its concepts from the Frappe Framework and re-implements them independently on a smaller, simpler stack (Rust, PostgreSQL, React + TypeScript).

> Build for longevity, not complexity. Maintenance is the most expensive part of software ownership.

## Target users

These are assumptions, not validated research:

- Developers who want Frappe-style metadata-driven business apps with a typed, compiled backend.
- Small teams who must maintain the result for years with few people.

## Goals

- Low long-term maintenance cost above all else.
- A small set of well-tested core capabilities, delivered incrementally ([milestones](milestones.md)).
- One binary and one datastore (PostgreSQL) for the core: no Redis, no Node service.
- Explicit, documented behavior: one fixed document lifecycle, deny-by-default permissions.
- Decisions recorded as ADRs ([docs/adr](../adr/README.md)).

## Explicit non-goals

- Feature parity with Frappe or ERPNext.
- Runtime plugins, dynamic libraries, WASM extensions (Forgebase "apps" are compiled in; see [architecture](architecture.md)).
- Server Scripts or admin-authored code stored in the database.
- Microservices, Redis, a separate realtime server, GraphQL.
- Supporting databases other than PostgreSQL.
- Domain applications (accounting, ERP modules). Forgebase is the framework only.

The full list of what is deliberately not built is in [architecture](architecture.md#what-we-deliberately-do-not-build).

## Principles

The ten engineering principles live in [AGENTS.md](../../AGENTS.md#2-core-engineering-principles). In short: simplicity first, modular monolith, dependency discipline, no unsupported assumptions, incremental delivery. When speed conflicts with maintainability, maintainability wins.

## Relation to Frappe

- Forgebase is **concept-inspired**: DocTypes, metadata-driven CRUD, hooks, docstatus, permissions, sites.
- It is an **independent implementation**. No Frappe source code was read for design or copied; the research used web-search summaries of official Frappe documentation pages (the pages themselves could not be fetched in full), and some claims rest on general knowledge and are labelled Unverified ([feature matrix](feature-matrix.md) explains the evidence labels).
- Where Frappe's design is costly to maintain, Forgebase deliberately differs (for example real foreign keys, compile-time apps, one lifecycle pipeline). Each difference is marked "Intentional difference" in the [feature matrix](feature-matrix.md).
- Forgebase is not compatible with Frappe apps, data or APIs, and does not aim to be.
