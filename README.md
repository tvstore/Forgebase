# Forgebase

Forgebase is a Rust framework for building business applications, inspired by the concepts and capabilities of the Frappe Framework. It is an independent implementation: no Frappe code is copied.

> Build for longevity, not complexity. Maintenance is the most expensive part of software ownership.

## Status

**Research and blueprint phase. There is no code yet.** The documents below record research, proposed architecture and open decisions. Most decisions are **Proposed**, not final; each document marks its status.

The repository is currently **private**. It is intended to be open source under the MIT license; it is not open source yet.

## Documentation map

| Document | Contents |
|---|---|
| [AGENTS.md](AGENTS.md) | Permanent project instructions: engineering principles, four-stage workflow, git safety, model selection policy |
| [docs/blueprint/vision.md](docs/blueprint/vision.md) | Purpose, target users, goals, non-goals, relation to Frappe |
| [docs/blueprint/feature-matrix.md](docs/blueprint/feature-matrix.md) | Frappe capabilities vs Forgebase status, with evidence labels |
| [docs/blueprint/architecture.md](docs/blueprint/architecture.md) | Proposed architecture: crates, metadata, lifecycle, API, jobs, multi-site |
| [docs/blueprint/dependency-evaluation.md](docs/blueprint/dependency-evaluation.md) | Rust dependency comparison, licenses, recommendations |
| [docs/blueprint/security-model.md](docs/blueprint/security-model.md) | Authentication, authorization, injection defense, isolation |
| [docs/blueprint/milestones.md](docs/blueprint/milestones.md) | M0 to M5 and later work, with exit criteria |
| [docs/blueprint/maintenance-policy.md](docs/blueprint/maintenance-policy.md) | Toolchain, dependency admission, testing, docs and review rules |
| [docs/adr/](docs/adr/README.md) | Architecture Decision Records |

## Getting started

There is nothing to build or run yet. To get oriented, read the blueprint in this order: [vision](docs/blueprint/vision.md), [architecture](docs/blueprint/architecture.md), [milestones](docs/blueprint/milestones.md). Contributors and agents must follow [AGENTS.md](AGENTS.md).

## License

MIT. See [LICENSE](LICENSE).
