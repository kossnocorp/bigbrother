# Big Borther

`bigbrother` crate is an async Git-aware file tracker for Rust that watches a working directory and reports files that are currently trackable by ignore rules.

- High-level API.
- Git-aware file watching.
- `.gitignore` and `.ignore` support across hierarchy, including parent directories.
- Global Git ignore support (`core.excludesFile`).
- Per-file transitions when ignore rules change.
- Async API via [`tokio`](https://crates.io/crates/tokio) channel.
-
- Optional source-only mode (`source-filter` feature) that only tracks source files based on [`tokei`](https://crates.io/crates/tokei) language definitions.

It combines:

- [`notify`](https://crates.io/crates/notify) for filesystem watching.
- [`ignore`](https://crates.io/crates/ignore) for fast parallel file discovery.
- [`ignore-files`](https://crates.io/crates/ignore-files) for discovering and compiling ignore files (`.gitignore`, `.ignore`, global git excludes).
- Optional [`tokei`](https://crates.io/crates/tokei) support for source-only filtering


## Installation

To add `bigbrother` to your project, install it with `cargo add`:

```bash
cargo add bigbrother
```

To enable source-only filtering (`only_source` option), use the source-filter feature:

```bash
cargo add bigbrother --features source-filter
```

## How it works

Here's a high-level overview of how Big Brother works:

- Resolves cwd and (if present) containing git repo root.
- If cwd is inside a repo, watches the repo root to catch parent ignore changes.
- Builds ignore state from local + global ignore sources.
- Performs an initial parallel scan, then emits InitialTracked events.
- Processes live filesystem events (`Created`, `Changed`, `Removed`, `Moved`) for tracked files.
- On ignore-file updates, rescans and diffs tracked state to emit `Tracked`/`Untracked`.

## Usage

Basic usage:

```rs
use bigbrother::{
    FileTracker, TrackEvent, TrackEventError, TrackEventFile, TrackEventFileMove, WatchOptions,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (tracker, mut rx) = FileTracker::start(".").await?;

    while let Some(event) = rx.recv().await {
        match event {
            TrackEvent::InitialTracked(TrackEventFile { path }) => println!("initial: {}", path.display()),
            TrackEvent::Tracked(TrackEventFile { path }) => println!("tracked: {}", path.display()),
            TrackEvent::Untracked(TrackEventFile { path }) => println!("untracked: {}", path.display()),
            TrackEvent::Created(TrackEventFile { path }) => println!("created: {}", path.display()),
            TrackEvent::Changed(TrackEventFile { path }) => println!("changed: {}", path.display()),
            TrackEvent::Removed(TrackEventFile { path }) => println!("removed: {}", path.display()),
            TrackEvent::Moved(TrackEventFileMove { from, to }) => {
                println!("moved: {} -> {}", from.display(), to.display())
            }
            TrackEvent::Error(TrackEventError { message }) => eprintln!("error: {message}"),
        }
    }

    tracker.stop().await?;

    Ok(())
}
```

If you only care about source files, enable the `source-filter` feature and set the `only_source` option:

```rs
use bigbrother::{FileTracker, WatchOptions};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let options = WatchOptions {
        only_source: true,
        ..WatchOptions::default()
    };
    let (tracker, mut rx) = FileTracker::start_with_options(".", options).await?;

    // ...

    Ok(())
}
```


## Changelog

See [the changelog](./CHANGELOG.md).

## License

[MIT © Sasha Koss](https://koss.nocorp.me/mit/)
