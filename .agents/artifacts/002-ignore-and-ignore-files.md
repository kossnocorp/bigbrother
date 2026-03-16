# Ignore Handling Research

## `ignore` crate highlights

- `WalkBuilder` supports:
  - parent ignore traversal (`parents(true)`),
  - `.ignore` and `.gitignore` loading,
  - global git ignore (`git_global(true)`),
  - high-performance parallel walking (`build_parallel`).
- Important methods reviewed in:
  - `tmp/ripgrep/crates/ignore/src/walk.rs`

## `ignore-files` crate highlights

- Designed for discovering ignore files and maintaining a mutable compiled filter.
- `from_origin(...)` discovers ignore files under an origin (including subtree ignore files).
- `from_environment(...)` discovers runtime global ignores (git global + app-specific ignore file locations).
- `IgnoreFilter::new(...)` loads file contents and compiles globs; `match_path(...)` checks path matching.
- Relevant files:
  - `tmp/watchexec/crates/ignore-files/src/discover.rs`
  - `tmp/watchexec/crates/ignore-files/src/filter.rs`

## Design implications for this crate

- Use `ignore::WalkBuilder::build_parallel` for initial indexing speed.
- Use a separately maintained matcher state based on `ignore-files` so ignore rules can be rebuilt when ignore files change.
- Keep set of known ignore files in memory and trigger rebuild + tracked-set diff when one changes.

## Edge conditions identified

- If `cwd` is inside a git repo, watch the repo root so ignore changes above `cwd` are observed.
- Global ignore files should be watched directly (or via parent dir if absent), not by recursively watching `$HOME`.
