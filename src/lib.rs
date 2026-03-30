use std::{
    collections::{BTreeSet, HashMap, HashSet, VecDeque},
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
use notify_debouncer_full::{
    DebounceEventResult, Debouncer, FileIdCache, RecommendedCache, new_debouncer,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
    time,
};

pub mod prelude;
use prelude::*;

mod path;
mod track;

#[cfg(feature = "source-filter")]
mod source;

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
            settle_delay: Duration::from_millis(50),
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

#[derive(Debug)]
pub struct FileTracker {
    path: ProjectPath,
    command_tx: mpsc::Sender<TrackerCommand>,
    stop_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug)]
enum TrackerCommand {
    IsTracked {
        path: AbsPath,
        reply: oneshot::Sender<bool>,
    },
}

impl FileTracker {
    pub async fn start(cwd: impl AsRef<Path>) -> Result<(Self, mpsc::Receiver<TrackEvent>)> {
        Self::start_with_options(cwd, WatchOptions::default()).await
    }

    pub async fn start_with_options(
        cwd: impl AsRef<Path>,
        options: WatchOptions,
    ) -> Result<(Self, mpsc::Receiver<TrackEvent>)> {
        let path = ProjectPath::find(cwd.as_ref())?;

        let state = TrackerState::new(path.clone(), options.clone()).await?;

        let (events_tx, events_rx) = mpsc::channel(state.options.event_channel_capacity);
        let ordered: BTreeSet<&AbsPath> = state.tracked.iter().collect();
        for path in ordered {
            let event_file = TrackEventFile::from_path(path);

            let _ = events_tx.send(TrackEvent::InitialTracked(event_file)).await;
        }

        let (stop_tx, stop_rx) = oneshot::channel();
        let (command_tx, command_rx) = mpsc::channel(32);
        let (ready_tx, ready_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            if let Err(err) =
                run_tracker_loop(state, events_tx, command_rx, stop_rx, ready_tx).await
            {
                eprintln!("tracker loop failed: {err:#}");
            }
        });

        let _ = ready_rx.await;

        Ok((
            Self {
                path,
                command_tx,
                stop_tx: Some(stop_tx),
                task: Some(task),
            },
            events_rx,
        ))
    }

    pub async fn is_tracked<P: AsRef<Path>>(&self, path: P) -> Result<bool> {
        if let Some(abs_path) = self.path.abs_path_within_cwd(path) {
            let (reply_tx, reply_rx) = oneshot::channel();

            self.command_tx
                .send(TrackerCommand::IsTracked {
                    path: abs_path,
                    reply: reply_tx,
                })
                .await
                .context("tracker is not running")?;

            reply_rx
                .await
                .context("tracker task stopped before replying")
        } else {
            Ok(false)
        }
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
    project_path: ProjectPath,
    options: WatchOptions,
    ignore_filter: IgnoreFilter,
    tracked: HashSet<AbsPath>,
    ignore_files: HashSet<AbsPath>,
    segment_readiness: HashMap<AbsDirPath, SegmentReadiness>,
    pending_rename_from: VecDeque<AbsPath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentReadiness {
    Ready,
    Dirty,
}

impl TrackerState {
    async fn new(project_path: ProjectPath, options: WatchOptions) -> Result<Self> {
        let repo_path = project_path.repo_path().clone();
        let (ignore_filter, ignore_files) =
            build_filter(&repo_path, options.app_name.as_deref()).await?;
        let tracked =
            scan_tracked_files(&project_path, &ignore_filter, options.only_source()).await?;

        Ok(Self {
            project_path,
            options,
            ignore_filter,
            tracked,
            ignore_files,
            segment_readiness: HashMap::from([(
                repo_path.abs_dir_path().clone(),
                SegmentReadiness::Ready,
            )]),
            pending_rename_from: VecDeque::new(),
        })
    }

    async fn rebuild(&mut self) -> Result<(HashSet<AbsPath>, HashSet<AbsPath>)> {
        let old = self.tracked.clone();
        let (ignore_filter, ignore_files) = build_filter(
            &self.project_path.repo_path(),
            self.options.app_name.as_deref(),
        )
        .await?;
        let tracked = scan_tracked_files(
            &self.project_path,
            &ignore_filter,
            self.options.only_source(),
        )
        .await?;

        self.ignore_filter = ignore_filter;
        self.ignore_files = ignore_files;
        self.tracked = tracked.clone();
        for readiness in self.segment_readiness.values_mut() {
            *readiness = SegmentReadiness::Ready;
        }
        self.pending_rename_from.clear();

        Ok((old, tracked))
    }

    fn needs_rebuild_for_query(&self, path: &AbsPath) -> bool {
        for segment in self.project_path.dir_segments_to(path) {
            if self
                .segment_readiness
                .get(&segment)
                .is_none_or(|state| *state != SegmentReadiness::Ready)
            {
                return true;
            }
        }

        false
    }

    fn mark_path_ready(&mut self, path: &AbsPath) {
        for segment in self.project_path.dir_segments_to(path) {
            self.segment_readiness
                .insert(segment, SegmentReadiness::Ready);
        }
    }

    fn mark_dirty_for_ignore_change(&mut self, path: &Path) {
        if let Some(abs) = self.project_path.abs_path_within_repo(path) {
            if abs.is_git_exclude_file() || abs.is_git_config_file() {
                for readiness in self.segment_readiness.values_mut() {
                    *readiness = SegmentReadiness::Dirty;
                }
                return;
            }

            let Some(parent) = abs.parent() else {
                return;
            };

            for (segment, readiness) in &mut self.segment_readiness {
                if parent.is_or_contains(segment) {
                    *readiness = SegmentReadiness::Dirty;
                }
            }
        }
    }
}

async fn run_tracker_loop(
    mut state: TrackerState,
    events_tx: mpsc::Sender<TrackEvent>,
    mut command_rx: mpsc::Receiver<TrackerCommand>,
    mut stop_rx: oneshot::Receiver<()>,
    ready_tx: oneshot::Sender<()>,
) -> Result<()> {
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel();

    let mut watcher: Debouncer<RecommendedWatcher, RecommendedCache> = new_debouncer(
        Duration::from_millis(30),
        None,
        move |res: DebounceEventResult| {
            let _ = raw_tx.send(res);
        },
    )
    .context("failed to create notify debouncer")?;

    let repo_path_ref = state.project_path.repo_path().path_buf();
    watcher
        .watch(repo_path_ref, RecursiveMode::Recursive)
        .with_context(|| format!("failed to watch {}", repo_path_ref.display()))?;

    let mut aux_watches = HashSet::new();
    refresh_aux_watches(&state, &mut watcher, &mut aux_watches)?;
    let _ = ready_tx.send(());

    loop {
        tokio::select! {
            _ = &mut stop_rx => {
                break;
            }
            maybe_command = command_rx.recv() => {
                match maybe_command {
                    Some(TrackerCommand::IsTracked { path, reply }) => {
                        time::sleep(state.options.settle_delay).await;

                        while let Ok(debounced_result) = raw_rx.try_recv() {
                            handle_debounced_result(
                                &mut state,
                                &events_tx,
                                &mut watcher,
                                &mut aux_watches,
                                debounced_result,
                            )
                            .await;
                        }

                        if state.needs_rebuild_for_query(&path)
                            && let Ok((old, new)) = state.rebuild().await
                        {
                            emit_diff_events(&events_tx,  &old, &new).await;
                            if let Err(err) = refresh_aux_watches(&state, &mut watcher, &mut aux_watches) {
                                let _ = events_tx.send(TrackEvent::Error(TrackEventError { message: err.to_string() })).await;
                            }
                        }

                        state.mark_path_ready(&path);

                        let _ = reply.send(state.tracked.contains(&path));
                    }
                    None => {
                        // No more command senders; continue serving filesystem events.
                    }
                }
            }
            maybe_batch = raw_rx.recv() => {
                let Some(debounced_result) = maybe_batch else {
                    break;
                };
                handle_debounced_result(
                    &mut state,
                    &events_tx,
                    &mut watcher,
                    &mut aux_watches,
                    debounced_result,
                )
                .await;
            }
        }
    }

    Ok(())
}

async fn handle_debounced_result<W: WatchControl>(
    state: &mut TrackerState,
    events_tx: &mpsc::Sender<TrackEvent>,
    watcher: &mut W,
    aux_watches: &mut HashSet<AbsPath>,
    debounced_result: DebounceEventResult,
) {
    match debounced_result {
        Ok(events) => {
            let mut touched_paths = HashSet::new();
            let mut saw_ignore_related_change = false;

            for debounced_event in events {
                let event = debounced_event.event;
                if matches!(event.kind, EventKind::Access(_)) {
                    continue;
                }

                for path in &event.paths {
                    if let Some(abs) = state.project_path.abs_path_within_repo(path) {
                        if abs.is_or_within(&state.project_path.cwd_path()) {
                            touched_paths.insert(abs.clone());
                        }

                        if state.ignore_files.contains(&abs) || abs.is_ignore_related() {
                            state.mark_dirty_for_ignore_change(path);
                            saw_ignore_related_change = true;
                        }
                    }
                }

                process_fs_event(state, events_tx, event).await;
            }

            flush_pending_rename_from(state, events_tx).await;

            if saw_ignore_related_change {
                time::sleep(state.options.settle_delay).await;
                match state.rebuild().await {
                    Ok((old, new)) => {
                        emit_diff_events_filtered(events_tx, &old, &new, &touched_paths).await;
                        if let Err(err) = refresh_aux_watches(state, watcher, aux_watches) {
                            let _ = events_tx
                                .send(TrackEvent::Error(TrackEventError {
                                    message: err.to_string(),
                                }))
                                .await;
                        }
                    }
                    Err(err) => {
                        let _ = events_tx
                            .send(TrackEvent::Error(TrackEventError {
                                message: err.to_string(),
                            }))
                            .await;
                    }
                }
            }
        }

        Err(errors) => {
            for err in errors {
                let _ = events_tx
                    .send(TrackEvent::Error(TrackEventError {
                        message: err.to_string(),
                    }))
                    .await;
            }
        }
    }
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
        state.pending_rename_from.clear();
        let from = state.project_path.abs_path_within_repo(&event.paths[0]);
        let to = state.project_path.abs_path_within_repo(&event.paths[1]);
        if let Some(from) = from
            && let Some(to) = to
        {
            handle_rename(state, events_tx, &from, &to).await;
        }
        return;
    }

    for path in event.paths {
        if let Some(abs_path) = state.project_path.abs_path_within_repo(&path) {
            match event.kind {
                EventKind::Remove(_) => {
                    if state.tracked.remove(&abs_path) {
                        let event_file = TrackEventFile::from_path(&abs_path);
                        let _ = events_tx.send(TrackEvent::Removed(event_file)).await;
                    }
                }

                EventKind::Create(_) => {
                    if is_tracked_file(
                        &state.project_path,
                        &abs_path,
                        &state.ignore_filter,
                        state.options.only_source(),
                    ) && state.tracked.insert(abs_path.clone())
                    {
                        let event_file = TrackEventFile::from_path(&abs_path);
                        let _ = events_tx.send(TrackEvent::Created(event_file)).await;
                    }
                }

                EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                    if state.tracked.contains(&abs_path) {
                        state.pending_rename_from.push_back(abs_path);
                    }
                }

                EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                    if let Some(from) = state.pending_rename_from.pop_front() {
                        handle_rename(state, events_tx, &from, &abs_path).await;
                    } else if is_tracked_file(
                        &state.project_path,
                        &abs_path,
                        &state.ignore_filter,
                        state.options.only_source(),
                    ) && state.tracked.insert(abs_path.clone())
                    {
                        let event_file = TrackEventFile::from_path(&abs_path);
                        let _ = events_tx.send(TrackEvent::Created(event_file)).await;
                    }
                }

                EventKind::Modify(_) => {
                    if state.tracked.contains(&abs_path) {
                        let event_file = TrackEventFile::from_path(&abs_path);
                        let _ = events_tx.send(TrackEvent::Changed(event_file)).await;
                    } else if is_tracked_file(
                        &state.project_path,
                        &abs_path,
                        &state.ignore_filter,
                        state.options.only_source(),
                    ) && state.tracked.insert(abs_path.clone())
                    {
                        let event_file = TrackEventFile::from_path(&abs_path);
                        let _ = events_tx.send(TrackEvent::Created(event_file)).await;
                    }
                }

                _ => {}
            }
        }
    }
}

async fn flush_pending_rename_from(state: &mut TrackerState, events_tx: &mpsc::Sender<TrackEvent>) {
    while let Some(from) = state.pending_rename_from.pop_front() {
        if state.tracked.remove(&from) {
            let event_file = TrackEventFile::from_path(&from);
            let _ = events_tx.send(TrackEvent::Removed(event_file)).await;
        }
    }
}

async fn handle_rename(
    state: &mut TrackerState,
    events_tx: &mpsc::Sender<TrackEvent>,
    from: &AbsPath,
    to: &AbsPath,
) {
    if from == to {
        return;
    }

    let was_tracked = state.tracked.remove(from);
    let now_tracked = is_tracked_file(
        &state.project_path,
        to,
        &state.ignore_filter,
        state.options.only_source(),
    );

    if now_tracked {
        state.tracked.insert(to.clone());
    }

    match (was_tracked, now_tracked) {
        (true, true) => {
            let _ = events_tx
                .send(TrackEvent::Moved(TrackEventFileMove {
                    from: from.clone(),
                    to: to.clone(),
                }))
                .await;
        }
        (true, false) => {
            let event_file = TrackEventFile::from_path(from);
            let _ = events_tx.send(TrackEvent::Removed(event_file)).await;
        }
        (false, true) => {
            let event_file = TrackEventFile::from_path(to);
            let _ = events_tx.send(TrackEvent::Created(event_file)).await;
        }
        (false, false) => {}
    }
}

async fn emit_diff_events(
    events_tx: &mpsc::Sender<TrackEvent>,
    old: &HashSet<AbsPath>,
    new: &HashSet<AbsPath>,
) {
    emit_diff_events_filtered(events_tx, old, new, &HashSet::new()).await;
}

async fn emit_diff_events_filtered(
    events_tx: &mpsc::Sender<TrackEvent>,
    old: &HashSet<AbsPath>,
    new: &HashSet<AbsPath>,
    ignored_paths: &HashSet<AbsPath>,
) {
    let mut tracked_now = BTreeSet::new();
    let mut untracked_now = BTreeSet::new();

    for path in new {
        if !old.contains(path) && !ignored_paths.contains(path) {
            tracked_now.insert(path.clone());
        }
    }
    for path in old {
        if !new.contains(path) && !ignored_paths.contains(path) {
            untracked_now.insert(path.clone());
        }
    }

    for path in tracked_now {
        let event_file = TrackEventFile::from_path(&path);
        let _ = events_tx.send(TrackEvent::Tracked(event_file)).await;
    }

    for path in untracked_now {
        let event_file = TrackEventFile::from_path(&path);
        let _ = events_tx.send(TrackEvent::Untracked(event_file)).await;
    }
}

fn refresh_aux_watches(
    state: &TrackerState,
    watcher: &mut impl WatchControl,
    aux_watches: &mut HashSet<AbsPath>,
) -> Result<()> {
    let mut wanted = HashSet::new();

    for ignore_file in &state.ignore_files {
        if state
            .project_path
            .repo_path()
            .abs_dir_path()
            .is_or_contains(ignore_file)
        {
            continue;
        }

        if ignore_file.path_buf().exists() {
            wanted.insert(ignore_file.clone());
        } else if let Some(parent) = ignore_file.parent() {
            wanted.insert(parent.to_abs_path());
        }
    }

    for abs_path in aux_watches.clone() {
        if !wanted.contains(&abs_path) {
            let _ = watcher.unwatch(abs_path.path());
            aux_watches.remove(&abs_path);
        }
    }

    for abs_path in wanted {
        if aux_watches.contains(&abs_path) {
            continue;
        }
        watcher
            .watch(abs_path.path(), RecursiveMode::NonRecursive)
            .with_context(|| format!("failed to watch aux path {}", abs_path.path().display()))?;
        aux_watches.insert(abs_path);
    }

    Ok(())
}

trait WatchControl {
    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()>;
    fn unwatch(&mut self, path: &Path) -> notify::Result<()>;
}

impl WatchControl for RecommendedWatcher {
    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
        Watcher::watch(self, path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        Watcher::unwatch(self, path)
    }
}

impl<C: FileIdCache> WatchControl for Debouncer<RecommendedWatcher, C> {
    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> notify::Result<()> {
        Debouncer::watch(self, path, recursive_mode)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        Debouncer::unwatch(self, path)
    }
}

async fn build_filter(
    watch_root: &RepoPath,
    app_name: Option<&str>,
) -> Result<(IgnoreFilter, HashSet<AbsPath>)> {
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

            let ignore_files: HashSet<AbsPath> = local
                .iter()
                .map(|f| AbsPath::try_new(f.path.clone()).unwrap())
                .collect();
            let filter = IgnoreFilter::new(&watch_root, &local).await?;
            Ok::<(IgnoreFilter, HashSet<AbsPath>), anyhow::Error>((filter, ignore_files))
        })
    })
    .await
    .context("ignore rebuild blocking task panicked")?
}

async fn scan_tracked_files(
    project_path: &ProjectPath,
    ignore_filter: &IgnoreFilter,
    only_source: bool,
) -> Result<HashSet<AbsPath>> {
    let root = project_path.cwd_path().to_path_buf();

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
        let abs_path = AbsPath::try_new(path)?;
        if is_tracked_file(project_path, &abs_path, ignore_filter, only_source) {
            tracked.insert(abs_path);
        }
    }

    Ok(tracked)
}

fn is_tracked_file(
    project_path: &ProjectPath,
    path: &AbsPath,
    filter: &IgnoreFilter,
    only_source: bool,
) -> bool {
    if !project_path.is_project_file(&path) {
        return false;
    }

    if only_source && !path.is_source_file() {
        return false;
    }

    match filter.match_path(path.path_buf(), false) {
        Match::None | Match::Whitelist(_) => true,
        Match::Ignore(glob) => !path.matches_glob(glob),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec;
    use std::{fs, time::Duration};

    use git2::Repository;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;
    use test_context::{AsyncTestContext, test_context};
    use tokio::time::timeout;

    const SETTLE_DELAY: Duration = Duration::from_millis(100);

    #[test]
    fn test_find_repo_root_from_nested_dir() {
        let repo = TestRepo::new();

        let nested = repo.path().join("a/b/c");
        fs::create_dir_all(&nested).unwrap();

        let project_path = ProjectPath::find(&nested).unwrap();
        assert_eq!(project_path.repo_path().path(), repo.path());
    }

    #[cfg(feature = "source-filter")]
    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_source_filter_enabled_behavior_filter_off(ctx: &mut RepoContext) {
        ctx.repo
            .write_all(vec![("file", ""), ("file.bin", ""), ("file.js", "")]);

        ctx.repo.start().await;
        assert_events!(
            ctx.repo,
            ctx.fac.initial_ev("file"),
            ctx.fac.initial_ev("file.bin"),
            ctx.fac.initial_ev("file.js"),
        );
    }

    #[cfg(feature = "source-filter")]
    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_source_filter_enabled_behavior_filter_on(ctx: &mut RepoContext) {
        ctx.repo
            .write_all(vec![("file", ""), ("file.bin", ""), ("file.js", "")]);

        ctx.repo
            .start_with_options(WatchOptions {
                only_source: true,
                ..WatchOptions::default()
            })
            .await;
        assert_events!(ctx.repo, ctx.fac.initial_ev("file.js"),);
    }

    #[cfg(not(feature = "source-filter"))]
    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_source_filter_disabled_behavior(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            ("file", ""),
            ("file.bin", ""),
            ("file.zip", ""),
            ("file.js", ""),
        ]);

        ctx.repo.start().await;
        assert_events!(
            ctx.repo,
            ctx.fac.initial_ev("file"),
            ctx.fac.initial_ev("file.bin"),
            ctx.fac.initial_ev("file.js"),
            ctx.fac.initial_ev("file.zip"),
        );
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_initial_tracked_events(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            (".gitignore", "ignored.tmp\nlogs/\n"),
            ("root.rs", "fn main() {}\n"),
            ("src/lib.rs", "pub fn lib() {}\n"),
            ("ignored.tmp", "skip\n"),
            ("logs/app.log", "skip\n"),
        ]);

        ctx.repo.start().await;
        assert_events!(
            ctx.repo,
            ctx.fac.initial_ev(".gitignore"),
            ctx.fac.initial_ev("root.rs"),
            ctx.fac.initial_ev("src/lib.rs"),
        );
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_start_with_options(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![("main.rs", "fn main() {}\n")]);

        ctx.repo
            .start_with_options(WatchOptions {
                settle_delay: Duration::from_millis(10),
                event_channel_capacity: 16,
                ..WatchOptions::default()
            })
            .await;
        assert_events!(ctx.repo, ctx.fac.initial_ev("main.rs"),);
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_ignore_changes(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            (".gitignore", "ignored.txt\n"),
            ("ignored.txt", "hello"),
            ("tracked.rs", "fn main() {}\n"),
        ]);

        ctx.repo.start().await;
        assert_events!(
            ctx.repo,
            ctx.fac.initial_ev(".gitignore"),
            ctx.fac.initial_ev("tracked.rs"),
        );

        ctx.repo.write(".gitignore", "");
        assert_events!(
            ctx.repo,
            ctx.fac.changed_ev(".gitignore"),
            ctx.fac.tracked_ev("ignored.txt"),
        );
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_nested_repo_cwd(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            (".gitignore", ""),
            ("app/a.log", "x\n"),
            ("app/main.rs", "fn main() {}\n"),
        ]);

        let fac = ctx.repo.start_at("app").await;
        assert_events!(ctx.repo, fac.initial_ev("a.log"), fac.initial_ev("main.rs"),);

        ctx.repo.write(".gitignore", "*.log\n");
        assert_events!(ctx.repo, fac.untracked_ev("a.log"),);

        ctx.repo.write(".gitignore", "");
        assert_events!(ctx.repo, fac.tracked_ev("a.log"),);
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_global_ignore(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            ("global-excludes", "global.txt\n"),
            ("app/global.txt", "x\n"),
        ]);
        ctx.repo.set_excludes_file("global-excludes");

        let fac = ctx.repo.start_at("app").await;
        assert_events!(ctx.repo);

        ctx.repo.write("global-excludes", "");
        assert_events!(ctx.repo, fac.tracked_ev("global.txt"),);
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_move(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            (".gitignore", "*.ignored.txt\n"),
            ("a.ignored.txt", "hello"),
            ("tracked.rs", "fn main() {}\n"),
        ]);

        ctx.repo.start().await;
        assert_events!(
            ctx.repo,
            ctx.fac.initial_ev(".gitignore"),
            ctx.fac.initial_ev("tracked.rs"),
        );

        ctx.repo.rename("tracked.rs", "moved.rs");
        ctx.repo.rename("a.ignored.txt", "b.ignored.txt");
        assert_events!(ctx.repo, ctx.fac.moved_ev("tracked.rs", "moved.rs"),);
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_change(ctx: &mut RepoContext) {
        ctx.repo
            .write_all(vec![("a.txt", "Hello"), ("b.txt", "Hello")]);

        ctx.repo.start().await;
        assert_events!(
            ctx.repo,
            ctx.fac.initial_ev("a.txt"),
            ctx.fac.initial_ev("b.txt"),
        );

        ctx.repo.write("a.txt", "Hello, world!");
        ctx.repo.write("b.txt", "Hello, world!");
        assert_events!(
            ctx.repo,
            ctx.fac.changed_ev("a.txt"),
            ctx.fac.changed_ev("b.txt"),
        );
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_rapid_updates(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            ("a.txt", "Hello"),
            ("b.txt", "Hello"),
            ("c.txt", "Hello"),
        ]);

        ctx.repo.start().await;
        assert_events!(
            ctx.repo,
            ctx.fac.initial_ev("a.txt"),
            ctx.fac.initial_ev("b.txt"),
            ctx.fac.initial_ev("c.txt"),
        );

        ctx.repo.write("a.txt", "Hello,");
        ctx.repo.write("a.txt", "Hello, ");
        ctx.repo.write("a.txt", "Hello, w");
        ctx.repo.write("a.txt", "Hello, wo...");
        ctx.repo.rename("b.txt", "b2.txt");
        ctx.repo.rename("b2.txt", "b.txt");
        ctx.repo.rename("b.txt", "b3.txt");
        ctx.repo.rename("c.txt", "c2.txt");
        ctx.repo.rename("c2.txt", "c.txt");
        assert_events!(
            ctx.repo,
            ctx.fac.changed_ev("a.txt"),
            ctx.fac.moved_ev("b.txt", "b3.txt"),
        );
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_split_rename_from_to_behaves_as_moved(ctx: &mut RepoContext) {
        ctx.repo.write("old.rs", "pub fn old() {}\n");
        ctx.repo.write("new.rs", "pub fn new() {}\n");

        let (mut state, _ignore_files, fac) = tracker_state_for(ctx.repo.path()).await;
        let (events_tx, mut events_rx) = mpsc::channel(16);

        process_fs_event(
            &mut state,
            &events_tx,
            notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::From)),
                paths: vec![ctx.repo.path().join("old.rs")],
                attrs: Default::default(),
            },
        )
        .await;

        process_fs_event(
            &mut state,
            &events_tx,
            notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::To)),
                paths: vec![ctx.repo.path().join("new.rs")],
                attrs: Default::default(),
            },
        )
        .await;

        let events = collect_all_events(&mut events_rx).await;

        assert_eq!(events, vec![fac.moved_ev("old.rs", "new.rs"),]);
        assert!(!state.tracked.contains(&fac.abs_path("old.rs")));
        assert!(state.tracked.contains(&fac.abs_path("new.rs")));
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_process_fs_event_emits_exact_live_event_sequence(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            ("main.rs", "fn main() {}\n"),
            ("new.rs", "pub fn new() {}\n"),
        ]);

        let (mut state, _ignore_files, fac) = tracker_state_for(ctx.repo.path()).await;
        let (events_tx, mut events_rx) = mpsc::channel(16);

        ctx.repo.write("extra.rs", "pub fn extra() {}\n");

        process_fs_event(
            &mut state,
            &events_tx,
            notify::Event {
                kind: EventKind::Create(notify::event::CreateKind::File),
                paths: vec![ctx.repo.path().join("extra.rs")],
                attrs: Default::default(),
            },
        )
        .await;

        process_fs_event(
            &mut state,
            &events_tx,
            notify::Event {
                kind: EventKind::Modify(ModifyKind::Data(notify::event::DataChange::Any)),
                paths: vec![ctx.repo.path().join("extra.rs")],
                attrs: Default::default(),
            },
        )
        .await;

        fs::rename(
            ctx.repo.path().join("main.rs"),
            ctx.repo.path().join("main2.rs"),
        )
        .unwrap();
        process_fs_event(
            &mut state,
            &events_tx,
            notify::Event {
                kind: EventKind::Modify(ModifyKind::Name(RenameMode::Both)),
                paths: vec![
                    ctx.repo.path().join("main.rs"),
                    ctx.repo.path().join("main2.rs"),
                ],
                attrs: Default::default(),
            },
        )
        .await;

        fs::remove_file(ctx.repo.path().join("extra.rs")).unwrap();
        process_fs_event(
            &mut state,
            &events_tx,
            notify::Event {
                kind: EventKind::Remove(notify::event::RemoveKind::File),
                paths: vec![ctx.repo.path().join("extra.rs")],
                attrs: Default::default(),
            },
        )
        .await;

        let events = collect_all_events(&mut events_rx).await;

        assert_eq!(
            events,
            vec![
                fac.created_ev("extra.rs"),
                fac.changed_ev("extra.rs"),
                fac.moved_ev("main.rs", "main2.rs"),
                fac.removed_ev("extra.rs"),
            ]
        );
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_emit_diff_events_emits_exact_tracking_transitions(ctx: &mut RepoContext) {
        let (events_tx, mut events_rx) = mpsc::channel(16);

        let old: HashSet<AbsPath> = [ctx.fac.abs_path("a.rs"), ctx.fac.abs_path("b.rs")].into();
        let new: HashSet<AbsPath> = [ctx.fac.abs_path("b.rs"), ctx.fac.abs_path("c.rs")].into();

        emit_diff_events(&events_tx, &old, &new).await;

        let events = collect_all_events(&mut events_rx).await;

        assert_eq!(
            events,
            vec![ctx.fac.tracked_ev("c.rs"), ctx.fac.untracked_ev("a.rs"),]
        );
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_mid_path_gitignore(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            ("path/to/file/.gitignore", "/a.txt\n"),
            ("path/to/file/a.txt", ""),
            ("path/to/file/b.txt", ""),
            ("path/to/file/c.txt", ""),
            ("path/to/file/d.txt", ""),
            ("path/to/a.txt", ""),
            ("path/to/b.txt", ""),
            ("path/to/c.txt", ""),
            ("path/to/d.txt", ""),
            (".gitignore", "d.txt\n"),
        ]);

        ctx.repo.start().await;
        assert_events!(
            ctx.repo,
            ctx.fac.initial_ev(".gitignore"),
            ctx.fac.initial_ev("path/to/a.txt"),
            ctx.fac.initial_ev("path/to/b.txt"),
            ctx.fac.initial_ev("path/to/c.txt"),
            ctx.fac.initial_ev("path/to/file/.gitignore"),
            ctx.fac.initial_ev("path/to/file/b.txt"),
            ctx.fac.initial_ev("path/to/file/c.txt"),
        );

        ctx.repo
            .write("path/to/.gitignore", "a.txt\n/b.txt\nc.txt\n");
        assert_events!(
            ctx.repo,
            ctx.fac.created_ev("path/to/.gitignore"),
            ctx.fac.untracked_ev("path/to/a.txt"),
            ctx.fac.untracked_ev("path/to/b.txt"),
            ctx.fac.untracked_ev("path/to/c.txt"),
            ctx.fac.untracked_ev("path/to/file/c.txt"),
        );
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_is_tracked_reflects_ignore_changes(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            (".gitignore", "a.txt\nb.txt\n"),
            ("a.txt", "a\n"),
            ("b.txt", "b\n"),
        ]);

        ctx.repo.start().await;
        let tracker = ctx.repo.tracker();

        assert!(!tracker.is_tracked("a.txt").await.unwrap());
        assert!(!tracker.is_tracked("b.txt").await.unwrap());

        ctx.repo.write(".gitignore", "b.txt\n");
        assert!(tracker.is_tracked("a.txt").await.unwrap());
        assert!(!tracker.is_tracked("b.txt").await.unwrap());

        ctx.repo.write(".gitignore", "a.txt\n");
        assert!(!tracker.is_tracked("a.txt").await.unwrap());
        assert!(tracker.is_tracked("b.txt").await.unwrap());
    }

    async fn collect_all_events(rx: &mut mpsc::Receiver<TrackEvent>) -> Vec<TrackEvent> {
        let mut out = Vec::new();
        while let Ok(Some(ev)) = timeout(Duration::from_millis(20), rx.recv()).await {
            out.push(ev);
        }
        out
    }

    async fn tracker_state_for(path: &Path) -> (TrackerState, HashSet<AbsPath>, TestFactory) {
        let watch_root = path.to_path_buf();
        let project_path = ProjectPath::find(&watch_root).unwrap();
        let (ignore_filter, ignore_files) =
            build_filter(&project_path.repo_path(), None).await.unwrap();
        let tracked = scan_tracked_files(&project_path, &ignore_filter, false)
            .await
            .unwrap();
        let fac = TestFactory {
            project_path: project_path.clone(),
        };

        (
            TrackerState {
                project_path,
                options: WatchOptions::default(),
                ignore_filter,
                tracked,
                ignore_files: ignore_files.clone(),
                segment_readiness: HashMap::new(),
                pending_rename_from: VecDeque::new(),
            },
            ignore_files,
            fac,
        )
    }

    struct RepoContext {
        repo: TestRepo,
        fac: TestFactory,
    }

    impl AsyncTestContext for RepoContext {
        async fn setup() -> RepoContext {
            let repo = TestRepo::new();
            let fac = TestFactory::new(&repo);
            RepoContext { repo, fac }
        }

        async fn teardown(self) {
            let mut repo = self.repo;
            repo.stop().await;
        }
    }

    struct TestRepo {
        dir: TempDir,
        cwd: PathBuf,
        repo: Repository,
        rx: Option<mpsc::Receiver<TrackEvent>>,
        tracker: Option<FileTracker>,
    }

    impl TestRepo {
        fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let cwd = dir.path().to_path_buf();
            let repo = Repository::init(dir.path()).unwrap();
            Self {
                dir,
                cwd,
                repo,
                rx: None,
                tracker: None,
            }
        }

        fn write<Pth: AsRef<Path>>(&self, path: Pth, content: &str) {
            let full_path = self.dir.path().join(path.as_ref());
            if let Some(parent) = full_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(full_path, content).unwrap();
        }

        fn rename<Pth: AsRef<Path>>(&self, from: Pth, to: Pth) {
            let from_path = self.dir.path().join(from.as_ref());
            let to_path = self.dir.path().join(to.as_ref());
            if let Some(parent) = to_path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::rename(from_path, to_path).unwrap();
        }

        fn write_all<Pth: AsRef<Path>>(&self, paths: Vec<(Pth, &str)>) {
            for (path, content) in paths {
                self.write(path, content);
            }
        }

        fn path(&self) -> &Path {
            self.cwd.as_path()
        }

        fn config(&self) -> git2::Config {
            self.repo.config().unwrap()
        }

        fn set_excludes_file(&self, path: &str) {
            let mut config = self.config();
            config
                .set_str(
                    "core.excludesFile",
                    &self.dir.path().join(path).to_string_lossy(),
                )
                .unwrap();
        }

        async fn start(&mut self) {
            self.start_with_options(Default::default()).await;
        }

        async fn start_at(&mut self, cwd: &str) -> TestFactory {
            self.cwd = self.cwd.join(cwd);
            let fac = TestFactory::new(self);
            self.start().await;
            fac
        }

        async fn start_with_options(&mut self, options: WatchOptions) {
            let (tracker, rx) = FileTracker::start_with_options(self.path(), options)
                .await
                .unwrap();
            self.tracker = Some(tracker);
            self.rx = Some(rx);
        }

        async fn stop(&mut self) {
            if let Some(tracker) = self.tracker.take() {
                tracker.stop().await.unwrap();
            }
        }

        async fn events(&mut self) -> Vec<TrackEvent> {
            let rx = &mut self.rx.as_mut().unwrap();
            let mut events = vec![];
            while let Ok(Some(event)) = timeout(SETTLE_DELAY, rx.recv()).await {
                events.push(event);
            }
            events
        }

        fn tracker(&self) -> &FileTracker {
            self.tracker.as_ref().unwrap()
        }
    }

    struct TestFactory {
        project_path: ProjectPath,
    }

    impl TestFactory {
        pub fn new(repo: &TestRepo) -> Self {
            let project_path = ProjectPath::find(repo.path()).unwrap();
            Self { project_path }
        }

        pub fn abs_path<P: Into<PathBuf>>(&self, path: P) -> AbsPath {
            self.project_path.abs_path_within_cwd(path.into()).unwrap()
        }

        pub fn event_file<P: Into<PathBuf>>(&self, path: P) -> TrackEventFile {
            TrackEventFile::from_path(&self.abs_path(path))
        }

        pub fn initial_ev<P: Into<PathBuf>>(&self, path: P) -> TrackEvent {
            TrackEvent::InitialTracked(self.event_file(path))
        }

        pub fn changed_ev<P: Into<PathBuf>>(&self, path: P) -> TrackEvent {
            TrackEvent::Changed(self.event_file(path))
        }

        pub fn moved_ev<From: Into<PathBuf>, To: Into<PathBuf>>(
            &self,
            from: From,
            to: To,
        ) -> TrackEvent {
            TrackEvent::Moved(TrackEventFileMove {
                from: self.abs_path(from),
                to: self.abs_path(to),
            })
        }

        pub fn removed_ev<P: Into<PathBuf>>(&self, path: P) -> TrackEvent {
            TrackEvent::Removed(self.event_file(path))
        }

        pub fn created_ev<Path: Into<PathBuf>>(&self, path: Path) -> TrackEvent {
            TrackEvent::Created(self.event_file(path))
        }

        pub fn tracked_ev<Path: Into<PathBuf>>(&self, path: Path) -> TrackEvent {
            TrackEvent::Tracked(self.event_file(path))
        }

        pub fn untracked_ev<Path: Into<PathBuf>>(&self, path: Path) -> TrackEvent {
            TrackEvent::Untracked(self.event_file(path))
        }
    }

    #[macro_export]
    macro_rules! assert_events {
        ($repo:expr $(, $event:expr)* $(,)?) => {
            let events = $repo.events().await;
            assert_eq!(
                events,
                vec![$($event),*],
            );
        };
    }
}
