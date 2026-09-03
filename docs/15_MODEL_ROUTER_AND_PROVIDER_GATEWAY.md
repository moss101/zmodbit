# Model Router and Provider Gateway

> **Authority date:** 2026-09-03  
> **Product:** Modbit — clean-slate implementation dossier  
> **Status vocabulary:** **LOCKED**, **PROVISIONAL**, **EXPERIMENT**, **DEFERRED**, **REJECTED**  
> **Source-of-truth rule:** latest explicit Modbit decision > locked decisions > current dossier > older project documents. Older Code-OSS/Modbit Lite material is historical only when it conflicts with this dossier.


## Objective

Expose one normalized streaming inference contract while allowing multiple providers/models and routing by capability, policy, reliability, latency, cost and quota. Provider-specific semantics never leak into Agent Runtime state machines.

## Provider contract

```text
ModelRequest
  request_id
  model_policy
  messages/context segments
  tool_projection
  response_format?
  reasoning_effort?
  cache_key/prefix metadata
  max_output_tokens
  timeout
  tenant/user policy tags

ModelEvent
  MessageDelta
  ReasoningDelta (when provider exposes it)
  ToolCallStart
  ToolCallDelta
  ToolCallComplete
  Usage
  ProviderMetadata
  Completed
  Error
```

Adapters translate OpenAI-compatible and Anthropic-style APIs first. Additional providers implement the same conformance suite before exposure.

## Routing

Route on a `TaskFingerprint`:
- code generation vs repository analysis vs browser/research;
- context size and modality;
- tool-calling/procedural-runtime support;
- required reasoning tier;
- latency class;
- privacy/execution policy;
- historical provider health;
- tenant quota and configured budget.

A user model selection can pin the route. “Auto” is explicit and observable: UI shows selected model/provider and reason codes.

## Health and failover

Provider health includes rolling success rate, first-token latency, stream interruption, tool-call validity, rate-limit state and cost. Retry is bounded with jitter. Failover is allowed only before an effectful tool action derived from an ambiguous partial response; after ambiguity, restart the turn from the last safe state with a new turn attempt.

## Prompt cache economics

Prompt Compiler emits stable segments:
1. system/policy;
2. stable workspace rules/skill manifests;
3. compaction epoch;
4. task context pack;
5. recent events.

Cache keys include model/provider/prompt compiler version and stable segment hashes. Context economics records cache hit/miss, input tokens by segment and recomputation cause.

## Credentials

Raw provider secrets never enter renderer/model context. Local provider credentials use OS-protected storage accessed by Core/main boundary. Hosted service credentials live in cloud secret manager; sandbox receives only short-lived broker handles when explicitly required for a tool, never model API secrets.

## Live provider proof

Provider adapters are not considered complete until nightly/RC CI successfully performs a real streaming model call, a real typed tool-call round trip and cancellation/timeout against the production provider endpoint using dedicated test credentials.


## V2 multimodal/provider capability fields

`ModelCapability` must include agent-loop support, vision, supported media modalities, context/output limits, parallel-tool support, structured-output support, tool-message media constraints and schema-compatibility mode. Provider adapters may transform canonical media placement (for example splitting media from a tool-role message) but may not alter canonical call/result identity. Routing rejects an endpoint that cannot satisfy required modality/policy/data-residency constraints.
