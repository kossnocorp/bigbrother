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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackEvent {
    InitialTracked(TrackEventFile),
    Tracked(TrackEventFile),
    Untracked(TrackEventFile),
    Created(TrackEventFile),
    Changed(TrackEventFile),
    Removed(TrackEventFile),
    Moved(TrackEventFileMove),
    Error(TrackEventError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackEventFile {
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackEventFileMove {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackEventError {
    pub message: String,
}

#[derive(Debug)]
pub struct FileTracker {
    pub cwd: PathBuf,
    pub repo_dir: Option<PathBuf>,
    command_tx: mpsc::Sender<TrackerCommand>,
    stop_tx: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

#[derive(Debug)]
enum TrackerCommand {
    IsTracked {
        path: PathBuf,
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
        let cwd = absolute_path(cwd.as_ref()).context("failed to resolve cwd")?;
        let repo_dir = find_repo_root(&cwd);
        let watch_root = repo_dir.clone().unwrap_or_else(|| cwd.clone());

        let state = TrackerState::new(cwd.clone(), watch_root, options.clone()).await?;

        let (events_tx, events_rx) = mpsc::channel(state.options.event_channel_capacity);
        for rel in sorted_rel_paths(&state.cwd, &state.tracked) {
            let _ = events_tx
                .send(TrackEvent::InitialTracked(TrackEventFile { path: rel }))
                .await;
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
                cwd,
                repo_dir,
                command_tx,
                stop_tx: Some(stop_tx),
                task: Some(task),
            },
            events_rx,
        ))
    }

    pub async fn is_tracked(&self, path: impl AsRef<Path>) -> Result<bool> {
        let abs = absolute_or_join(&self.cwd, path.as_ref());
        let (reply_tx, reply_rx) = oneshot::channel();

        self.command_tx
            .send(TrackerCommand::IsTracked {
                path: abs,
                reply: reply_tx,
            })
            .await
            .context("tracker is not running")?;

        reply_rx
            .await
            .context("tracker task stopped before replying")
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
    segment_readiness: HashMap<PathBuf, SegmentReadiness>,
    pending_rename_from: VecDeque<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentReadiness {
    Ready,
    Dirty,
}

impl TrackerState {
    async fn new(cwd: PathBuf, watch_root: PathBuf, options: WatchOptions) -> Result<Self> {
        let (ignore_filter, ignore_files) =
            build_filter(&watch_root, options.app_name.as_deref()).await?;
        let tracked =
            scan_tracked_files(&cwd, &watch_root, &ignore_filter, options.only_source()).await?;

        Ok(Self {
            cwd,
            watch_root: watch_root.clone(),
            options,
            ignore_filter,
            tracked,
            ignore_files,
            segment_readiness: HashMap::from([(watch_root.clone(), SegmentReadiness::Ready)]),
            pending_rename_from: VecDeque::new(),
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
        for readiness in self.segment_readiness.values_mut() {
            *readiness = SegmentReadiness::Ready;
        }
        self.pending_rename_from.clear();

        Ok((old, tracked))
    }

    fn needs_rebuild_for_query(&self, path: &Path) -> bool {
        for segment in self.path_segments(path) {
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

    fn mark_path_ready(&mut self, path: &Path) {
        for segment in self.path_segments(path) {
            self.segment_readiness
                .insert(segment, SegmentReadiness::Ready);
        }
    }

    fn mark_dirty_for_ignore_change(&mut self, path: &Path) {
        let abs = absolute_or_join(&self.watch_root, path);

        if abs.to_string_lossy().ends_with(&format!(
            "{0}.git{0}info{0}exclude",
            std::path::MAIN_SEPARATOR
        )) || (abs.file_name().is_some_and(|n| n == "config")
            && abs
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == ".git"))
        {
            for readiness in self.segment_readiness.values_mut() {
                *readiness = SegmentReadiness::Dirty;
            }
            return;
        }

        let Some(parent) = abs.parent() else {
            return;
        };

        let parent = parent.to_path_buf();
        for (segment, readiness) in &mut self.segment_readiness {
            if segment.starts_with(&parent) {
                *readiness = SegmentReadiness::Dirty;
            }
        }
    }

    fn path_segments(&self, path: &Path) -> Vec<PathBuf> {
        if !path.starts_with(&self.watch_root) {
            return Vec::new();
        }

        let mut segments = Vec::new();
        let mut current = path.parent();
        while let Some(dir) = current {
            if dir.starts_with(&self.watch_root) {
                segments.push(dir.to_path_buf());
            } else {
                break;
            }

            if dir == self.watch_root {
                break;
            }

            current = dir.parent();
        }

        segments.reverse();
        segments
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
                            emit_diff_events(&events_tx, &state.cwd, &old, &new).await;
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
    aux_watches: &mut HashSet<PathBuf>,
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
                    let abs = absolute_or_join(&state.watch_root, path);
                    if abs.starts_with(&state.cwd) {
                        touched_paths.insert(abs.clone());
                    }
                    if is_ignore_related_path(path, state) {
                        state.mark_dirty_for_ignore_change(path);
                        saw_ignore_related_change = true;
                    }
                }

                process_fs_event(state, events_tx, event).await;
            }

            flush_pending_rename_from(state, events_tx).await;

            if saw_ignore_related_change {
                time::sleep(state.options.settle_delay).await;
                match state.rebuild().await {
                    Ok((old, new)) => {
                        emit_diff_events_filtered(
                            events_tx,
                            &state.cwd,
                            &old,
                            &new,
                            &touched_paths,
                        )
                        .await;
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
                        .send(TrackEvent::Removed(TrackEventFile {
                            path: to_relative(&state.cwd, &abs),
                        }))
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
                        .send(TrackEvent::Created(TrackEventFile {
                            path: to_relative(&state.cwd, &abs),
                        }))
                        .await;
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                if state.tracked.contains(&abs) {
                    state.pending_rename_from.push_back(abs);
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::To)) => {
                if let Some(from) = state.pending_rename_from.pop_front() {
                    handle_rename(state, events_tx, &from, &abs).await;
                } else if is_tracked_file(
                    &state.cwd,
                    &abs,
                    &state.ignore_filter,
                    state.options.only_source(),
                ) && state.tracked.insert(abs.clone())
                {
                    let _ = events_tx
                        .send(TrackEvent::Created(TrackEventFile {
                            path: to_relative(&state.cwd, &abs),
                        }))
                        .await;
                }
            }
            EventKind::Modify(_) => {
                if state.tracked.contains(&abs) {
                    let _ = events_tx
                        .send(TrackEvent::Changed(TrackEventFile {
                            path: to_relative(&state.cwd, &abs),
                        }))
                        .await;
                } else if is_tracked_file(
                    &state.cwd,
                    &abs,
                    &state.ignore_filter,
                    state.options.only_source(),
                ) && state.tracked.insert(abs.clone())
                {
                    let _ = events_tx
                        .send(TrackEvent::Created(TrackEventFile {
                            path: to_relative(&state.cwd, &abs),
                        }))
                        .await;
                }
            }
            _ => {}
        }
    }
}

async fn flush_pending_rename_from(state: &mut TrackerState, events_tx: &mpsc::Sender<TrackEvent>) {
    while let Some(from) = state.pending_rename_from.pop_front() {
        if state.tracked.remove(&from) {
            let _ = events_tx
                .send(TrackEvent::Removed(TrackEventFile {
                    path: to_relative(&state.cwd, &from),
                }))
                .await;
        }
    }
}

async fn handle_rename(
    state: &mut TrackerState,
    events_tx: &mpsc::Sender<TrackEvent>,
    from: &Path,
    to: &Path,
) {
    if from == to {
        return;
    }

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
                .send(TrackEvent::Moved(TrackEventFileMove {
                    from: to_relative(&state.cwd, from),
                    to: to_relative(&state.cwd, to),
                }))
                .await;
        }
        (true, false) => {
            let _ = events_tx
                .send(TrackEvent::Removed(TrackEventFile {
                    path: to_relative(&state.cwd, from),
                }))
                .await;
        }
        (false, true) => {
            let _ = events_tx
                .send(TrackEvent::Created(TrackEventFile {
                    path: to_relative(&state.cwd, to),
                }))
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
    emit_diff_events_filtered(events_tx, cwd, old, new, &HashSet::new()).await;
}

async fn emit_diff_events_filtered(
    events_tx: &mpsc::Sender<TrackEvent>,
    cwd: &Path,
    old: &HashSet<PathBuf>,
    new: &HashSet<PathBuf>,
    ignored_paths: &HashSet<PathBuf>,
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
        let _ = events_tx
            .send(TrackEvent::Tracked(TrackEventFile {
                path: to_relative(cwd, &path),
            }))
            .await;
    }
    for path in untracked_now {
        let _ = events_tx
            .send(TrackEvent::Untracked(TrackEventFile {
                path: to_relative(cwd, &path),
            }))
            .await;
    }
}

fn refresh_aux_watches(
    state: &TrackerState,
    watcher: &mut impl WatchControl,
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
        let found = find_repo_root(&nested).unwrap();
        assert_eq!(found, repo.path().to_path_buf());
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
            initial_ev("file"),
            initial_ev("file.bin"),
            initial_ev("file.js"),
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
        assert_events!(ctx.repo, initial_ev("file.js"),);
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
            initial_ev("file"),
            initial_ev("file.bin"),
            initial_ev("file.js"),
            initial_ev("file.zip"),
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
            initial_ev(".gitignore"),
            initial_ev("root.rs"),
            initial_ev("src/lib.rs"),
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
        assert_events!(ctx.repo, initial_ev("main.rs"),);
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
        assert_events!(ctx.repo, initial_ev(".gitignore"), initial_ev("tracked.rs"),);

        ctx.repo.write(".gitignore", "");
        assert_events!(
            ctx.repo,
            changed_ev(".gitignore"),
            tracked_ev("ignored.txt"),
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

        ctx.repo.start_at("app").await;
        assert_events!(ctx.repo, initial_ev("a.log"), initial_ev("main.rs"),);

        ctx.repo.write(".gitignore", "*.log\n");
        assert_events!(ctx.repo, untracked_ev("a.log"),);

        ctx.repo.write(".gitignore", "");
        assert_events!(ctx.repo, tracked_ev("a.log"),);
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_global_ignore(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            ("global-excludes", "global.txt\n"),
            ("app/global.txt", "x\n"),
        ]);
        ctx.repo.set_excludes_file("global-excludes");

        ctx.repo.start_at("app").await;
        assert_events!(ctx.repo);

        ctx.repo.write("global-excludes", "");
        assert_events!(ctx.repo, tracked_ev("global.txt"),);
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
        assert_events!(ctx.repo, initial_ev(".gitignore"), initial_ev("tracked.rs"),);

        ctx.repo.rename("tracked.rs", "moved.rs");
        ctx.repo.rename("a.ignored.txt", "b.ignored.txt");
        assert_events!(ctx.repo, moved_ev("tracked.rs", "moved.rs"),);
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_change(ctx: &mut RepoContext) {
        ctx.repo
            .write_all(vec![("a.txt", "Hello"), ("b.txt", "Hello")]);

        ctx.repo.start().await;
        assert_events!(ctx.repo, initial_ev("a.txt"), initial_ev("b.txt"),);

        ctx.repo.write("a.txt", "Hello, world!");
        ctx.repo.write("b.txt", "Hello, world!");
        assert_events!(ctx.repo, changed_ev("a.txt"), changed_ev("b.txt"),);
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
            initial_ev("a.txt"),
            initial_ev("b.txt"),
            initial_ev("c.txt"),
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
        assert_events!(ctx.repo, changed_ev("a.txt"), moved_ev("b.txt", "b3.txt"),);
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_split_rename_from_to_behaves_as_moved(ctx: &mut RepoContext) {
        ctx.repo.write("old.rs", "pub fn old() {}\n");
        ctx.repo.write("new.rs", "pub fn new() {}\n");

        let (mut state, _ignore_files) = tracker_state_for(ctx.repo.path()).await;
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

        assert_eq!(
            events,
            vec![TrackEvent::Moved(TrackEventFileMove {
                from: PathBuf::from("old.rs"),
                to: PathBuf::from("new.rs"),
            })]
        );
        assert!(!state.tracked.contains(&ctx.repo.path().join("old.rs")));
        assert!(state.tracked.contains(&ctx.repo.path().join("new.rs")));
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_process_fs_event_emits_exact_live_event_sequence(ctx: &mut RepoContext) {
        ctx.repo.write_all(vec![
            ("main.rs", "fn main() {}\n"),
            ("new.rs", "pub fn new() {}\n"),
        ]);

        let (mut state, _ignore_files) = tracker_state_for(ctx.repo.path()).await;
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
                TrackEvent::Created(TrackEventFile {
                    path: PathBuf::from("extra.rs"),
                }),
                TrackEvent::Changed(TrackEventFile {
                    path: PathBuf::from("extra.rs"),
                }),
                TrackEvent::Moved(TrackEventFileMove {
                    from: PathBuf::from("main.rs"),
                    to: PathBuf::from("main2.rs"),
                }),
                TrackEvent::Removed(TrackEventFile {
                    path: PathBuf::from("extra.rs"),
                }),
            ]
        );
    }

    #[test_context(RepoContext)]
    #[tokio::test]
    async fn test_emit_diff_events_emits_exact_tracking_transitions(ctx: &mut RepoContext) {
        let (events_tx, mut events_rx) = mpsc::channel(16);

        let old: HashSet<PathBuf> =
            [ctx.repo.path().join("a.rs"), ctx.repo.path().join("b.rs")].into();
        let new: HashSet<PathBuf> =
            [ctx.repo.path().join("b.rs"), ctx.repo.path().join("c.rs")].into();

        emit_diff_events(&events_tx, ctx.repo.path(), &old, &new).await;

        let events = collect_all_events(&mut events_rx).await;

        assert_eq!(
            events,
            vec![
                TrackEvent::Tracked(TrackEventFile {
                    path: PathBuf::from("c.rs"),
                }),
                TrackEvent::Untracked(TrackEventFile {
                    path: PathBuf::from("a.rs"),
                }),
            ]
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
            initial_ev(".gitignore"),
            initial_ev("path/to/a.txt"),
            initial_ev("path/to/b.txt"),
            initial_ev("path/to/c.txt"),
            initial_ev("path/to/file/.gitignore"),
            initial_ev("path/to/file/b.txt"),
            initial_ev("path/to/file/c.txt"),
        );

        ctx.repo
            .write("path/to/.gitignore", "a.txt\n/b.txt\nc.txt\n");
        assert_events!(
            ctx.repo,
            created_ev("path/to/.gitignore"),
            untracked_ev("path/to/a.txt"),
            untracked_ev("path/to/b.txt"),
            untracked_ev("path/to/c.txt"),
            untracked_ev("path/to/file/c.txt"),
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

    async fn tracker_state_for(path: &Path) -> (TrackerState, HashSet<PathBuf>) {
        let watch_root = path.to_path_buf();
        let (filter, ignore_files) = build_filter(&watch_root, None).await.unwrap();
        let tracked = scan_tracked_files(path, &watch_root, &filter, false)
            .await
            .unwrap();

        (
            TrackerState {
                cwd: watch_root.clone(),
                watch_root,
                options: WatchOptions::default(),
                ignore_filter: filter,
                tracked,
                ignore_files: ignore_files.clone(),
                segment_readiness: HashMap::new(),
                pending_rename_from: VecDeque::new(),
            },
            ignore_files,
        )
    }

    struct RepoContext {
        repo: TestRepo,
    }

    impl AsyncTestContext for RepoContext {
        async fn setup() -> RepoContext {
            let repo = TestRepo::new();
            RepoContext { repo }
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

        async fn start_at(&mut self, cwd: &str) {
            self.cwd = self.cwd.join(cwd);
            self.start().await;
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

    fn initial_ev<Path: Into<PathBuf>>(path: Path) -> TrackEvent {
        TrackEvent::InitialTracked(TrackEventFile { path: path.into() })
    }

    fn changed_ev<Path: Into<PathBuf>>(path: Path) -> TrackEvent {
        TrackEvent::Changed(TrackEventFile { path: path.into() })
    }

    fn moved_ev<From: Into<PathBuf>, To: Into<PathBuf>>(from: From, to: To) -> TrackEvent {
        TrackEvent::Moved(TrackEventFileMove {
            from: from.into(),
            to: to.into(),
        })
    }

    fn created_ev<Path: Into<PathBuf>>(path: Path) -> TrackEvent {
        TrackEvent::Created(TrackEventFile { path: path.into() })
    }

    fn tracked_ev<Path: Into<PathBuf>>(path: Path) -> TrackEvent {
        TrackEvent::Tracked(TrackEventFile { path: path.into() })
    }

    fn untracked_ev<Path: Into<PathBuf>>(path: Path) -> TrackEvent {
        TrackEvent::Untracked(TrackEventFile { path: path.into() })
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
