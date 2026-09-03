# ADR-<next number>: <one-line decision>

- **ID:** ADR-<next number>
- **Status:** PROPOSED | ACCEPTED | REJECTED | SUPERSEDED
- **Date:** YYYY-MM-DD
- **Affects:** <locked file path(s) this ADR authorizes changing>
- **Decides:** <the invariant or behavior being changed/established>

## Trigger / Evidence

What forced this decision. Link evidence (runs, incidents, requirement rows, benchmarks).

## Current Behavior

The locked invariant or implementation behavior as it stands before this ADR.

## Proposed Replacement

The new invariant/behavior, stated precisely enough to implement and test.

## Migration

How existing state, code and workflows move from current to proposed.

## Compatibility

Impact on protocol/schema/API/event compatibility, local↔cloud parity, and stored data.

## Security Impact

Threat-model consequences; policy/effect/secret implications; new attack surface.

## Test Impact

Required changes to QUAL rows, E2E scenarios, conformance suites.

## Rollback

How to revert the decision and restore the prior invariant safely.

## Explicit User Approval

Who approved, when, and via what channel. An ADR without a concrete human
approval here is not ACCEPTED.
