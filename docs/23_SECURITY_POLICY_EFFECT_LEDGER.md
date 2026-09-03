# Security, Policy, Capabilities, Secrets, and Effect Ledger

> **Authority date:** 2026-09-03  
> **Product:** Modbit — clean-slate implementation dossier  
> **Status vocabulary:** **LOCKED**, **PROVISIONAL**, **EXPERIMENT**, **DEFERRED**, **REJECTED**  
> **Source-of-truth rule:** latest explicit Modbit decision > locked decisions > current dossier > older project documents. Older Code-OSS/Modbit Lite material is historical only when it conflicts with this dossier.


## Threat model

Assume hostile repository files, package scripts, model output, browser content, MCP servers, sandbox guests, downloads, symlinks, terminal output and network responses. The renderer and model are not trusted decision-makers for privileged effects.

## Capability kernel

A `CapabilityLease` binds tenant/user/session/task/agent, resource selector, operation set, effect ceiling, execution profile, expiry and generation. Tools must present a valid lease; tool name alone is not authority.

Examples:
- `fs.read:/repo/**`
- `fs.write:/repo/src/**`
- `git.commit:worktree-123`
- `network.egress:api.github.com:443`
- `browser.control:session-9`
- `secret.use:github-token -> origin api.github.com`

## Approval policy

Default policy should permit routine read/search/test activity and reversible writes inside isolated worktrees while escalating:
- protected path writes;
- destructive Git/filesystem actions;
- external sends/posts/purchases/deploys;
- new secret use;
- permission expansion;
- browser actions with irreversible effects;
- ambiguous effect outcome.

Approval binds normalized intent hash + scope + expiry. Changing parameters invalidates approval.

## Protected-effect receipt chain

For protected/external effects append:

```text
EffectReceipt {
  effect_id
  previous_receipt_hash
  session/task/turn/step/tool_call
  capability_lease
  normalized_intent_hash
  policy_decision
  approval_id?
  execution_target
  precondition/checkpoint refs
  result/evidence refs
  status
  occurred_at
  receipt_hash
}
```

The chain is append-only and independently verifiable. A rejected/failed effect is also recorded when security-relevant.

## Secrets

- Renderer never sees raw cloud/provider secrets.
- Sandbox images contain no tenant secrets.
- Guest receives short-lived scoped credential material only through broker at execution time, preferably via pipe/fd/memory rather than static env.
- Secret handles are origin/tool/effect scoped and auditable.
- Terminal output redactor detects known secret fingerprints before persistence/display while preserving original only in protected diagnostic vault if policy allows.

## Protected paths

System config, SSH keys, credential stores, `.git` internals, CI secrets and user-defined paths default to deny or approval. Path checks are performed after symlink resolution and before each write/open, not only at task creation.

## Supply chain

Dependencies are pinned by lockfiles; release builds generate SBOM, verify licenses and run vulnerability scans. Guest/sandbox images are digest-pinned and signed. Skills and external tool manifests are signed/versioned; unsigned local development skills are visibly marked.

## Emergency stop

Global stop revokes active capability leases, blocks new effects, cancels safe tool calls, freezes dangerous external operations at broker/gateway where possible and marks ambiguous outcomes for reconciliation.
