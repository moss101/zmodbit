# Package Integrity and Build Coverage

## Documentation package checks

- all payload files are `.md`;
- master manifests enumerate every file;
- exactly 291 evidence-derived requirement rows are present;
- every ADOPT/ADAPT row has an implementation task and qualification test;
- no `UNREVIEWED`, `UNKNOWN`, `TBD IMPLEMENTATION`, fake-completion or placeholder acceptance states;
- build-agent docs contain no external-product feature shorthand;
- exact dependency/provider names appear only where implementation binding requires them.

## Product CI coverage checks to implement

CI must parse requirement/task/test metadata and fail when a production requirement has no owner, task or real qualification. It must also fail on production fake adapters, forbidden dependency edges, skipped protected-effect tests and incomplete manifest evidence.
