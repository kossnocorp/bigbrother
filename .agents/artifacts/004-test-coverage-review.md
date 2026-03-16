# Test Coverage Review Against Original Requirements

## Newly covered in this iteration

- Nested `cwd` test now checks both directions of ignore transitions at repo root:
  - adding pattern causes `Untracked`,
  - removing pattern causes `Tracked`.
- Added explicit filesystem event test for `Created`, `Moved`, `Removed` notifications.
- `only_source` is feature-gated and now tested in both build modes:
  - without feature: `only_source=true` errors,
  - with feature: source classification works.
- Tests now use `git2` for git repository setup and config writes.

## Covered requirements status

- Initial tracked listing with `.gitignore`/`.ignore` semantics: covered.
- Nested cwd in git repo and parent ignore changes: covered.
- Re-evaluate and emit tracked/untracked on ignore file change: covered.
- Global git ignore (`core.excludesFile`) changes: covered.
- Create/remove/move notifications: covered.
- Expose `cwd` and `repo_dir`: implemented (API), indirectly exercised.

## Remaining notable gaps (still worth adding)

- Nested git repo boundary behavior (submodule-like or inner repo with its own `.git`): not explicitly tested.
- App-specific global `.ignore` via `WatchOptions::app_name` path discovery: not explicitly tested.
- Ignore file rename/delete edge cases (not just content modify) for global and local ignore files: not explicitly tested.
- Stress/throughput behavior under high event volume and parallel listing scaling: not benchmark-tested.
