# Requirement → Task → Test Traceability

## Trace chain

- Product requirement / architecture invariant → milestone task(s).
- Evidence-derived requirement `REQ-EV-nnnn` → `IMP-EV-nnnn` when production/experiment implementation is applicable.
- Every evidence-derived requirement → `QUAL-EV-nnnn`.
- High-risk capabilities additionally map to E2E/security/fault/performance test IDs.

## CI rule to implement

A parser should fail CI when:

- an ADOPT/ADAPT requirement lacks a canonical owner;
- its `IMP-EV-*` task is absent;
- its `QUAL-EV-*` test is absent;
- a task says COMPLETE but no qualifying evidence ref exists in the build manifest;
- a code module registers a production capability with no requirement ID;
- an architectural owner has two active production implementations without an approved migration ADR.
