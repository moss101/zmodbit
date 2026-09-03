# ADR-0001: Adopt the docs/02 decision register as baseline authority and add the decisions workflow

- **ID:** ADR-0001
- **Status:** ACCEPTED
- **Date:** 2026-09-04
- **Affects:** docs/02_AUTHORITY_AND_DECISIONS.md
- **Decides:** The MOD-* decision register in docs/02 is the baseline authority set for the build; future changes to LOCKED items follow the Decision Record workflow in `docs/decisions/`.

## Trigger / Evidence

M0.2 ("Add authoritative ADRs and status ledger", docs/43) requires an ADR
mechanism with CI enforcement. docs/02 § Change control requires a Decision
Record with trigger/evidence, current behavior, proposed replacement, migration,
compatibility, security impact, test impact, rollback and explicit user approval
before any LOCKED item changes.

## Current Behavior

The decision register exists in docs/02 but no Decision Record workflow,
template, ledger, or CI enforcement exists; nothing mechanically prevents a
silent edit to a locked invariant.

## Proposed Replacement

1. `docs/decisions/` holds the ADR template, this ledger (README), and one ADR
   per decision change.
2. `tools/decision-guard.py` fails architecture CI when a locked architecture
   file is changed without an ACCEPTED ADR covering it in the same changeset.
3. docs/02 § Change control gains a pointer to this directory so the register
   self-documents where records live.

## Migration

None — the register content is unchanged; only the workflow and pointer are added.

## Compatibility

No protocol, schema, API or event changes. Documentation-only plus CI tooling.

## Security Impact

Positive: locked security-relevant invariants (receipt chain, sandbox policy,
path policy) can no longer be weakened silently. The ADR template forces an
explicit Security Impact analysis per change.

## Test Impact

New guard self-test proves detection of: locked change without ADR, invalid ADR
(missing sections/approval), and clean passes. Wired into the CI architecture job.

## Rollback

Revert the changeset; the guard and ADR directory are additive and stateless.

## Explicit User Approval

Approved by mohsin (repo owner, account moss101) on 2026-09-04 by commissioning
implementation of the graph queue ("implement all the actions as per graph
starting with M0.2") in the ZCode session, M0.2 being this exact task.
