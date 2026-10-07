---
id: docs-frontmatter
type: design
title: Documentation frontmatter specification
status: current
created: 2026-09-21
tags:
  - documentation
  - conventions
---

# Documentation frontmatter specification

## Common fields

```yaml
---
id: unique-document-id
type: adr
title: Human-readable title
status: accepted
created: 2026-09-21
updated: 2026-09-21
owners:
  - hisamekms
tags:
  - architecture
related:
  - design-overview
---
```

`id`, `type`, `title`, `status`, and `created` are required, and ADRs also require `updated`. `owners`, `tags`, and `related` are optional lists of strings. Dates use ISO 8601 calendar dates (`YYYY-MM-DD`). IDs are stable and use lowercase kebab-case, except ADR IDs. ADR IDs (`adr-NNNN` and `adr-t<task ID>-<N>`), their filenames and how they are referenced follow [the documentation rules](development/documents.md) (「ADRのID」).

In an ADR, `updated` is the last content change. Documents other than ADRs (types `design`, `development` and `plan`) have no `updated` or `last_verified`; when a document changed and which task changed it are in the Git history, per [ADR-t1964-1](adr/2026-10-07-t1964-1-non-adr-docs-drop-updated-and-last-verified.md). The `created` line and the `updated` line of an ADR hold only the date, with no trailing comment such as `# task N` naming the task or the change; which task changed a document is in the Git history (the `Dagq-Task` trailer of the landing commit), per [ADR-t1854-1](adr/2026-10-07-t1854-1-frontmatter-date-lines-hold-only-the-date.md) decision 1.

## Type-specific fields

| Type | Allowed status | Additional fields |
| --- | --- | --- |
| `adr` | `proposed`, `accepted`, `rejected`, `superseded`, `deprecated` | `accepted_on`, `superseded_by`, `superseded_on`, `deprecated_on`, `supersedes`, `amends`, `amended_by` (see [ADR fields](#adr-fields)) |
| `design` | `draft`, `current`, `deprecated`, `superseded` | optional `scope` |
| `plan` | `proposed`, `active`, `blocked`, `completed`, `archived` | optional `milestone`, `target`, `depends_on` |
| `development` | `current`, `deprecated` | — |

Design documents describe the current state and may be edited. Development documents (`docs/development/`) hold this repository's current development rules (how a planner chooses verify, paths, evidence and change, what tests a worker runs, the test and documentation rules) and may be edited, with IDs `development-<slug>`; the reasons and history stay in ADRs and plans ([ADR-t1453-2](adr/2026-10-03-t1453-2-ownership-of-agents-md-plugin-development-docs-and-config.md)). Plans describe intended work and may be edited while active. The progress and state of individual tasks live in the dagq queue, not in documents.

## ADR fields

This section holds the fields and the formats. How ADRs are written, replaced, amended and indexed (small ADRs, append-only, replace or amend, the index) is in [the documentation rules](development/documents.md) (「ADR」), the one place for those rules.

| Status | Meaning |
| --- | --- |
| `proposed` | Under consideration. Its decisions are not in force yet |
| `accepted` | Adopted. Every decision in its body is in force, except the decisions changed by the ADRs in its `amended_by` |
| `rejected` | Not adopted |
| `superseded` | Replaced as a whole by the ADR in `superseded_by` |
| `deprecated` | Retired without a successor |

| Field | Carried by | Value |
| --- | --- | --- |
| `accepted_on` | `accepted`, `superseded`, `deprecated` | The date the ADR moved from `proposed` to `accepted`. A `rejected` ADR has none |
| `superseded_by` | `superseded` | One ADR ID of the successor. If the successor is itself superseded, the reader follows the chain to an `accepted` ADR |
| `superseded_on` | `superseded` | The date the ADR became `superseded` |
| `deprecated_on` | `deprecated` | The date the ADR became `deprecated` |
| `supersedes` | The replacing ADR | A list of the ADR IDs it replaces |
| `amends` | A new ADR that changes some decisions of an ADR with several numbered decisions | A list of `<ADR ID> decision <N>` entries it changes, for example `adr-0047 decision 24` |
| `amended_by` | An ADR with several numbered decisions, changed in part | A list of the IDs of the ADRs that amend it |

```yaml
status: superseded
created: 2026-09-22
updated: 2026-09-22
accepted_on: 2026-09-22
superseded_by: adr-0040
superseded_on: 2026-09-25
```

```yaml
status: deprecated
created: 2026-09-22
updated: 2026-09-22
accepted_on: 2026-09-22
deprecated_on: 2026-09-25
```

A `deprecated` ADR has no `superseded_by` or `superseded_on`, and a `superseded` ADR has no `deprecated_on`.

- **Banner.** A `superseded` or `deprecated` ADR has a one-line note directly after its H1. The superseded banner is dated with `superseded_on`, and the deprecated banner with `deprecated_on`:

  ```markdown
  > **置き換え済み（YYYY-MM-DD）**: このADRの決定は現在有効ではない。現行の決定は[ADR-XXXX](XXXX-....md)を読む。
  ```

  ```markdown
  > **廃止（YYYY-MM-DD）**: このADRの決定は現在有効ではない。理由: ...
  ```

## Validation

Three scripts check the frontmatter (what each checks in detail is in the script's header comment). `scripts/check-adr-numbers.sh` checks the ADR filenames and IDs and their uniqueness among the ADRs, and that a task-ID ADR that is `accepted`, `superseded` or `deprecated` has an `accepted_on` equal to the date in its filename. `scripts/check-frontmatter-dates.sh` checks that each `created`, `updated` and `last_verified` line in a frontmatter under `docs/` holds only a date of the form `YYYY-MM-DD`. `scripts/check-doc-frontmatter.sh` checks, for the documents other than the ADRs, the required keys, the type and status values, the form and uniqueness of the IDs, and that no `updated` or `last_verified` line is there.

No script checks yet: that the IDs in `related`, `depends_on`, `superseded_by` and `supersedes` name existing documents; that an ADR ID differs from the IDs of the other documents; the required keys and the status values of the ADRs; the other ADR status and field combinations and dates (see [ADR fields](#adr-fields)), including the form of `accepted_on`, `superseded_on` and `deprecated_on`; and the filenames of the documents other than the ADRs. These are left to the review.

Example paths:

```text
docs/adr/0001-rust-runtime.md
docs/adr/2026-09-26-t598-1-adr-id-is-task-id-small-adrs-and-design-holds-current-state.md
docs/design/supervisor-lifecycle.md
docs/plans/rust-runtime-mvp.md
```
