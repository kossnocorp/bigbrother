---
run: cat ./.agents/prompts/001-initial.md | opencode run --agent plan
---

I need to create a Rust crate that would do the following:

- Get me a list of files trackable by reading `.gitignore` and `.ignore`.

- In a directory (cwd) that can be nested in a git repo, so that it should look up the hierarchy (for each file, so the ignore files in subdirs would be properly handled too).

- Start watch mode for this directory, notifying me about file changes (create, remove, move).

- When an ignore file changes, it should reevaluate the initial list and see if anything gets tracked now.

- If the cwd is not a git repo but is in a git repo, the watcher must watch up the hierarchy and look for ignore file changes too.

- If previously ignored files get unignored, I must be notified with each.

- Global git ignore (and .ignore if applicable) must be tracked too!

- This must work in parallel to get the maximum efficiency and play well with async code.

---

Some implementation thoughts:

Use the `ignore` crate to maintain the list of ignore files. Make sure to rebuild the ignore matcher when any of the ignore files change.

To make it work, it would have to keep the list of files in memory so when a file gets untracked, it can be notified.

Ignore files content must be in memory too.

Using `ignore` for walking is good too, but ignore matcher must be handled separately to avoid `ignore` reading the ignore files every time.

Use the `notify` crate to watch for file changes. If cwd is inside a git repo, watch the whole repo for changes, but only notify about changes in the cwd. This way we can also track changes in ignore files up the hierarchy.

I feel like global ignore files must be handled separately. If we were to watch ~/ to know about changes in global git ignore, it would probably get us notified about all home dir changes and result in inefficiency. Think about it carefully.

When files get untracked, we must be notified with a corresponding message. The same goes for files that will be tracked after receiving an update about ignore files.

I will be using `async` code with `tokio`. Make sure the crate is designed to work well with async code and can run in parallel where possible.

The code must be as efficient as possible. Here's an example of how I optimized file listing:

```rs
use crate::prelude::*;

pub struct Watcher {}

impl Watcher {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn load(self: Arc<Self>, path: &Path) -> Result<()> {
        let walk = WalkBuilder::new(path).build_parallel();

        let (tx, rx) = bounded::<PathBuf>(256);

        let walker = tokio::task::spawn_blocking(move || {
            walk.run(|| {
                let tx = tx.clone();

                Box::new(move |result| {
                    if let Ok(entry) = result {
                        let path = entry.path().to_path_buf();
                        let _ = tx.send_blocking(path);
                    }
                    ignore::WalkState::Continue
                })
            });
        });

        let workers = available_parallelism()?.get();
        let mut handles = Vec::with_capacity(workers);

        for _ in 0..workers {
            let this = self.clone();
            let rx = rx.clone();

            handles.push(tokio::spawn(async move {
                while let Ok(path) = rx.recv().await {
                    this.load_file(path).await?;
                }
                Ok::<(), anyhow::Error>(())
            }));
        }

        walker.await?;

        drop(rx);

        for handle in handles {
            handle.await??;
        }

        Ok(())
    }

    async fn load_file(&self, path: PathBuf) -> Result<()> {
        // TODO: Load file content
        Ok(())
    }
}
```

It might not be 100% optimal, so try digging into coming up with a better solution if you can.

I don't have a specific end-user API design in mind. I would prefer passing a closure that would be called with files that get tracked/untracked/changed/moved/etc., but getting back a `rx` is also good if that would be more efficient. You can also come up with something else if you think it would be better. Just make sure to explain the design decisions you made and why.

The crate must be well tested. Make sure to write tests for all the functionality and edge cases (i.e., ignore file changes, nested Git repos, global ignore files, etc.). You would probably need to create a temporary directory and spawn Git repos, write some file structure, and manage ignore files to test all the functionality. Make sure to clean up after the tests.

For assertions, use the `pretty_assertions` crate to get better error messages. If you ever need snapshots, use the `insta` crate. When using `insta`, prefer using RON format and inline snapshots to make the tests easier to understand, e.g.,:

```rs
assert_ron_snapshot!(vars, @"
[
  PromptVar(
    span: SpanShape(
      outer: (8, 14),
      inner: (9, 13),
    ),
  ),
]
");
```

For unit tests of the internal functionality, use same-file modules, e.g.,

```rs

#[cfg(test)]
mod tests {
    use super::*;
    use insta::assert_ron_snapshot;

    #[test]
    fn test_something() {
      // ...
    }
}
```

I didn't install any dependencies yet, so feel free to add any via `cargo add` so it installs the latest version.

If you ever need to install `cargo` extensions, i.e., `cargo-insta`, add `cargo-binstall = "latest"` to `mise.toml` (inside the tools section) and install the tool via `"cargo:cargo-insta" = "latest"` in `mise.toml` (same section). `mise install` then add it to the project.

---

I cloned the `ripgrep` repo into `tmp/ripgrep`. You will find the `ignore` crate source code in `tmp/ripgrep/crates/ignore`. Explore the source code to understand how to utilize it. `ripgrep` itself is using `ignore`, so looking up the usage might give some ideas on how to use it.

The crate's struct instance must expose `cwd`, `repo_dir` so I can use it as a crate end-user.

---

Additionally, look for crates or data files on the internet that can help filter source-only files. It can be an option `only_source: true` that would make the crate only track source files (e.g., .rs, .js, .ts, etc.). I would rather not maintain the list of source file extensions myself. That's why you must look for existing solutions. Maybe GitHub has some kind of file extensions library/repo that you can utilize.

---

Before you start implementation, try an alternative approach with the `watchexec` crate (using it as a library) and its extensions `watchexec_filterer_ignore`. It might be more efficient than using `notify` and `ignore` and implementing the logic yourself. To make the judgment, use the criteria (especially around parallelism and efficiency) mentioned in the task description. I `watchexec` into `tmp/watchexec`. You will find `watchexec_filterer_ignore` in `tmp/watchexec/crates/ignore-files`. Explore the source code to understand if you can utilize them and if they would be more efficient than the `notify` + `ignore` approach. If you think they would be better, go with that approach and write a wrapper. Just make sure to explain your decision and how you are utilizing those crates in your implementation.

If the wrapper approach is just a few lines of code, abandon the idea of creating a separate crate and give me a bin example of how I can use it instead of the wrapper crate.

---

When exploring a crate, prefer searching and navigating `https://docs.rs` instead of the local source code, as it would be faster and easier to understand the API. Explore the source code if you can't find the answers in docs.

---

When experimenting with a crate API, create a temporary bin file and write some code to test the APIs. You can delete the file later, but it would be faster to understand the API and how to utilize it.

---

While working, use `.agents/artifacts` and create Markdown files there. For each research topic, create a separate file and write your findings there. If you used temporary bin files to test the APIs, make sure to add it to the corresponding Markdown file as well, so I can see how you utilized the API and what you tried.

Before you start, you must create a plan Markdown file in `.agents/artifacts` and write your plan there with steps as `- [ ] <description>`. Add tasks for each significant step, including searching the internet. Update that plan as you go, marking the progress by checking the boxes (`- [x]`) and adding more steps if needed. This way I can track your progress and understand your implementation plan.
