# Decision Records (ADRs) — Status Ledger

Every change to a **LOCKED** invariant or a locked architecture file requires a
Decision Record (ADR) in this directory, per `docs/02_AUTHORITY_AND_DECISIONS.md`
(§ Change control). A PR that silently changes a locked invariant fails
architecture CI (`tools/decision-guard.py`).

## Process

1. Copy `TEMPLATE.md` to `ADR-<next number>-<slug>.md`.
2. Fill every section — an ADR with an empty section is invalid and the guard rejects it.
3. `Status` starts as `PROPOSED`. `ACCEPTED` requires the Explicit User Approval
   section to name the human approval (who, when, how).
4. Include the ADR **in the same changeset** as the locked-file change it authorizes.
5. Add a row to the ledger below.

## Locked architecture files

Changes to these paths require a linked, accepted ADR in the same changeset:

- `docs/02_AUTHORITY_AND_DECISIONS.md` — decision register
- `docs/03_ARCHITECTURAL_CONFLICTS_AND_SUPERSESSIONS.md` — supersession ledger
- `docs/40_EVIDENCE_DERIVED_REQUIREMENT_LEDGER.md` — requirement rows (LOCKED, docs/46)
- `docs/41_EVIDENCE_DERIVED_IMPLEMENTATION_TASKS.md` — task rows (LOCKED, docs/46)
- `docs/42_EVIDENCE_DERIVED_QUALIFICATION_TEST_MATRIX.md` — qualification rows (LOCKED, docs/46)
- `docs/46_REQUIREMENT_COVERAGE_FREEZE_GATE.md` — freeze gate
- `docs/81_ARCHITECTURE_GUARDRAILS_AND_FORBIDDEN_DUPLICATION.md` — canonical ownership

## Ledger

| ID | Title | Status | Date | Affects |
|---|---|---|---|---|
| ADR-0001 | Adopt the docs/02 decision register as baseline authority and add decisions workflow | ACCEPTED | 2026-09-04 | docs/02_AUTHORITY_AND_DECISIONS.md |
