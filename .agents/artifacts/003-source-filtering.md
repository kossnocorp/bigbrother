# Source-Only Filtering Research

## Goal

Support `only_source: true` without maintaining a custom extension list.

## Options considered

- GitHub Linguist bindings/data crates: not clearly maintained/standardized in Rust ecosystem for this exact use case.
- `tokei`: maintained, broad language map, can infer language from path/extension.

## Decision

Use `tokei::LanguageType::from_path` as the source-file classifier.

Benefits:

1. Large maintained language dataset.
2. Zero manual extension table in this crate.
3. Lightweight runtime call for per-file checks.

Tradeoff:

- "Source" means "recognized language by tokei" (may include some non-code text formats depending on tokei language table).
