# Modbit Dossier Manifest — V3.1

> **Authority date:** 2026-09-03  
> **Generated:** 2026-09-05 by `tools/build_manifest.py`  
> **Scope:** every specification file in `docs/` plus the root governing files and tooling. The previous `99_MANIFEST.md` covered only 39 Part 2 files; this manifest covers all 70 docs.
> **Machine-readable twin:** `manifest.json` (same content, same hashes).

## Integrity rule

A dossier package is valid only if every path below exists with the listed SHA-256. `python3 tools/check_dossier.py --manifest` verifies this. Regenerate after any edit with `python3 tools/build_manifest.py`.

## Summary

| Section | Range | Files | Bytes |
|---|---|---:|---:|
| Authority and orientation | 00–09 | 5 | 26678 |
| Architecture and subsystems | 10–29 | 17 | 76622 |
| Implementation specifications | 30–39 | 8 | 29941 |
| Requirements, tasks and traceability | 40–49 | 9 | 292645 |
| Verification and testing | 50–69 | 11 | 39710 |
| Delivery and operations | 70–79 | 5 | 15036 |
| Agent process and governance | 80–97 | 14 | 25221 |
| Live state | 98–99 | 1 | 2283 |
| **Total docs** | | **70** | **508136** |

## Specification files (`docs/`)

| # | File | Title | Section | Bytes | SHA-256 |
|---:|---|---|---|---:|---|
| 00 | `docs/00_MASTER_INDEX.md` | Modbit — AI-Agent Build Dossier V3.1 | authority | 10612 | `4739b765f5781a297a231a5a1f4b29e618d2f4924e7b5ab81099b011c74a3ccb` |
| 01 | `docs/01_START_HERE_FOR_BUILD_AGENTS.md` | Start Here for Build Agents | authority | 2109 | `6ad6ee428f676259a10a5e27e57f1e89216304f4d527c1f2afe4c085f9809e5a` |
| 02 | `docs/02_AUTHORITY_AND_DECISIONS.md` | Authority, Decision Register, and Conflict Resolution | authority | 8658 | `62e0ecd4fb2141ae49f4165a9216a622ea7f2f16c12f22912a467adc24957756` |
| 03 | `docs/03_ARCHITECTURAL_CONFLICTS_AND_SUPERSESSIONS.md` | Architectural Conflicts and Supersessions | authority | 4047 | `e5fcb6e088ee2fa07777cb45d93f3179df08fa78b722cb820d37a6e891e81d87` |
| 04 | `docs/04_REQUIREMENT_BASIS_AND_LIMITS.md` | Requirement Basis and Limits | authority | 1252 | `942aec1f5b14f71d9bc5f398ecb8dc4c4f5636c0d23db4d0ef83b7bf2ef5cb5d` |
| 10 | `docs/10_PRODUCT_PRD_AND_UX.md` | Product Requirements and UX Specification | architecture | 6497 | `fa91e06198c0527e5a0bed57ecd1ff33066f7bd8036dbc08d573d1e5cac16252` |
| 11 | `docs/11_SYSTEM_ARCHITECTURE.md` | End-to-End System Architecture | architecture | 8744 | `986c4912fa8e6ebc77560342d92d87b837095cc39628851df0d483493ffb9674` |
| 12 | `docs/12_REPOSITORY_AND_MODULE_LAYOUT.md` | Clean Repository and Module Layout | architecture | 4967 | `cdeafe379b26d5f8f85eef0b4ed02d0a0969b84c06d4688aa6f94b02dac6ca97` |
| 13 | `docs/13_DOMAIN_MODEL_AND_STATE_MACHINES.md` | Canonical Domain Model and State Machines | architecture | 4296 | `4e0f5a5f9984679bd8dfb4f129d7f8864f5135ec8203d7dd4985a6df79d33aee` |
| 14 | `docs/14_AGENT_RUNTIME_AND_ORCHESTRATION.md` | Agent Runtime and Orchestration | architecture | 4795 | `6658315a4a4096569c936448a0b7c566d1b4bd926dda05ce1dc01a6180c6f78a` |
| 15 | `docs/15_MODEL_ROUTER_AND_PROVIDER_GATEWAY.md` | Model Router and Provider Gateway | architecture | 3591 | `9491115d519a3a9dbce3f95cf7704f152ce9dd763a3e168bce3d24c319bbeeba` |
| 16 | `docs/16_TOOL_CAPABILITY_AND_PROCEDURAL_RUNTIME.md` | Tool System, Capability Kernel, Procedural Runtime, and MCP | architecture | 4550 | `5e8b803190bdd39f246f99bd614bdd3f767b58aafd730a5ca9ed11fdf65499e2` |
| 17 | `docs/17_CANONICAL_TOOL_AND_CAPABILITY_INVENTORY.md` | Canonical Tool and Capability Inventory | architecture | 4372 | `dec6fbc6f5bee1cb3313bef50ffb10199c1f64bdcfef49ba0b7bac06447ca4ac` |
| 18 | `docs/18_CONTEXT_RETRIEVAL_AND_ENGINEERING_KNOWLEDGE.md` | Context, Retrieval, and Engineering Knowledge Engine | architecture | 4771 | `1899b5869afba2502a22576bceb56edf5fc728d5a36bb192b75798e1c2c97b2a` |
| 19 | `docs/19_DURABLE_STATE_MEMORY_COMPACTION_CHECKPOINTS.md` | Durable State, Memory, Compaction, and Checkpoints | architecture | 4019 | `718634cd402ece3c57c77db48593ca8e59e4720f326fe847546172ac70f71a62` |
| 20 | `docs/20_WORKSPACE_GIT_AND_TRUSTED_CODE_SURFACE.md` | Workspace, Git, Worktrees, Diagnostics, and Trusted Code Surface | architecture | 2883 | `0f1d42c9d0915595c582ca2b73f16040caed17bf2e20638c0464f64f3df3d469` |
| 21 | `docs/21_TERMINAL_EXECUTION_AND_SANDBOX.md` | Terminal, Execution Router, and Sandbox Architecture | architecture | 3332 | `b36b4bb9b3e8d837e2c62e048147acf9b236b74cc006e8fbfc2fc8d751f102ff` |
| 22 | `docs/22_BROWSER_AND_COMPUTER_USE.md` | Browser and Computer-Use Architecture | architecture | 3961 | `a7e441d9bc60785aea7dcc62fe8cef5946f3aa2551665368a360d4ec3519220b` |
| 23 | `docs/23_SECURITY_POLICY_EFFECT_LEDGER.md` | Security, Policy, Capabilities, Secrets, and Effect Ledger | architecture | 3408 | `218e02653a8b7b086672051c0534581f7767d8dae8c8e969e32b99c5025f5297` |
| 24 | `docs/24_CLOUD_CONTROL_PLANE_AND_SYNC.md` | Cloud Control Plane, Remote Execution, and Sync | architecture | 2971 | `adbd0235a34e23792054573eb80b94de23bac62e9e959d8d17824adfb2d22f8f` |
| 25 | `docs/25_MULTIMODAL_MEDIA_AND_NOTEBOOK_RUNTIME.md` | Multimodal, Media and Notebook Runtime | architecture | 2960 | `e35d2b47f9ee33aa17e682f4226b26b8daa6ac8823a06722c3877fe416b02918` |
| 26 | `docs/26_SKILL_REGISTRY_AND_EVOLUTION.md` | Skill Registry and Evolution Integration — Skill Evolution Without a Second Runtime | architecture | 6505 | `624555fe55a2697f966a0b76293467e4614fbaef9c6f69580da4b50995412491` |
| 30 | `docs/30_PROTOCOL_APIS_AND_EVENT_SCHEMAS.md` | Protocol, APIs, and Event Schemas | implementation | 5472 | `6927e07ba380272c2a9353bfa855ce2ae450118168fc81826cdb571973cd4af3` |
| 31 | `docs/31_DATABASE_AND_STORAGE_SCHEMA.md` | Database and Storage Schema | implementation | 5445 | `0c7ad8b3021b7f762708700e53f7393d8e29fb8a90fcf5b37b5630833aaf4cc8` |
| 32 | `docs/32_DESKTOP_FRONTEND_IMPLEMENTATION.md` | Desktop Frontend Implementation | implementation | 4183 | `fb96b5be03dd7699e5b3c236e179e2338c3abcf2fd71c723ec098d7ce33dc2c0` |
| 33 | `docs/33_CORE_AND_CLOUD_BACKEND_IMPLEMENTATION.md` | Core and Cloud Backend Implementation | implementation | 4169 | `ccf8a07c3afc5ec451b6dac6a7464bc83566f630f0a68f303acfef6a05163635` |
| 34 | `docs/34_OBSERVABILITY_COST_AND_OPERATIONS_DATA.md` | Observability, Cost, and Operations Data | implementation | 2849 | `0be1a2c47a758c70b75e1678efcadd06fdb27994beb5a1ed6769611792064243` |
| 35 | `docs/35_DEPENDENCY_AND_BINDING_DECISIONS.md` | Dependency and Binding Decisions | implementation | 1492 | `96725161310f8c53975cc067e437c164e4d8def3c1efbfb40394573ec71ae4c6` |
| 36 | `docs/36_BUILD_BUY_DEPENDENCY_AND_LICENSE_POLICY.md` | Build / Buy / Dependency / License Decisions | implementation | 3363 | `538f768996bec4254231f7671517e68bea6cf529f51b60588a5b2dd91007071a` |
| 37 | `docs/37_EXISTING_CODE_DONOR_AND_REUSE_POLICY.md` | Existing-Code Donor and Reuse Policy | implementation | 2968 | `3b694bd3111e152492f2973edaf293f3d0afa40d1e87b1362deda9e31f7e91b6` |
| 40 | `docs/40_EVIDENCE_DERIVED_REQUIREMENT_LEDGER.md` | Evidence-Derived Requirement Ledger — Build Edition | requirements | 76383 | `d673606834f48960f015f4719c0b6fd956988469c39348aa859c4c0d91e20336` |
| 41 | `docs/41_EVIDENCE_DERIVED_IMPLEMENTATION_TASKS.md` | Evidence-Derived Implementation Tasks | requirements | 148721 | `a21427aa970d6494565ea852ffa201c377c0de0d1deb6a524f7262880c2ad507` |
| 42 | `docs/42_EVIDENCE_DERIVED_QUALIFICATION_TEST_MATRIX.md` | Evidence-Derived Qualification Test Matrix | requirements | 48833 | `643bdf34de9158e7fa216a63f86568ffed9f1fe56906a4e595edc6853ac2080f` |
| 43 | `docs/43_IMPLEMENTATION_ROADMAP_AND_TASK_GRAPH.md` | Implementation Roadmap and Verifiable Task Graph | requirements | 8320 | `e7cdf421f603b38fc827caa611889f83f6c2e2becf5ece9216a602ffa6dbacb4` |
| 44 | `docs/44_REQUIREMENTS_TRACEABILITY_MATRIX.md` | Requirements Traceability Matrix | requirements | 3031 | `4ba0a3465db3abd4e60f00fb50963bf6284300792fd13c0e92c08a5eafbbad73` |
| 45 | `docs/45_REQUIREMENT_TO_TASK_TO_TEST_TRACEABILITY.md` | Requirement → Task → Test Traceability | requirements | 833 | `87bf7cf03f7725fdd9559fdbeb4e9e1588d8e91669629c8f3dc04641a2cb0266` |
| 46 | `docs/46_REQUIREMENT_COVERAGE_FREEZE_GATE.md` | Requirement Coverage Freeze Gate | requirements | 716 | `f8942afd05aa108f4bfc80e55a31ed77c9a8bc1e15278a882a281c4468ccd825` |
| 47 | `docs/47_REQUIREMENT_COVERAGE_AUDIT_REPORT.md` | Requirement Coverage Audit Report — Build Edition | requirements | 879 | `573ea050e0825f1320f4ae8b464a9319cb3c3a0c37a2abd503106d555d5339f8` |
| 48 | `docs/48_FEATURE_DEPTH_CONTRACTS.md` | Feature Depth Contracts | requirements | 4929 | `4360e0bb6df93ab871b6dc559917ec2d8f16f99291686806072108958b4dcf07` |
| 50 | `docs/50_TEST_STRATEGY_REAL_SYSTEM_GATES.md` | Test Strategy — Real-System Completion Gates | verification | 4127 | `c20dcc419d0c071db55f61352377499953c5d4a59aa3b06437dff9e240ca0581` |
| 51 | `docs/51_E2E_ACCEPTANCE_TEST_CATALOG.md` | End-to-End Acceptance Test Catalog | verification | 7438 | `eaa3b15c27a0b3d33b309c6742325d131a5c600a13dd36594654c03d41c75804` |
| 52 | `docs/52_SECURITY_THREAT_MODEL_AND_TESTS.md` | Security Threat Model and Verification | verification | 4690 | `ecc70282ba0a0be5d270622deb356ac44ba4f4c91295112992a820ffa8e4d03b` |
| 53 | `docs/53_PERFORMANCE_AND_BENCHMARK_PLAN.md` | Performance, Context Economics, and Benchmark Plan | verification | 3992 | `0813df933c7a7e2cc2fccb8afb2c4b34d97ef23ecd25a01097f231f44d86920a` |
| 54 | `docs/54_FAULT_INJECTION_AND_RECOVERY_CATALOG.md` | Fault Injection and Recovery Catalog | verification | 1744 | `706fce8d19136058bfc90f48bfdcca0b9f67f4556dfea7665665cc7bce065540` |
| 55 | `docs/55_MUTATION_NEGATIVE_AND_CHAOS_TEST_POLICY.md` | Mutation, Negative and Chaos Test Policy | verification | 1006 | `d29b3a0c1f71a8f85cedd18f80c1cb81561d73cfcb96659b5827ada2e70c29e0` |
| 56 | `docs/56_TOOL_CAPABILITY_CONFORMANCE.md` | Tool Parity and Capability Conformance — Real Effect Tests | verification | 2790 | `3f8c4641d4d7996b862b4344a1a5df8ed47714858113c91a53dd9a87fc6a1332` |
| 57 | `docs/57_SKILL_EVOLUTION_REAL_TESTS.md` | Skill Evolution Real-System Tests | verification | 3466 | `fa5335a1367e464dc7c51cffbc972c803b26781b9e17163c81df25b4c2853f21` |
| 58 | `docs/58_MULTIMODAL_MEDIA_REAL_TESTS.md` | Multimodal / Media Real-System Tests | verification | 3391 | `f04a81fdf805ecac83242ff96948ad1467acd11308ac767029935efc1e7801d9` |
| 59 | `docs/59_RELEASE_ZERO_PROOF_SCENARIO.md` | Release Zero — Single Proof Scenario | verification | 3669 | `15fbb005f7a8196b4987468034f8de113dcd15a1ebd3f34dba822075012064fc` |
| 60 | `docs/60_RELEASE_ZERO_EXPANDED_PROOF.md` | Release Zero Expanded Proof — Clean-Slate V2 | verification | 3397 | `706d9697c40a9098bccd62244fe517da292ad1573fa8145dbc904a9e36e754f4` |
| 70 | `docs/70_CI_CD_RELEASE_AND_SUPPLY_CHAIN.md` | CI/CD, Release Engineering, and Supply Chain | delivery | 2870 | `a6beb7f75a81a84b64babee72d75248f68418d9ef18d94418765ed5db07a97ef` |
| 71 | `docs/71_OPERATIONS_RUNBOOK.md` | Operations and Incident Runbook | delivery | 3141 | `4e7140ee8f23dc65e4b4fbd6c42defd7e470bdcdab4fade5aee1f57c4b414062` |
| 72 | `docs/72_RISK_REGISTER_AND_OPEN_DECISIONS.md` | Risk Register and Open Technical Decisions | delivery | 7168 | `9925252939ef27b7c869f5401ea0d4358f0056f0ccc1106c828235a07c75376d` |
| 73 | `docs/73_RELEASE_BLOCKERS_AND_STOP_THE_LINE_RULES.md` | Release Blockers and Stop-the-Line Rules | delivery | 1007 | `23a8ae8af8a2d883b0ae12c90d87e17ce28da0ddc189642adfacc40d986b81fa` |
| 74 | `docs/74_PACKAGE_INTEGRITY_AND_BUILD_COVERAGE.md` | Package Integrity and Build Coverage | delivery | 850 | `b7f0f2e61823c1b591c8c86ff64db73f1b7e618d2065c91b222f84578d5b31dd` |
| 80 | `docs/80_ANTI_SUPERFICIAL_IMPLEMENTATION_STANDARD.md` | Anti-Superficial Implementation Standard | governance | 3193 | `9d5ab7ddbe39110cff675b57fb75c3ec7fd3173480865674f3073489761fcb0f` |
| 81 | `docs/81_ARCHITECTURE_GUARDRAILS_AND_FORBIDDEN_DUPLICATION.md` | Architecture Guardrails and Forbidden Duplication | governance | 1484 | `c1c0e3633914b239be0c97d5c2b0ecf2acb07f799cfedb3461ed39893f352f93` |
| 82 | `docs/82_NO_PLACEHOLDER_PRODUCTION_EVIDENCE_GATE.md` | No-Placeholder Production Evidence Gate | governance | 2418 | `f8ee13f8a255ed071aaa471d2c9a16dbe0e77900faa60d40222a299fe6a975d1` |
| 83 | `docs/83_DEFINITION_OF_DONE_AND_ACCEPTANCE.md` | Definition of Done and Acceptance Criteria | governance | 3840 | `ed66e9d4528f518f5b78612ee2170ae3d7a8c30004f68a98dcd26649f3692730` |
| 84 | `docs/84_EXISTING_CODE_FEATURE_AUDIT_PROTOCOL.md` | Existing-Code Feature Audit Protocol | governance | 1661 | `30747e11bc05265a7785c06be0864104f1c5f9cc58c60afb1c30830caace84b1` |
| 85 | `docs/85_AGENT_TASK_EXECUTION_PROTOCOL.md` | Agent Task Execution Protocol | governance | 1524 | `e9fbb235471fa2a536dfc05ec5af12539060625f61293984a37e0d19f3e298c7` |
| 86 | `docs/86_TASK_CARD_TEMPLATE.md` | Task Card Template | governance | 1141 | `6c474e992049f671c5f44662788bdda47ed05623fab42a16b472ee9faa649998` |
| 87 | `docs/87_HANDOFF_AND_MANIFEST_PROTOCOL.md` | Handoff and Manifest Protocol | governance | 1007 | `cec0530ae98473c30c74d527561f795bbab5f4e323c4ade9d0973d4f95e9d5df` |
| 88 | `docs/88_PARALLEL_AGENT_COORDINATION_RULES.md` | Parallel Agent Coordination Rules | governance | 1087 | `ddf65450d6d2974498c0fb63f6a12e80aeda3aae964d0b2f92c5a4caa4c2f4fc` |
| 89 | `docs/89_BUILD_AGENT_CONTEXT_LOADING_POLICY.md` | Build-Agent Context Loading Policy | governance | 1106 | `c754c7998ae1fe378b00259ce7adde4d09c242dba56e275ac85930a5cd1d5312` |
| 90 | `docs/90_PR_CHANGE_EVIDENCE_TEMPLATE.md` | PR / Change Evidence Template | governance | 931 | `3bf425f520927e92d76facca58376302c0978a34d0906d9743648875e2e889ee` |
| 91 | `docs/91_FEATURE_COMPLETION_AUDIT.md` | Feature Completion Audit | governance | 1074 | `810c4488a24258cff4ed87cd0ba6cea6b21cda5834b44bc02bf4c9e0a4eda3f5` |
| 92 | `docs/92_BUILD_EVIDENCE_AND_DEPENDENCY_MANIFEST.md` | Build Evidence and Dependency Manifest | governance | 829 | `6cbd341583a935351b02637e0d295802a56073070054b5736f58fc9d5790ff86` |
| 93 | `docs/93_STATUS_VOCABULARY_AND_LIFECYCLE.md` | Status Vocabulary and Lifecycle Reconciliation | governance | 3926 | `7693851eabf968fb946a5a84eb70b2af058d2cb5c0846767f51c07307aa70a29` |
| 98 | `docs/98_BUILD_MANIFEST.md` | Build Manifest | live-state | 2283 | `b4b61d304e597eefabcd84e054aba0d6dd8439401b9412870a184efb3424c7ca` |

## Root governing files and tooling

| File | Role | Bytes | SHA-256 |
|---|---|---:|---|
| `README.md` | human orientation | 4960 | `c420d46be36216c21d8892ca9db10f9f549262c11ce5a68ae13bc1cda313fa03` |
| `AGENTS.md` | build-agent operating contract (highest authority) | 6595 | `4eddeb49773780df0b4091cd0eb64cb9c2d167340957fb86a9eae6fd7cccdc25` |
| `SKILLS.md` | governed procedures for agents | 13482 | `e8414f394cc48ad19bbafa2f23c530d6acdd5c302fa0adc68c57a1af19f0fcdf` |
| `graph/project-graph.json` | project driver graph with live status | 636766 | `9235a84d3cca2edb628769670b5a35d54645143f9f140916dc23a9eb8aa372c6` |
| `graph/PROJECT_GRAPH.md` | human view of the graph | 22203 | `e209c5d501db73b4a00a9902a414ac944b6303a927cab1321ade27ce2a09f41b` |
| `tools/build_manifest.py` | regenerates this manifest | 14670 | `3e38e5b32c03532493b7926305400079a6526f0e33bbdd2cb9ab1651e9b5844a` |
| `tools/build_graph.py` | regenerates graph structure from docs | 27166 | `1af561157fe5fc612584dea142ee9ca63ae0e62209792f17e5584c26cf82753a` |
| `tools/graph.py` | query/update graph | 19373 | `4bf6107ea47ece515ec72936110618f2842e4b69ad3b432d9b2172c60f6d1fc2` |
| `tools/check_dossier.py` | integrity gate | 8995 | `0200b96f063e7f65abe3a912e2d5748ca333c07a4ca8fda488282d07699a1f46` |
| `tools/decision-guard.py` |  | 7908 | `cda3b0c9feeb2706ab93489d809dfd5ff0caa8da9207662d51a2e5dbf961bcfc` |
| `.github/workflows/ci.yml` |  | 2984 | `21a25c6b6729e5457261ee403ba7251045c3111b90fce01ec385b33d6bca0f3e` |
| `Cargo.toml` |  | 484 | `0e74629caa64c7d248556e7c48acb53e70da550c36c99c20948504b8fd6995a7` |
| `pnpm-workspace.yaml` |  | 93 | `4f1844ac535cd5c2607091352d8e97cd8b04a48acaf48fb69674de6597a1c1aa` |
| `package.json` |  | 278 | `36a409ec8e4504484b1a773d166a5ec917d8f6903d01858b2b6dd282615fbe6c` |
| `apps/desktop/dist/assets/index-BngEqeZV.js` |  | 146638 | `e2e0c547a033d2e8e1cbef8e576bbf394c5f488745d80c2bf376906fce067f61` |
| `apps/desktop/dist/assets/index-DrM0Ho-V.css` |  | 664 | `e0f5b17ca79348d107ea783cbc7cf8118ecae0a838287dbc82973ee6e5bfffc4` |
| `apps/desktop/dist/index.html` |  | 448 | `5a322bc5abed3f7d0a907a96f4a5015bfe135d17db2d4d3eaebe8bfbdc2e02e1` |
| `apps/desktop/electron/bridge-schema.cjs` |  | 4651 | `25c3a73660ed33a15151993b127111d8ec729590951bfdec70cd204d67df4b62` |
| `apps/desktop/electron/main.cjs` |  | 5045 | `7073604d0cb8f0247bd4f3d0bfd28647d6600c19b73a7ef43dba9262de4019ab` |
| `apps/desktop/electron/preload.cjs` |  | 692 | `7970aa0ef8822ecc426ff163d560636cce1a28b0d5748454e7efbc71d4a5476d` |
| `apps/desktop/electron/surface-client.cjs` |  | 11768 | `92020c6ecc6838bb4e7eced9e05263bc75d5c5b3e6795f650b91327bff192b75` |
| `apps/desktop/index.html` |  | 348 | `fe3a3fc8fb9f256a4f651b6252eede9328674eaef7d67dc9ebba05e3366b0f5d` |
| `apps/desktop/package.json` |  | 485 | `e29470ae01c6516082c8a466b4b361a55c9a76b4605afa9a4b2198f965fafce8` |
| `apps/desktop/src/App.tsx` |  | 3764 | `8ccd9ef39784bdcde1777e3276977c988ed9f71af87dd89ba6430bf2a3c59990` |
| `apps/desktop/src/code-review/revisions.test.ts` |  | 1796 | `cecdc9b266d2a291273dd8738489ec374a4f9d2079755e9cffe582d2c287c6ab` |
| `apps/desktop/src/command-center/commands.ts` |  | 2241 | `789f1cecc06fd978e60eb5304fcdc04995bceeb489d404de82008dafeee33f24` |
| `apps/desktop/src/context-inspector/inspector.test.ts` |  | 1213 | `3d2009a9090dd04b6793af794c894cd64c3df8ad2cb0d821fa8bd48c55ca65b2` |
| `apps/desktop/src/context-inspector/inspector.ts` |  | 1518 | `f38ab87c66e744fa8c0a1b6b5433c445655b5bddcacbf72dea4b41b608503d8b` |
| `apps/desktop/src/fleet/grouping.test.ts` |  | 1567 | `c42b72324ab747e334366296b626c18f5922c7e5ac182c675e5996822c3f7004` |
| `apps/desktop/src/fleet/grouping.ts` |  | 2067 | `289eced8443546348bb7f941b247405307ba92df798dfd1c98546785bfe5c55e` |
| `apps/desktop/src/fleet/supervision.test.ts` |  | 1085 | `b5bcfae4e3b7c1e7e3d2cdf336765e4c2d5cc6382f7d4ae15bde0c6313993283` |
| `apps/desktop/src/fleet/supervision.ts` |  | 918 | `7676a5b0d2bd6909b23c281d741d623743fcaa627048efb55f885f096c15aee4` |
| `apps/desktop/src/global.d.ts` |  | 654 | `1c577b7c9682d24ee958c8969c3069174126ad402a4c7b6db85bfe6ab58dd15f` |
| `apps/desktop/src/main.ts` |  | 188 | `e89ba26ebd97598e96598c91df30513d3608b4053551390a917b73dadbae1d42` |
| `apps/desktop/src/main.tsx` |  | 232 | `ccbefe7be6c69706a1bb01c4c3f20492eb002435af6ba7e5a6d45f7c6aa3a140` |
| `apps/desktop/src/status-center/status.test.ts` |  | 1210 | `21829549ac56b4673383e5f91af4f9fd61bf875816d55eb877813f3d7b7d1005` |
| `apps/desktop/src/status-center/status.ts` |  | 1251 | `b5bac21314e4e4a7d79ad2d05626714e24057e35eb6591ff29ec744ab2e37b3d` |
| `apps/desktop/src/styles.css` |  | 785 | `80b36bcf1afafab6a373c59063fa8358e50fd36cad614dd69ccd13631e7b7862` |
| `apps/desktop/test/bridge.test.ts` |  | 2632 | `64ee3d705a2f2b483ac6b9334159181f517bcfa5d2b423de9f646bddece98efe` |
| `apps/desktop/test/surface.e2e.test.ts` |  | 4444 | `caed16f7a4ba2cf16e0fb35285d065f3f5454f7be9a22681b7810247be0fa4eb` |
| `apps/desktop/tsconfig.json` |  | 384 | `04d41467686090e811fa48d26f64694df7722dfffa12891b0559143d781b62ff` |
| `apps/desktop/vite.config.ts` |  | 150 | `ce36fc4ce9db762ab99a5056aabd3c722f73f37010e639c1f1a2c992c63e5f4d` |
| `docs/decisions/ADR-0001-baseline-decision-register.md` |  | 2328 | `2fcf3eafcce3ccff939244c77c9faa0ebf7106edf2ba7f1cbc9d300524100379` |
| `docs/decisions/README.md` |  | 1657 | `59264591f661d13c95c0e384f27a04852164fbaa878c5ec172ac219b2e748847` |
| `docs/decisions/TEMPLATE.md` |  | 1195 | `666bfdf99aa951eec878352547e20d9a4ea69e8ac18cb2e935b407d71bc2fca3` |
| `packages/design-tokens/package.json` |  | 234 | `20d0f683df2fdb5748ef6582f73d8a643158b2403bc9f1472be1baf0ccb9847a` |
| `packages/design-tokens/src/index.ts` |  | 204 | `f2d07647db91487425dbc9e901b36f5f78b22a83719c3cb65cbaf3d8abe78d55` |
| `packages/design-tokens/tsconfig.json` |  | 323 | `8a64916c72e55ad9185511e81712786b381fea18711dd8741593564b02b26110` |
| `packages/surface-protocol/package.json` |  | 420 | `bbb218f7a79544c401d7df1f6d606a9fb69a4ee6ab6e208b2189bbf37130dda8` |
| `packages/surface-protocol/scripts/generate.mjs` |  | 2626 | `c36bf3f4982af8158048f0d8cde5ff230d10d02ecb605776d89770f0cabdabb8` |
| `packages/surface-protocol/src/generated/google/protobuf/timestamp.ts` |  | 7992 | `41bead303f2e6d23d691fb4f0b00760fe41ac02fe984fcf657274029cdaa1db2` |
| `packages/surface-protocol/src/generated/modbit/protocol/v1/commands.ts` |  | 16905 | `65f4121c6c049dcdf27797ccaf0f08cacad2b8de49d654ee59f2bcf891bf1eb5` |
| `packages/surface-protocol/src/generated/modbit/protocol/v1/common.ts` |  | 3659 | `47a603090fc65ee04ddbd940f71fcf151bd23a2ebbba9c001829f3a203f98e61` |
| `packages/surface-protocol/src/generated/modbit/protocol/v1/domain.ts` |  | 2886 | `eab432c06eb3fc6480a201930f869418ed42236bf040e85dace4e00c50e59f8b` |
| `packages/surface-protocol/src/generated/modbit/protocol/v1/events.ts` |  | 23242 | `f24199327140fa3b9cb39084532b2f2c3bfa807ad19589efdd9197371414c98a` |
| `packages/surface-protocol/src/generated/modbit/protocol/v1/surface.ts` |  | 41596 | `67724e8d0b3d3c28f56b3e36efac82a49c0ec235078e99de802d178593d81d90` |
| `packages/surface-protocol/src/generated/modbit/protocol/v1/transport.ts` |  | 11854 | `c1eb6739e86a75a80872a57916f9526bcd90607a3096efff3067b100e18b3133` |
| `packages/surface-protocol/src/index.ts` |  | 729 | `11be0e5c80215d6c2e4d98b96cddfd9c09496a4c8aae680fe0c96f629c672e10` |
| `packages/surface-protocol/test/wire-compat.test.ts` |  | 4021 | `11780e82f2c5bcc4a982a6a7bf8cf04a416913eda099520ea91249e57275b0da` |
| `packages/surface-protocol/tsconfig.json` |  | 323 | `8a64916c72e55ad9185511e81712786b381fea18711dd8741593564b02b26110` |
| `packages/ui/package.json` |  | 223 | `0690dc195afbe986050db57fe4dbc696df44362c4264502aa4768ed43d6d6bfd` |
| `packages/ui/src/index.ts` |  | 194 | `a526d54c14940b3a22dbaae3cf68391b692ee26b4b3f0a67a5a612d91db21b66` |
| `packages/ui/tsconfig.json` |  | 323 | `8a64916c72e55ad9185511e81712786b381fea18711dd8741593564b02b26110` |
| `tools/architecture-lint/lint.py` |  | 10866 | `59dc7f68e5725574d5dcc82ca5b618de8d6d0ace244d3b627b221222e2a1d481` |
| `tools/coverage-guard.py` |  | 11760 | `72cd433655fd4afa141d6c663216838a1c6dbf45f004c42129230afbb2f85247` |
| `tools/examples_runner.py` |  | 3505 | `3e34e656ad9b0c85e8ba92f64d19257a3e518d33d268be96abe1171fd7e752f4` |

## Rename map (V3 flat numbering → V3.1 `docs/`)

V3 reused numbers 17–29 for two different file sets. V3.1 assigns one number per file, grouped by section. Content was not changed by the move except cross-reference rewrites, removal of de-branding artifacts, and the merge noted below.

| V3 file | V3.1 location |
|---|---|
| `00_MASTER_INDEX.md` | `00_MASTER_INDEX.md` |
| `00_START_HERE_FOR_BUILD_AGENTS.md` | `01_START_HERE_FOR_BUILD_AGENTS.md` |
| `01_AUTHORITY_AND_DECISIONS.md` | `02_AUTHORITY_AND_DECISIONS.md` |
| `23_ARCHITECTURAL_CONFLICTS_AND_SUPERSESSIONS.md` | `03_ARCHITECTURAL_CONFLICTS_AND_SUPERSESSIONS.md` |
| `17_REQUIREMENT_BASIS_AND_LIMITS.md` | `04_REQUIREMENT_BASIS_AND_LIMITS.md` |
| `02_PRODUCT_PRD_AND_UX.md` | `10_PRODUCT_PRD_AND_UX.md` |
| `03_SYSTEM_ARCHITECTURE.md` | `11_SYSTEM_ARCHITECTURE.md` |
| `04_REPOSITORY_AND_MODULE_LAYOUT.md` | `12_REPOSITORY_AND_MODULE_LAYOUT.md` |
| `05_DOMAIN_MODEL_AND_STATE_MACHINES.md` | `13_DOMAIN_MODEL_AND_STATE_MACHINES.md` |
| `06_AGENT_RUNTIME_AND_ORCHESTRATION.md` | `14_AGENT_RUNTIME_AND_ORCHESTRATION.md` |
| `07_MODEL_ROUTER_AND_PROVIDER_GATEWAY.md` | `15_MODEL_ROUTER_AND_PROVIDER_GATEWAY.md` |
| `08_TOOL_CAPABILITY_AND_PROCEDURAL_RUNTIME.md` | `16_TOOL_CAPABILITY_AND_PROCEDURAL_RUNTIME.md` |
| `21_CANONICAL_TOOL_AND_CAPABILITY_INVENTORY.md` | `17_CANONICAL_TOOL_AND_CAPABILITY_INVENTORY.md` |
| `09_CONTEXT_RETRIEVAL_AND_ENGINEERING_KNOWLEDGE.md` | `18_CONTEXT_RETRIEVAL_AND_ENGINEERING_KNOWLEDGE.md` |
| `10_DURABLE_STATE_MEMORY_COMPACTION_CHECKPOINTS.md` | `19_DURABLE_STATE_MEMORY_COMPACTION_CHECKPOINTS.md` |
| `11_WORKSPACE_GIT_AND_TRUSTED_CODE_SURFACE.md` | `20_WORKSPACE_GIT_AND_TRUSTED_CODE_SURFACE.md` |
| `12_TERMINAL_EXECUTION_AND_SANDBOX.md` | `21_TERMINAL_EXECUTION_AND_SANDBOX.md` |
| `13_BROWSER_AND_COMPUTER_USE.md` | `22_BROWSER_AND_COMPUTER_USE.md` |
| `14_SECURITY_POLICY_EFFECT_LEDGER.md` | `23_SECURITY_POLICY_EFFECT_LEDGER.md` |
| `15_CLOUD_CONTROL_PLANE_AND_SYNC.md` | `24_CLOUD_CONTROL_PLANE_AND_SYNC.md` |
| `20_MULTIMODAL_MEDIA_AND_NOTEBOOK_RUNTIME.md` | `25_MULTIMODAL_MEDIA_AND_NOTEBOOK_RUNTIME.md` |
| `19_SKILL_REGISTRY_AND_EVOLUTION.md` | `26_SKILL_REGISTRY_AND_EVOLUTION.md` |
| `17_PROTOCOL_APIS_AND_EVENT_SCHEMAS.md` | `30_PROTOCOL_APIS_AND_EVENT_SCHEMAS.md` |
| `18_DATABASE_AND_STORAGE_SCHEMA.md` | `31_DATABASE_AND_STORAGE_SCHEMA.md` |
| `19_DESKTOP_FRONTEND_IMPLEMENTATION.md` | `32_DESKTOP_FRONTEND_IMPLEMENTATION.md` |
| `20_CORE_AND_CLOUD_BACKEND_IMPLEMENTATION.md` | `33_CORE_AND_CLOUD_BACKEND_IMPLEMENTATION.md` |
| `21_OBSERVABILITY_COST_AND_OPERATIONS_DATA.md` | `34_OBSERVABILITY_COST_AND_OPERATIONS_DATA.md` |
| `27_DEPENDENCY_AND_BINDING_DECISIONS.md` | `35_DEPENDENCY_AND_BINDING_DECISIONS.md` |
| `30_BUILD_BUY_DEPENDENCY_AND_LICENSE_POLICY.md` | `36_BUILD_BUY_DEPENDENCY_AND_LICENSE_POLICY.md` |
| `28_EXISTING_CODE_DONOR_AND_REUSE_POLICY.md` | `37_EXISTING_CODE_DONOR_AND_REUSE_POLICY.md` |
| `29_OLD_REPO_DONOR_MIGRATION_RULES.md` | `37_EXISTING_CODE_DONOR_AND_REUSE_POLICY.md (merged; was a strict subset)` |
| `18_EVIDENCE_DERIVED_REQUIREMENT_LEDGER.md` | `40_EVIDENCE_DERIVED_REQUIREMENT_LEDGER.md` |
| `40_EVIDENCE_DERIVED_IMPLEMENTATION_TASKS.md` | `41_EVIDENCE_DERIVED_IMPLEMENTATION_TASKS.md` |
| `35_EVIDENCE_DERIVED_QUALIFICATION_TEST_MATRIX.md` | `42_EVIDENCE_DERIVED_QUALIFICATION_TEST_MATRIX.md` |
| `27_IMPLEMENTATION_ROADMAP_AND_TASK_GRAPH.md` | `43_IMPLEMENTATION_ROADMAP_AND_TASK_GRAPH.md` |
| `32_REQUIREMENTS_TRACEABILITY_MATRIX.md` | `44_REQUIREMENTS_TRACEABILITY_MATRIX.md` |
| `56_REQUIREMENT_TO_TASK_TO_TEST_TRACEABILITY.md` | `45_REQUIREMENT_TO_TASK_TO_TEST_TRACEABILITY.md` |
| `24_REQUIREMENT_COVERAGE_FREEZE_GATE.md` | `46_REQUIREMENT_COVERAGE_FREEZE_GATE.md` |
| `39_REQUIREMENT_COVERAGE_AUDIT_REPORT.md` | `47_REQUIREMENT_COVERAGE_AUDIT_REPORT.md` |
| `22_FEATURE_DEPTH_CONTRACTS.md` | `48_FEATURE_DEPTH_CONTRACTS.md` |
| `22_TEST_STRATEGY_REAL_SYSTEM_GATES.md` | `50_TEST_STRATEGY_REAL_SYSTEM_GATES.md` |
| `23_E2E_ACCEPTANCE_TEST_CATALOG.md` | `51_E2E_ACCEPTANCE_TEST_CATALOG.md` |
| `24_SECURITY_THREAT_MODEL_AND_TESTS.md` | `52_SECURITY_THREAT_MODEL_AND_TESTS.md` |
| `25_PERFORMANCE_AND_BENCHMARK_PLAN.md` | `53_PERFORMANCE_AND_BENCHMARK_PLAN.md` |
| `49_FAULT_INJECTION_AND_RECOVERY_CATALOG.md` | `54_FAULT_INJECTION_AND_RECOVERY_CATALOG.md` |
| `50_MUTATION_NEGATIVE_AND_CHAOS_TEST_POLICY.md` | `55_MUTATION_NEGATIVE_AND_CHAOS_TEST_POLICY.md` |
| `38_TOOL_CAPABILITY_CONFORMANCE.md` | `56_TOOL_CAPABILITY_CONFORMANCE.md` |
| `36_SKILL_EVOLUTION_REAL_TESTS.md` | `57_SKILL_EVOLUTION_REAL_TESTS.md` |
| `37_MULTIMODAL_MEDIA_REAL_TESTS.md` | `58_MULTIMODAL_MEDIA_REAL_TESTS.md` |
| `33_RELEASE_ZERO_PROOF_SCENARIO.md` | `59_RELEASE_ZERO_PROOF_SCENARIO.md` |
| `42_RELEASE_ZERO_EXPANDED_PROOF.md` | `60_RELEASE_ZERO_EXPANDED_PROOF.md` |
| `26_CI_CD_RELEASE_AND_SUPPLY_CHAIN.md` | `70_CI_CD_RELEASE_AND_SUPPLY_CHAIN.md` |
| `31_OPERATIONS_RUNBOOK.md` | `71_OPERATIONS_RUNBOOK.md` |
| `34_RISK_REGISTER_AND_OPEN_DECISIONS.md` | `72_RISK_REGISTER_AND_OPEN_DECISIONS.md` |
| `54_RELEASE_BLOCKERS_AND_STOP_THE_LINE_RULES.md` | `73_RELEASE_BLOCKERS_AND_STOP_THE_LINE_RULES.md` |
| `55_PACKAGE_INTEGRITY_AND_BUILD_COVERAGE.md` | `74_PACKAGE_INTEGRITY_AND_BUILD_COVERAGE.md` |
| `16_ANTI_SUPERFICIAL_IMPLEMENTATION_STANDARD.md` | `80_ANTI_SUPERFICIAL_IMPLEMENTATION_STANDARD.md` |
| `26_ARCHITECTURE_GUARDRAILS_AND_FORBIDDEN_DUPLICATION.md` | `81_ARCHITECTURE_GUARDRAILS_AND_FORBIDDEN_DUPLICATION.md` |
| `41_NO_PLACEHOLDER_PRODUCTION_EVIDENCE_GATE.md` | `82_NO_PLACEHOLDER_PRODUCTION_EVIDENCE_GATE.md` |
| `28_DEFINITION_OF_DONE_AND_ACCEPTANCE.md` | `83_DEFINITION_OF_DONE_AND_ACCEPTANCE.md` |
| `44_EXISTING_CODE_FEATURE_AUDIT_PROTOCOL.md` | `84_EXISTING_CODE_FEATURE_AUDIT_PROTOCOL.md` |
| `45_AGENT_TASK_EXECUTION_PROTOCOL.md` | `85_AGENT_TASK_EXECUTION_PROTOCOL.md` |
| `46_TASK_CARD_TEMPLATE.md` | `86_TASK_CARD_TEMPLATE.md` |
| `47_HANDOFF_AND_MANIFEST_PROTOCOL.md` | `87_HANDOFF_AND_MANIFEST_PROTOCOL.md` |
| `48_PARALLEL_AGENT_COORDINATION_RULES.md` | `88_PARALLEL_AGENT_COORDINATION_RULES.md` |
| `29_BUILD_AGENT_CONTEXT_LOADING_POLICY.md` | `89_BUILD_AGENT_CONTEXT_LOADING_POLICY.md` |
| `52_PR_CHANGE_EVIDENCE_TEMPLATE.md` | `90_PR_CHANGE_EVIDENCE_TEMPLATE.md` |
| `51_FEATURE_COMPLETION_AUDIT.md` | `91_FEATURE_COMPLETION_AUDIT.md` |
| `25_BUILD_EVIDENCE_AND_DEPENDENCY_MANIFEST.md` | `92_BUILD_EVIDENCE_AND_DEPENDENCY_MANIFEST.md` |
| `(new in V3.1)` | `93_STATUS_VOCABULARY_AND_LIFECYCLE.md` |
| `53_BUILD_MANIFEST.md` | `98_BUILD_MANIFEST.md` |
| `99_MANIFEST.md` | `MANIFEST.md + manifest.json at repository root (covers all files, not only Part 2)` |
