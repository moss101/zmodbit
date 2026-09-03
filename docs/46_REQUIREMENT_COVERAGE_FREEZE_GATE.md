# Requirement Coverage Freeze Gate

This build dossier is frozen only if:

1. exactly 291 evidence-derived rows exist in `40_EVIDENCE_DERIVED_REQUIREMENT_LEDGER.md`;
2. every production row has canonical owner + `IMP-EV-*` task + `QUAL-EV-*` test;
3. every experiment is isolated behind an existing owner and has a measurable exit decision;
4. deferred/rejected rows remain explicit;
5. no external-product feature name is required to understand a build requirement;
6. task/test files use native Modbit contracts;
7. all architectural locks and supersessions are represented;
8. no placeholder/TBD is accepted as a production requirement.

Coverage freeze proves specification completeness, not product completion.
