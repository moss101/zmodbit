# Performance, Context Economics, and Benchmark Plan

> **Authority date:** 2026-09-03  
> **Product:** Modbit — clean-slate implementation dossier  
> **Completion rule:** code is not “done” until it is wired through the real runtime and passes the release-gate real-system test with evidence.  
> **No-placeholder rule:** production code paths may not contain fake implementations, TODO return values, hard-coded success, disabled security checks, or UI-only simulations of unavailable behavior.


All numbers are **engineering targets**, not claimed results.

## Desktop/runtime budgets

| Metric | Target |
|---|---:|
| Warm fleet snapshot from local Core | < 150 ms p95 |
| Local command dispatch overhead excluding process | < 25 ms p95 |
| L0 exact/symbol retrieval warm | < 100 ms p95 |
| L1 hybrid retrieval warm on medium repo | < 300 ms p95 |
| Context Pack compile excluding embedding backlog | < 500 ms p95 typical |
| UI event-to-render latency | < 100 ms p95 |
| Terminal replay attach | < 250 ms p95 local |
| Browser semantic delta after settled DOM change | < 250 ms p95 local |
| Core idle RSS | target < 250 MB excluding indexes/LSP/model assets |
| Renderer idle RSS | target < 300 MB |

## Repository scale classes

Benchmark at roughly:
- small: <10k files;
- medium: 10k–100k files;
- large: 100k+ files / multi-million LOC.

Measure cold index, incremental one-file edit, branch/worktree switch, symbol query, hybrid query, structural impact query and memory footprint.

## Retrieval comparison

Profiles:
- **A Baseline:** exact/ripgrep-like tools.
- **B Hybrid:** exact + BM25 + semantic ANN.
- **C Modbit Structural:** B + AST/LSP/dependency/Git/diagnostics/runtime evidence and planner.

Hold model, prompt, task, environment and limits constant. Agent is not forced to call retrieval treatment.

### Targets derived from project hybrid-retrieval benchmark comparison

On equivalent BrowseComp-Plus protocol, aim for:
- answer accuracy ≥ ~99%;
- input tokens reduction ≥ ~40% vs baseline;
- tool calls reduction ≥ ~45%;
- agent time reduction ≥ ~40%.

Do not publish/claim until independently reproduced. Record index build time separately and also report end-user cold-start + incremental-index cost.

## Engineering benchmarks

- SWE-QA-style repository multi-hop question suite.
- Relevant-file recall@1/@5/@10.
- Evidence precision among injected chunks.
- Changed-code impact accuracy.
- Diagnostic/test linkage precision.
- Cross-file relation answer correctness.
- SWE-bench Verified or equivalent coding suite using frozen model/environment for regression; establish baseline before setting score target.

## Agent reliability metrics

- task success across N independent trials;
- median/p95 tokens, tool calls and wall time;
- wrong-effect attempts blocked;
- number of repair loops;
- completion evidence coverage;
- restart/resume success.

## Browser metrics

- percentage actions solved structurally without screenshot;
- semantic state bytes vs full AX snapshot bytes;
- delta bytes/action;
- stable entity ID survival across benign DOM rerenders;
- targeted visual fallback success;
- takeover latency.

## Performance regression policy

A PR fails if deterministic benchmark worsens >10% on a protected metric without approved decision record. Agent/model benchmarks run nightly because stochasticity/cost requires repeated trials; release uses frozen baseline and confidence intervals.


## V2 benchmark profiles

Retrieval benchmark must include A baseline exact search, B hybrid retrieval and C structural Modbit context. Keep task/model/revision/environment fixed and report verified outcome, input tokens, normalized tool calls, agent time, cold index time, incremental update time, recall@K and evidence precision. Skill evolution reports no-skill/current/candidate paired results and modality tests report bytes transformed/provider latency/vision fallback rate. External benchmark numbers remain targets until reproduced.
