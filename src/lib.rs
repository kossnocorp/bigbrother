use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use ignore::{Match, WalkBuilder, WalkState};
use ignore_files::{IgnoreFilter, from_environment, from_origin};
use notify::{
    RecommendedWatcher, RecursiveMode, Watcher,
    event::{EventKind, ModifyKind, RenameMode},
};
#[cfg(feature = "source-filter")]
use tokei::LanguageType;
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time,
};

#[derive(Debug, Clone)]
pub struct WatchOptions {
    #[cfg(feature = "source-filter")]
    pub only_source: bool,
    pub app_name: Option<String>,
    pub settle_delay: Duration,
    pub event_channel_capacity: usize,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            #[cfg(feature = "source-filter")]
            only_source: false,
            app_name: None,
            settle_delay: Duration::from_millis(60),
            event_channel_capacity: 1024,
        }
    }
}

impl WatchOptions {
    fn only_source(&self) -> bool {
        #[cfg(feature = "source-filter")]
        {
            self.only_source
        }

        #[cfg(not(feature = "source-filter"))]
        {
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackEvent {
    InitialTracked { path: PathBuf },
    Tracked { path: PathBuf },
    Untracked { path: PathBuf },
    Created { path: PathBuf },
    Changed { path: PathBuf },
    Removed { path: PathBuf },
    Moved { from: PathBuf, to: PathBuf },
    Error { message: String },
}

#[derive(Debug)]
pub struct FileTracker {
    pub cwd: PathBuf,
    pub repo_dir: Option<PathBuf>,
    stop_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

impl FileTracker {
    pub async fn start(
        cwd: impl AsRef<Path>,
        options: WatchOptions,
    ) -> Result<(Self, mpsc::Receiver<TrackEvent>)> {
        let cwd = absolute_path(cwd.as_ref()).context("failed to resolve cwd")?;
        let repo_dir = find_repo_root(&cwd);
        let watch_root = repo_dir.clone().unwrap_or_else(|| cwd.clone());

        let state = TrackerState::new(cwd.clone(), watch_root, options).await?;

        let (events_tx, events_rx) = mpsc::channel(state.options.event_channel_capacity);
        for rel in sorted_rel_paths(&state.cwd, &state.tracked) {
            let _ = events_tx
                .send(TrackEvent::InitialTracked { path: rel })
                .await;
        }

        let (stop_tx, stop_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            if let Err(err) = run_tracker_loop(state, events_tx, stop_rx, ready_tx).await {
                eprintln!("tracker loop failed: {err:#}");
            }
        });

        let _ = ready_rx.await;

        Ok((
            Self {
                cwd,
                repo_dir,
                stop_tx: Some(stop_tx),
                task: Some(task),
            },
            events_rx,
        ))
    }

    pub async fn stop(mut self) -> Result<()> {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        Ok(())
    }
}

impl Drop for FileTracker {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

#[derive(Debug)]
struct TrackerState {
    cwd: PathBuf,
    watch_root: PathBuf,
    options: WatchOptions,
    ignore_filter: IgnoreFilter,
    tracked: HashSet<PathBuf>,
    ignore_files: HashSet<PathBuf>,
}

impl TrackerState {
    async fn new(cwd: PathBuf, watch_root: PathBuf, options: WatchOptions) -> Result<Self> {
        let (ignore_filter, ignore_files) =
            build_filter(&watch_root, options.app_name.as_deref()).await?;
        let tracked =
            scan_tracked_files(&cwd, &watch_root, &ignore_filter, options.only_source()).await?;

        Ok(Self {
            cwd,
            watch_root,
            options,
            ignore_filter,
            tracked,
            ignore_files,
        })
    }

    async fn rebuild(&mut self) -> Result<(HashSet<PathBuf>, HashSet<PathBuf>)> {
        let old = self.tracked.clone();
        let (ignore_filter, ignore_files) =
            build_filter(&self.watch_root, self.options.app_name.as_deref()).await?;
        let tracked = scan_tracked_files(
            &self.cwd,
            &self.watch_root,
            &ignore_filter,
            self.options.only_source(),
        )
        .await?;

        self.ignore_filter = ignore_filter;
        self.ignore_files = ignore_files;
        self.tracked = tracked.clone();

        Ok((old, tracked))
    }
}

async fn run_tracker_loop(
    mut state: TrackerState,
    events_tx: mpsc::Sender<TrackEvent>,
    mut stop_rx: oneshot::Receiver<()>,
    ready_tx: oneshot::Sender<()>,
) -> Result<()> {
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();

    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |res| {
        let _ = raw_tx.send(res);
    })
    .context("failed to create notify watcher")?;

    watcher
        .watch(&state.watch_root, RecursiveMode::Recursive)
        .with_context(|| format!("failed to watch {}", state.watch_root.display()))?;

    let mut aux_watches = HashSet::new();
    refresh_aux_watches(&state, &mut watcher, &mut aux_watches)?;
    let _ = ready_tx.send(());

    loop {
        tokio::select! {
            _ = &mut stop_rx => {
                break;
            }
            maybe_event = raw_rx.recv() => {
                let Some(event_result) = maybe_event else {
                    break;
                };

                match event_result {
                    Ok(event) => {
                        if event.paths.iter().any(|p| is_ignore_related_path(p, &state)) {
                            time::sleep(state.options.settle_delay).await;
                            match state.rebuild().await {
                                Ok((old, new)) => {
                                    emit_diff_events(&events_tx, &state.cwd, &old, &new).await;
                                    if let Err(err) = refresh_aux_watches(&state, &mut watcher, &mut aux_watches) {
                                        let _ = events_tx.send(TrackEvent::Error { message: err.to_string() }).await;
                                    }
                                }
                                Err(err) => {
                                    let _ = events_tx.send(TrackEvent::Error { message: err.to_string() }).await;
                                }
                            }
                            continue;
                        }

                        process_fs_event(&mut state, &events_tx, event).await;
                    }
                    Err(err) => {
                        let _ = events_tx.send(TrackEvent::Error { message: err.to_string() }).await;
                    }
                }
            }
        }
    }

    Ok(())
}

async fn process_fs_event(
    state: &mut TrackerState,
    events_tx: &mpsc::Sender<TrackEvent>,
    event: notify::Event,
) {
    if matches!(
        event.kind,
        EventKind::Modify(ModifyKind::Name(RenameMode::Both))
    ) && event.paths.len() >= 2
    {
        let from = absolute_or_join(&state.watch_root, &event.paths[0]);
        let to = absolute_or_join(&state.watch_root, &event.paths[1]);
        handle_rename(state, events_tx, &from, &to).await;
        return;
    }

    for path in event.paths {
        let abs = absolute_or_join(&state.watch_root, &path);
        if !abs.starts_with(&state.cwd) {
            continue;
        }

        match event.kind {
            EventKind::Remove(_) => {
                if state.tracked.remove(&abs) {
                    let _ = events_tx
                        .send(TrackEvent::Removed {
                            path: to_relative(&state.cwd, &abs),
                        })
                        .await;
                }
            }
            EventKind::Create(_) => {
                if is_tracked_file(
                    &state.cwd,
                    &abs,
                    &state.ignore_filter,
                    state.options.only_source(),
                ) && state.tracked.insert(abs.clone())
                {
                    let _ = events_tx
                        .send(TrackEvent::Created {
                            path: to_relative(&state.cwd, &abs),
                        })
                        .await;
                }
            }
            EventKind::Modify(_) => {
                if state.tracked.contains(&abs) {
                    let _ = events_tx
                        .send(TrackEvent::Changed {
                            path: to_relative(&state.cwd, &abs),
                        })
                        .await;
                } else if is_tracked_file(
                    &state.cwd,
                    &abs,
                    &state.ignore_filter,
                    state.options.only_source(),
                ) && state.tracked.insert(abs.clone())
                {
                    let _ = events_tx
                        .send(TrackEvent::Created {
                            path: to_relative(&state.cwd, &abs),
                        })
                        .await;
                }
            }
            _ => {}
        }
    }
}

async fn handle_rename(
    state: &mut TrackerState,
    events_tx: &mpsc::Sender<TrackEvent>,
    from: &Path,
    to: &Path,
) {
    let was_tracked = state.tracked.remove(from);
    let now_tracked = is_tracked_file(
        &state.cwd,
        to,
        &state.ignore_filter,
        state.options.only_source(),
    );

    if now_tracked {
        state.tracked.insert(to.to_path_buf());
    }

    match (was_tracked, now_tracked) {
        (true, true) => {
            let _ = events_tx
                .send(TrackEvent::Moved {
                    from: to_relative(&state.cwd, from),
                    to: to_relative(&state.cwd, to),
                })
                .await;
        }
        (true, false) => {
            let _ = events_tx
                .send(TrackEvent::Removed {
                    path: to_relative(&state.cwd, from),
                })
                .await;
        }
        (false, true) => {
            let _ = events_tx
                .send(TrackEvent::Created {
                    path: to_relative(&state.cwd, to),
                })
                .await;
        }
        (false, false) => {}
    }
}

async fn emit_diff_events(
    events_tx: &mpsc::Sender<TrackEvent>,
    cwd: &Path,
    old: &HashSet<PathBuf>,
    new: &HashSet<PathBuf>,
) {
    let mut tracked_now = BTreeSet::new();
    let mut untracked_now = BTreeSet::new();

    for path in new {
        if !old.contains(path) {
            tracked_now.insert(path.clone());
        }
    }
    for path in old {
        if !new.contains(path) {
            untracked_now.insert(path.clone());
        }
    }

    for path in tracked_now {
        let _ = events_tx
            .send(TrackEvent::Tracked {
                path: to_relative(cwd, &path),
            })
            .await;
    }
    for path in untracked_now {
        let _ = events_tx
            .send(TrackEvent::Untracked {
                path: to_relative(cwd, &path),
            })
            .await;
    }
}

fn refresh_aux_watches(
    state: &TrackerState,
    watcher: &mut RecommendedWatcher,
    aux_watches: &mut HashSet<PathBuf>,
) -> Result<()> {
    let mut wanted = HashSet::new();

    for ignore_file in &state.ignore_files {
        if ignore_file.starts_with(&state.watch_root) {
            continue;
        }
        if ignore_file.exists() {
            wanted.insert(ignore_file.clone());
        } else if let Some(parent) = ignore_file.parent() {
            wanted.insert(parent.to_path_buf());
        }
    }

    for path in aux_watches.clone() {
        if !wanted.contains(&path) {
            let _ = watcher.unwatch(&path);
            aux_watches.remove(&path);
        }
    }

    for path in wanted {
        if aux_watches.contains(&path) {
            continue;
        }
        watcher
            .watch(&path, RecursiveMode::NonRecursive)
            .with_context(|| format!("failed to watch aux path {}", path.display()))?;
        aux_watches.insert(path);
    }

    Ok(())
}

fn is_ignore_related_path(path: &Path, state: &TrackerState) -> bool {
    let abs = absolute_or_join(&state.watch_root, path);
    if state.ignore_files.contains(&abs) {
        return true;
    }

    if abs
        .file_name()
        .is_some_and(|name| name == ".ignore" || name == ".gitignore" || name == ".hgignore")
    {
        return true;
    }

    if abs.to_string_lossy().ends_with(&format!(
        "{0}.git{0}info{0}exclude",
        std::path::MAIN_SEPARATOR
    )) {
        return true;
    }

    abs.file_name().is_some_and(|n| n == "config")
        && abs
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|n| n == ".git")
}

async fn build_filter(
    watch_root: &Path,
    app_name: Option<&str>,
) -> Result<(IgnoreFilter, HashSet<PathBuf>)> {
    let watch_root = watch_root.to_path_buf();
    let app_name = app_name.map(ToOwned::to_owned);

    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build current-thread runtime for ignore rebuild")?;

        rt.block_on(async move {
            let (mut local, _) = from_origin(watch_root.as_path()).await;
            let (mut global, _) = from_environment(app_name.as_deref()).await;
            local.append(&mut global);

            let ignore_files: HashSet<PathBuf> = local.iter().map(|f| f.path.clone()).collect();
            let filter = IgnoreFilter::new(&watch_root, &local).await?;
            Ok::<(IgnoreFilter, HashSet<PathBuf>), anyhow::Error>((filter, ignore_files))
        })
    })
    .await
    .context("ignore rebuild blocking task panicked")?
}

async fn scan_tracked_files(
    cwd: &Path,
    watch_root: &Path,
    filter: &IgnoreFilter,
    only_source: bool,
) -> Result<HashSet<PathBuf>> {
    let root = watch_root.to_path_buf();

    let candidates: Vec<PathBuf> = tokio::task::spawn_blocking(move || {
        let mut builder = WalkBuilder::new(&root);
        builder
            .standard_filters(false)
            .parents(false)
            .ignore(false)
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .hidden(false);

        let walk = builder.build_parallel();
        let files = Arc::new(Mutex::new(Vec::new()));

        walk.run(|| {
            let files = files.clone();
            Box::new(move |entry| {
                if let Ok(entry) = entry
                    && entry.file_type().is_some_and(|ft| ft.is_file())
                    && let Ok(mut lock) = files.lock()
                {
                    lock.push(entry.into_path());
                }
                WalkState::Continue
            })
        });

        if let Ok(mutex) = Arc::try_unwrap(files) {
            mutex.into_inner().unwrap_or_default()
        } else {
            Vec::new()
        }
    })
    .await
    .context("parallel scan task panicked")?;

    let mut tracked = HashSet::new();
    for path in candidates {
        if is_tracked_file(cwd, &path, filter, only_source) {
            tracked.insert(path);
        }
    }

    Ok(tracked)
}

fn is_tracked_file(cwd: &Path, path: &Path, filter: &IgnoreFilter, only_source: bool) -> bool {
    if !path.starts_with(cwd) {
        return false;
    }

    if !path.is_file() {
        return false;
    }

    if path.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|part| {
            matches!(
                part,
                ".git" | ".hg" | ".svn" | "_darcs" | ".bzr" | ".fossil-settings"
            )
        })
    }) {
        return false;
    }

    if only_source && !is_source_file(path) {
        return false;
    }

    match filter.match_path(path, false) {
        Match::None | Match::Whitelist(_) => true,
        Match::Ignore(glob) => !glob.from().is_none_or(|f| path.starts_with(f)),
    }
}

fn is_source_file(path: &Path) -> bool {
    #[cfg(feature = "source-filter")]
    {
        LanguageType::from_path(path, &tokei::Config::default()).is_some()
    }

    #[cfg(not(feature = "source-filter"))]
    {
        let _ = path;
        false
    }
}

fn to_relative(cwd: &Path, abs: &Path) -> PathBuf {
    abs.strip_prefix(cwd).unwrap_or(abs).to_path_buf()
}

fn sorted_rel_paths(cwd: &Path, paths: &HashSet<PathBuf>) -> Vec<PathBuf> {
    let mut ordered: BTreeSet<PathBuf> = BTreeSet::new();
    for path in paths {
        ordered.insert(to_relative(cwd, path));
    }
    ordered.into_iter().collect()
}

fn absolute_or_join(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let git_marker = dir.join(".git");
        if git_marker.exists() {
            return Some(dir.to_path_buf());
        }
        current = dir.parent();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeSet, fs, time::Duration};

    use git2::Repository;
    #[cfg(feature = "source-filter")]
    use insta::assert_ron_snapshot;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use tokio::time::timeout;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    fn git_init(path: &Path) -> Repository {
        Repository::init(path).unwrap()
    }

    async fn recv_until(
        rx: &mut mpsc::Receiver<TrackEvent>,
        predicate: impl Fn(&TrackEvent) -> bool,
    ) -> Option<TrackEvent> {
        let deadline = Duration::from_secs(5);
        timeout(deadline, async {
            loop {
                let ev = rx.recv().await?;
                if predicate(&ev) {
                    return Some(ev);
                }
            }
        })
        .await
        .ok()
        .flatten()
    }

    #[test]
    fn test_find_repo_root_from_nested_dir() {
        let tmp = TempDir::new().unwrap();
        let _repo = git_init(tmp.path());

        let nested = tmp.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        let found = find_repo_root(&nested).unwrap();
        assert_eq!(found, tmp.path().to_path_buf());
    }

    #[cfg(feature = "source-filter")]
    #[test]
    fn test_only_source_with_tokei() {
        let values = vec![
            is_source_file(Path::new("src/main.rs")),
            is_source_file(Path::new("web/app.ts")),
            is_source_file(Path::new("docs/readme.unknownext")),
        ];

        assert_ron_snapshot!(values, @"
        [
          true,
          true,
          false,
        ]
        ");
    }

    #[cfg(not(feature = "source-filter"))]
    #[test]
    fn test_source_filter_disabled_behavior() {
        assert!(!is_source_file(Path::new("src/main.rs")));
    }

    #[tokio::test]
    async fn test_ignore_change_unignores_file() {
        let tmp = TempDir::new().unwrap();
        let _repo = git_init(tmp.path());

        write(&tmp.path().join(".gitignore"), "ignored.txt\n");
        write(&tmp.path().join("ignored.txt"), "hello");
        write(&tmp.path().join("tracked.rs"), "fn main() {}\n");

        let (tracker, mut rx) = FileTracker::start(tmp.path(), WatchOptions::default())
            .await
            .unwrap();

        let mut initial = BTreeSet::new();
        while let Ok(Some(ev)) = timeout(Duration::from_millis(50), rx.recv()).await {
            if let TrackEvent::InitialTracked { path } = ev {
                initial.insert(path);
            } else {
                break;
            }
        }

        assert!(initial.contains(&PathBuf::from("tracked.rs")));
        assert!(!initial.contains(&PathBuf::from("ignored.txt")));

        write(&tmp.path().join(".gitignore"), "");

        let tracked = recv_until(
            &mut rx,
            |ev| matches!(ev, TrackEvent::Tracked { path } if path == Path::new("ignored.txt")),
        )
        .await;
        assert!(tracked.is_some());

        tracker.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_nested_cwd_observes_repo_root_ignore_changes() {
        let tmp = TempDir::new().unwrap();
        let _repo = git_init(tmp.path());

        write(&tmp.path().join(".gitignore"), "");
        write(&tmp.path().join("app/a.log"), "x\n");
        write(&tmp.path().join("app/main.rs"), "fn main() {}\n");

        let (tracker, mut rx) = FileTracker::start(tmp.path().join("app"), WatchOptions::default())
            .await
            .unwrap();

        write(&tmp.path().join(".gitignore"), "*.log\n");

        let untracked = recv_until(
            &mut rx,
            |ev| matches!(ev, TrackEvent::Untracked { path } if path == Path::new("a.log")),
        )
        .await;
        assert!(untracked.is_some());

        write(&tmp.path().join(".gitignore"), "");

        let tracked = recv_until(
            &mut rx,
            |ev| matches!(ev, TrackEvent::Tracked { path } if path == Path::new("a.log")),
        )
        .await;
        assert!(tracked.is_some());

        tracker.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_global_ignore_via_core_excludes_file_changes() {
        let tmp = TempDir::new().unwrap();
        let repo = git_init(tmp.path());

        let excludes = tmp.path().join("global-excludes");
        write(&excludes, "global.txt\n");
        let mut config = repo.config().unwrap();
        config
            .set_str("core.excludesFile", &excludes.to_string_lossy())
            .unwrap();

        write(&tmp.path().join("global.txt"), "x\n");

        let (tracker, mut rx) = FileTracker::start(tmp.path(), WatchOptions::default())
            .await
            .unwrap();

        write(&excludes, "");

        let tracked = recv_until(
            &mut rx,
            |ev| matches!(ev, TrackEvent::Tracked { path } if path == Path::new("global.txt")),
        )
        .await;
        assert!(tracked.is_some());

        tracker.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_create_remove_and_move_events() {
        let tmp = TempDir::new().unwrap();
        let _repo = git_init(tmp.path());

        write(&tmp.path().join("main.rs"), "fn main() {}\n");

        let (tracker, mut rx) = FileTracker::start(tmp.path(), WatchOptions::default())
            .await
            .unwrap();

        write(&tmp.path().join("new.rs"), "pub fn x() {}\n");
        let created = recv_until(
            &mut rx,
            |ev| matches!(ev, TrackEvent::Created { path } if path == Path::new("new.rs")),
        )
        .await;
        assert!(created.is_some());

        fs::rename(tmp.path().join("new.rs"), tmp.path().join("moved.rs")).unwrap();
        let moved = recv_until(
            &mut rx,
            |ev| {
                matches!(ev, TrackEvent::Moved { from, to } if from == Path::new("new.rs") && to == Path::new("moved.rs"))
            },
        )
        .await;
        assert!(moved.is_some());

        fs::remove_file(tmp.path().join("moved.rs")).unwrap();
        let removed = recv_until(
            &mut rx,
            |ev| matches!(ev, TrackEvent::Removed { path } if path == Path::new("moved.rs")),
        )
        .await;
        assert!(removed.is_some());

        tracker.stop().await.unwrap();
    }
}
