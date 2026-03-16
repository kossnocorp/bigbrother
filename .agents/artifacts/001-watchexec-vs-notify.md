# Watchexec vs Notify+Ignore Research

## Question

Should this crate use `watchexec` (and `watchexec_filterer_ignore`) as the core watcher, or build directly with `notify` + ignore tooling?

## Findings

- `watchexec_filterer_ignore` is a thin wrapper around `ignore_files::IgnoreFilter`.
  - Source: `tmp/watchexec/crates/filterer/ignore/src/lib.rs`
  - It only adapts filtering into Watchexec's `Filterer` trait.
- `watchexec` is a full runtime oriented around action scheduling and command execution.
  - Source: `tmp/watchexec/crates/lib/src/lib.rs`
  - It is powerful, but introduces extra runtime/event machinery that is not needed for this crate's custom tracked/untracked state transitions.

## Decision

Use direct `notify` + `ignore-files` + `ignore` integration.

Why:

1. Better control over custom semantics (tracked/untracked transitions, full rescan on ignore-file changes, special handling of global ignore files).
2. Less glue/indirection than embedding Watchexec runtime.
3. Can still keep the hot path efficient (parallel initial listing via `ignore::WalkBuilder::build_parallel`).

## Notes

No temporary API-probing bin was needed for this decision because docs + local source were sufficient.
