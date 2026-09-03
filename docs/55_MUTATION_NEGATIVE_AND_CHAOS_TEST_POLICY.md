# Mutation, Negative and Chaos Test Policy

## Purpose

Positive tests can pass while security/recovery checks are ineffective. Critical gates need tests that prove the test itself detects broken behavior.

## Mutation requirements

For policy, effect idempotency, checkpoint fencing, context freshness, tenant isolation, secret redaction and path protection, periodically introduce controlled mutations such as inverted predicate, skipped fence, stale read, duplicate dispatch or removed redaction and prove the suite fails.

## Negative fixtures

Maintain fixtures for ambiguous edits, invalid/stale IDs, duplicate requests, oversized outputs/media, malformed provider/tool events, hostile web/doc content, symlink/path escapes, cross-tenant handles, expired capabilities and corrupted artifacts.

## Chaos

Nightly/staging may inject process kills, network loss, latency, partial responses and resource exhaustion. Chaos tests must have bounded blast radius and deterministic post-run invariant checks.
