//! Debounced filesystem hot-reload watcher integration (`bamabi_watcher`).
//!
//! Monitors a mods directory recursively for native shared libraries (`.so`, `.dylib`, `.dll`)
//! and declarative `mod.toml` manifests, validates SemVer DAG constraints, performs incremental
//! topological directory loading, and triggers cascading reverse-topological `reload_mod` /
//! `unload_mod` operations in [`ModLifecycleFlush`].

use bamabi_host::mod_dag::{
    ModDagError, ModDependencyGraph, ModTomlManifest, discover_mod_manifests,
    resolve_dependency_dag,
};
use bamabi_host::schedule_sets::leased_schedules;
use bamabi_host::world_ext::ModWorldExt;
use bamabi_host::{
    ModLifecycleFlush, ModScheduleSetRegistry, ScheduleSetMode,
    ensure_mod_lifecycle_flush_schedule, prevalidate_mod_schedule_set_rewire_in_world,
    rewire_mod_schedule_sets_in_world,
};
#[cfg(feature = "signature-verification")]
use bamabi_host_sys::BAMPACK_MAGIC;
use bamabi_host_sys::schedule::{ModNamedSystemSet, ModVisibleSets, resolve_schedule_id_in_world};
use bamabi_host_sys::{LoadOptions, ModHost, ModId, ModLoadError, canonical_mod_name_from_path};
use bamabi_sys::ScheduleId;
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::Resource;
use bevy_ecs::world::World;
use notify_debouncer_full::notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify_debouncer_full::notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

/// Default bounded capacity of [`ModHotReloadHistory`] ring buffer (`256` entries).
pub const DEFAULT_MAX_HOT_RELOAD_HISTORY: usize = 256;

/// Default number of consecutive identical `(size, modified, inode)` polls required before a
/// changed mod artifact is treated as stable and eligible for loading/reloading.
pub const DEFAULT_STABILITY_POLLS: usize = 2;

/// Action taken by [`ModHotReloadWatcher`] for a changed or deleted mod artifact / manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModHotReloadAction {
    /// A newly discovered mod library was loaded into the `World`.
    Loaded,
    /// An already-loaded mod library (and any dependents) was hot-reloaded in topological order.
    Reloaded,
    /// A deleted mod library (and any dependents) was cleanly unloaded.
    Unloaded,
    /// Validating, loading, or reloading the changed mod failed with a diagnostic message.
    Failed(String),
}

/// Outcome record emitted by [`ModHotReloadWatcher::process_pending_reloads`] and stored in
/// [`ModHotReloadHistory`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModHotReloadOutcome {
    /// Path of the mod shared library or `mod.toml` manifest associated with the operation.
    pub path: PathBuf,
    /// Resulting [`ModId`] on `Loaded` / `Reloaded` (or unloaded [`ModId`] on `Unloaded`), or `None` on failure.
    pub mod_id: Option<ModId>,
    /// Action performed.
    pub action: ModHotReloadAction,
}

/// World resource recording bounded ring-buffer outcomes emitted by [`ModHotReloadPlugin`].
#[derive(Resource, Debug, Clone)]
pub struct ModHotReloadHistory {
    /// Chronological ring buffer of hot-reload outcomes observed in this `World`.
    pub outcomes: VecDeque<ModHotReloadOutcome>,
    max_history: usize,
}

impl Default for ModHotReloadHistory {
    #[inline]
    fn default() -> Self {
        Self::with_max_history(DEFAULT_MAX_HOT_RELOAD_HISTORY)
    }
}

impl ModHotReloadHistory {
    /// Creates an empty [`ModHotReloadHistory`] retaining up to `max_history.max(1)` outcomes.
    #[inline]
    pub fn with_max_history(max_history: usize) -> Self {
        let cap = max_history.max(1);
        Self {
            outcomes: VecDeque::with_capacity(cap),
            max_history: cap,
        }
    }

    /// Returns the maximum number of retained historical outcomes.
    #[inline]
    pub const fn max_history(&self) -> usize {
        self.max_history
    }

    /// Appends a single [`ModHotReloadOutcome`], evicting the oldest record when full.
    pub fn push(&mut self, outcome: ModHotReloadOutcome) {
        while self.outcomes.len() >= self.max_history {
            let _ = self.outcomes.pop_front();
        }
        self.outcomes.push_back(outcome);
    }

    /// Appends a batch of [`ModHotReloadOutcome`]s while enforcing the bounded ring capacity.
    pub fn extend_outcomes<I: IntoIterator<Item = ModHotReloadOutcome>>(&mut self, iter: I) {
        for item in iter {
            self.push(item);
        }
    }
}

#[derive(Default)]
struct PendingRetries {
    changes: Vec<PathBuf>,
    deletes: Vec<PathBuf>,
}

struct PollEventsBatch {
    changed: Vec<PathBuf>,
    deleted: Vec<PathBuf>,
    disconnect_outcome: Option<ModHotReloadOutcome>,
}

struct ApplyHotReloadResult {
    outcomes: Vec<ModHotReloadOutcome>,
    retry_changes: Vec<PathBuf>,
    retry_deletes: Vec<PathBuf>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct FileFingerprint {
    modified: Option<std::time::SystemTime>,
    len: u64,
    #[cfg(unix)]
    ino: u64,
}

impl FileFingerprint {
    fn from_path(path: &Path) -> Option<Self> {
        let meta = std::fs::metadata(path).ok()?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Some(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
            #[cfg(unix)]
            ino: meta.ino(),
        })
    }
}

fn canonical_key(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

struct SyncDirResult {
    outcomes: Vec<ModHotReloadOutcome>,
    retry_changes: Vec<PathBuf>,
    loaded_paths: Vec<PathBuf>,
}

/// Debounced filesystem watcher (`notify-debouncer-full`) monitoring a directory tree for
/// native mod shared libraries (`.so`, `.dylib`, `.dll`) and `mod.toml` manifests.
#[derive(Resource)]
pub struct ModHotReloadWatcher {
    watch_dir: PathBuf,
    load_options: LoadOptions,
    debounce: Duration,
    unload_on_delete: bool,
    stability_polls_required: usize,
    rx: Mutex<mpsc::Receiver<DebounceEventResult>>,
    pending_deletes: Mutex<HashMap<PathBuf, Instant>>,
    pending_retries: Mutex<PendingRetries>,
    pending_stability: Mutex<HashMap<PathBuf, (FileFingerprint, usize)>>,
    sync_fingerprints: Mutex<HashMap<PathBuf, (FileFingerprint, Instant)>>,
    disconnected_logged: AtomicBool,
    disconnected_outcome_recorded: AtomicBool,
    _debouncer: Mutex<Option<Debouncer<RecommendedWatcher, RecommendedCache>>>,
}

impl ModHotReloadWatcher {
    /// Starts watching `dir` recursively with a 200ms debounce window and
    /// `LoadOptions::default().with_shadow_copy(true)`.
    pub fn watch(dir: impl AsRef<Path>) -> Result<Self, notify_debouncer_full::notify::Error> {
        Self::watch_with_options(
            dir,
            Duration::from_millis(200),
            true,
            LoadOptions::default().with_shadow_copy(true),
        )
    }

    /// Starts watching `dir` with custom debounce duration, recursion mode, and [`LoadOptions`].
    pub fn watch_with_options(
        dir: impl AsRef<Path>,
        debounce: Duration,
        recursive: bool,
        load_options: LoadOptions,
    ) -> Result<Self, notify_debouncer_full::notify::Error> {
        let watch_dir = dir.as_ref().to_path_buf();
        let (tx, rx) = mpsc::channel::<DebounceEventResult>();

        let mut debouncer = new_debouncer(debounce, None, move |res: DebounceEventResult| {
            let _ = tx.send(res);
        })?;

        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        debouncer.watch(&watch_dir, mode)?;

        Ok(Self {
            watch_dir,
            load_options: load_options.with_shadow_copy(true),
            debounce,
            unload_on_delete: true,
            stability_polls_required: DEFAULT_STABILITY_POLLS,
            rx: Mutex::new(rx),
            pending_deletes: Mutex::new(HashMap::new()),
            pending_retries: Mutex::new(PendingRetries::default()),
            pending_stability: Mutex::new(HashMap::new()),
            sync_fingerprints: Mutex::new(HashMap::new()),
            disconnected_logged: AtomicBool::new(false),
            disconnected_outcome_recorded: AtomicBool::new(false),
            _debouncer: Mutex::new(Some(debouncer)),
        })
    }

    #[cfg(test)]
    pub(crate) fn from_receiver_for_test(
        watch_dir: impl AsRef<Path>,
        debounce: Duration,
        rx: mpsc::Receiver<DebounceEventResult>,
        load_options: LoadOptions,
    ) -> Self {
        Self {
            watch_dir: watch_dir.as_ref().to_path_buf(),
            load_options: load_options.with_shadow_copy(true),
            debounce,
            unload_on_delete: true,
            stability_polls_required: 1,
            rx: Mutex::new(rx),
            pending_deletes: Mutex::new(HashMap::new()),
            pending_retries: Mutex::new(PendingRetries::default()),
            pending_stability: Mutex::new(HashMap::new()),
            sync_fingerprints: Mutex::new(HashMap::new()),
            disconnected_logged: AtomicBool::new(false),
            disconnected_outcome_recorded: AtomicBool::new(false),
            _debouncer: Mutex::new(None),
        }
    }

    /// Configures how many consecutive identical `(size, modified, inode)` polls are required
    /// before a modified/created artifact is treated as write-complete (`>= 1`).
    #[must_use]
    pub fn with_stability_polls(mut self, polls: usize) -> Self {
        self.stability_polls_required = polls.max(1);
        self
    }

    /// Enables or disables automatic unloading when a watched mod binary or `mod.toml` is
    /// permanently deleted on disk (default `true`, guarded against atomic compiler renames).
    #[must_use]
    pub const fn with_unload_on_delete(mut self, enabled: bool) -> Self {
        self.unload_on_delete = enabled;
        self
    }

    /// Returns the root directory currently being watched.
    #[inline]
    pub fn watch_dir(&self) -> &Path {
        &self.watch_dir
    }

    /// Returns the [`LoadOptions`] applied when loading or reloading changed mods.
    #[inline]
    pub fn load_options(&self) -> &LoadOptions {
        &self.load_options
    }

    /// Drains all debounced filesystem events and returns deduplicated mod library or
    /// `mod.toml` paths that were created or modified.
    pub fn poll_changed_paths(&self) -> Vec<PathBuf> {
        let (changed, _deleted) = self.poll_events();
        changed
    }

    /// Drains debounced events, separating confirmed creates/modifies from deletions that have
    /// remained absent past the atomic-rename grace window (`self.debounce`).
    pub fn poll_events(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let batch = self.poll_events_inner(false);
        (batch.changed, batch.deleted)
    }

    fn poll_events_inner(&self, record_disconnect: bool) -> PollEventsBatch {
        let mut batches = Vec::new();
        let mut channel_disconnected = false;
        {
            let guard = self
                .rx
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                match guard.try_recv() {
                    Ok(batch) => batches.push(batch),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        channel_disconnected = true;
                        break;
                    }
                }
            }
        }

        let mut disconnect_outcome = None;
        if channel_disconnected {
            if !self.disconnected_logged.swap(true, Ordering::AcqRel) {
                tracing::error!(
                    dir = %self.watch_dir.display(),
                    "watcher channel disconnected"
                );
            }
            if record_disconnect
                && !self
                    .disconnected_outcome_recorded
                    .swap(true, Ordering::AcqRel)
            {
                disconnect_outcome = Some(ModHotReloadOutcome {
                    path: self.watch_dir.clone(),
                    mod_id: None,
                    action: ModHotReloadAction::Failed("watcher channel disconnected".to_string()),
                });
            }
        }

        let retries = {
            let mut guard = self
                .pending_retries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *guard)
        };

        let mut fingerprints = if batches.is_empty() {
            HashMap::new()
        } else {
            let mut guard = self
                .sync_fingerprints
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            guard.retain(|_, (_, recorded_at)| {
                now.duration_since(*recorded_at) < Duration::from_secs(2)
            });
            std::mem::take(&mut *guard)
        };

        let mut changed = Vec::new();
        let mut seen_changed = HashSet::new();
        let mut deleted = Vec::new();
        let mut seen_deleted = HashSet::new();

        let mut pending = self
            .pending_deletes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let now = Instant::now();
        for batch in batches {
            let events = match batch {
                Ok(events) => events,
                Err(errors) => {
                    for err in errors {
                        tracing::warn!(
                            dir = %self.watch_dir.display(),
                            error = %err,
                            "filesystem watcher reported error"
                        );
                    }
                    continue;
                }
            };
            for ev in events {
                let is_remove = matches!(
                    ev.kind,
                    EventKind::Remove(RemoveKind::File | RemoveKind::Any)
                );
                let is_change = is_relevant_event_kind(ev.kind);
                if !is_remove && !is_change {
                    continue;
                }
                for p in &ev.paths {
                    if !is_watchable_mod_path(p) {
                        continue;
                    }
                    if is_remove && !p.is_file() {
                        fingerprints.remove(&canonical_key(p));
                        pending.entry(p.clone()).or_insert_with(Instant::now);
                    } else if p.is_file() {
                        let key = canonical_key(p);
                        let matches_sync =
                            fingerprints.get(&key).is_some_and(|(fp, recorded_at)| {
                                now.duration_since(*recorded_at) < Duration::from_secs(2)
                                    && FileFingerprint::from_path(p) == Some(*fp)
                            });
                        if matches_sync {
                            continue;
                        }
                        fingerprints.remove(&key);
                        pending.remove(p);
                        if seen_changed.insert(p.clone()) {
                            changed.push(p.clone());
                        }
                    }
                }
            }
        }
        drop(pending);

        if !fingerprints.is_empty() {
            let mut guard = self
                .sync_fingerprints
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for (k, v) in fingerprints {
                guard.entry(k).or_insert(v);
            }
        }

        let mut pending = self
            .pending_deletes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut resolved_keys = Vec::new();
        for (path, first_seen) in pending.iter() {
            if path.is_file() {
                resolved_keys.push((path.clone(), true));
            } else if now.duration_since(*first_seen) >= self.debounce {
                resolved_keys.push((path.clone(), false));
            }
        }
        for (path, recreated) in resolved_keys {
            pending.remove(&path);
            if recreated {
                if seen_changed.insert(path.clone()) {
                    changed.push(path);
                }
            } else if self.unload_on_delete && seen_deleted.insert(path.clone()) {
                deleted.push(path);
            }
        }

        if self.stability_polls_required > 1 {
            let mut stability = self
                .pending_stability
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let mut ready = Vec::new();

            for path in &changed {
                if let Some(fp) = FileFingerprint::from_path(path) {
                    let count = match stability.get(path) {
                        Some(&(prev_fp, prev_count)) if prev_fp == fp => prev_count + 1,
                        _ => 1,
                    };
                    if count >= self.stability_polls_required {
                        stability.remove(path);
                        ready.push(path.clone());
                    } else {
                        stability.insert(path.clone(), (fp, count));
                    }
                }
            }

            let existing_keys: Vec<PathBuf> = stability
                .keys()
                .filter(|p| !seen_changed.contains(*p))
                .cloned()
                .collect();
            for path in existing_keys {
                if !path.is_file() {
                    stability.remove(&path);
                    continue;
                }
                if let Some(fp) = FileFingerprint::from_path(&path) {
                    let count = match stability.get(&path) {
                        Some(&(prev_fp, prev_count)) if prev_fp == fp => prev_count + 1,
                        _ => 1,
                    };
                    if count >= self.stability_polls_required {
                        stability.remove(&path);
                        if seen_changed.insert(path.clone()) {
                            ready.push(path);
                        }
                    } else {
                        stability.insert(path, (fp, count));
                    }
                }
            }
            for del in &deleted {
                stability.remove(del);
            }
            changed = ready;
        }

        for path in retries.changes {
            if path.is_file() && !pending.contains_key(&path) && !changed.contains(&path) {
                changed.push(path);
            }
        }
        for path in retries.deletes {
            if !path.is_file()
                && !pending.contains_key(&path)
                && !seen_changed.contains(&path)
                && self.unload_on_delete
                && seen_deleted.insert(path.clone())
            {
                deleted.push(path);
            }
        }

        PollEventsBatch {
            changed,
            deleted,
            disconnect_outcome,
        }
    }

    fn requeue_pending(&self, changes: Vec<PathBuf>, deletes: Vec<PathBuf>) {
        if changes.is_empty() && deletes.is_empty() {
            return;
        }
        let mut guard = self
            .pending_retries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for path in changes {
            if !guard.changes.contains(&path) {
                guard.changes.push(path);
            }
        }
        for path in deletes {
            if !guard.deletes.contains(&path) {
                guard.deletes.push(path);
            }
        }
    }

    fn record_outcome_fingerprints(&self, outcomes: &[ModHotReloadOutcome]) {
        let mut to_record = Vec::new();
        let mut to_remove = Vec::new();
        for outcome in outcomes {
            match outcome.action {
                ModHotReloadAction::Loaded | ModHotReloadAction::Reloaded => {
                    to_record.push(outcome.path.clone());
                    if let Some(dir) = outcome.path.parent() {
                        let manifest = dir.join("mod.toml");
                        if manifest.is_file() {
                            to_record.push(manifest);
                        }
                    }
                }
                ModHotReloadAction::Unloaded => {
                    to_remove.push(outcome.path.clone());
                }
                ModHotReloadAction::Failed(_) => {}
            }
        }
        if to_record.is_empty() && to_remove.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut guard = self
            .sync_fingerprints
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for path in to_remove {
            guard.remove(&canonical_key(&path));
        }
        for path in to_record {
            if let Some(fp) = FileFingerprint::from_path(&path) {
                guard.insert(canonical_key(&path), (fp, now));
            }
        }
    }

    /// Performs an incremental topological load of all currently unloaded `mod.toml` mods in
    /// `self.watch_dir()`, leaving already-loaded mods untouched.
    pub fn sync_dir(&self, world: &mut World) -> Vec<ModHotReloadOutcome> {
        let sync = load_unloaded_dag_mods(world, &self.watch_dir, &self.load_options);
        if !sync.loaded_paths.is_empty() {
            let now = Instant::now();
            let mut guard = self
                .sync_fingerprints
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for path in sync.loaded_paths {
                if let Some(fp) = FileFingerprint::from_path(&path) {
                    guard.insert(canonical_key(&path), (fp, now));
                }
            }
        }
        if !sync.retry_changes.is_empty() {
            self.requeue_pending(sync.retry_changes, Vec::new());
        }
        sync.outcomes
    }

    /// Drains pending debounced changes/deletions and loads, reloads (cascading via
    /// [`ModDependencyGraph`]), or unloads affected mods in `world`.
    pub fn process_pending_reloads(&self, world: &mut World) -> Vec<ModHotReloadOutcome> {
        let poll = self.poll_events_inner(true);
        let mut outcomes = Vec::new();
        if let Some(disconnect_outcome) = poll.disconnect_outcome {
            outcomes.push(disconnect_outcome);
        }
        if !poll.changed.is_empty() || !poll.deleted.is_empty() {
            let batch = apply_hot_reload_events(
                world,
                &self.watch_dir,
                &poll.changed,
                &poll.deleted,
                &self.load_options,
            );
            self.record_outcome_fingerprints(&batch.outcomes);
            outcomes.extend(batch.outcomes);
            self.requeue_pending(batch.retry_changes, batch.retry_deletes);
        }
        outcomes
    }
}

/// Bevy [`Plugin`] installing [`ModHotReloadWatcher`], ensuring [`ModLifecycleFlush`] is
/// scheduled, and polling the watcher during [`ModLifecycleFlush`].
#[derive(Clone, Debug)]
pub struct ModHotReloadPlugin {
    watch_dir: PathBuf,
    debounce: Duration,
    recursive: bool,
    unload_on_delete: bool,
    sync_on_start: bool,
    max_history: usize,
    load_options: LoadOptions,
}

impl ModHotReloadPlugin {
    /// Creates a [`ModHotReloadPlugin`] watching `dir` recursively with a 200ms debounce window.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self {
            watch_dir: dir.as_ref().to_path_buf(),
            debounce: Duration::from_millis(200),
            recursive: true,
            unload_on_delete: true,
            sync_on_start: false,
            max_history: DEFAULT_MAX_HOT_RELOAD_HISTORY,
            load_options: LoadOptions::default().with_shadow_copy(true),
        }
    }

    /// Overrides the filesystem debounce duration.
    #[must_use]
    pub const fn with_debounce(mut self, debounce: Duration) -> Self {
        self.debounce = debounce;
        self
    }

    /// Enables or disables recursive directory watching.
    #[must_use]
    pub const fn with_recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        self
    }

    /// Enables or disables unloading mods when their binary or `mod.toml` is deleted on disk.
    #[must_use]
    pub const fn with_unload_on_delete(mut self, enabled: bool) -> Self {
        self.unload_on_delete = enabled;
        self
    }

    /// When `true`, performs an initial topological `sync_dir` load during plugin build (before
    /// any frame schedules are leased, so initial `Startup` mod systems schedule into `Startup`).
    #[must_use]
    pub const fn with_sync_on_start(mut self, enabled: bool) -> Self {
        self.sync_on_start = enabled;
        self
    }

    /// Configures the bounded ring-buffer capacity for [`ModHotReloadHistory`] (`>= 1`).
    #[must_use]
    pub fn with_max_history(mut self, max_history: usize) -> Self {
        self.max_history = max_history.max(1);
        self
    }

    /// Overrides the [`LoadOptions`] used when loading or reloading mods.
    #[must_use]
    pub fn with_load_options(mut self, load_options: LoadOptions) -> Self {
        self.load_options = load_options.with_shadow_copy(true);
        self
    }
}

impl Plugin for ModHotReloadPlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<ModHotReloadHistory>() {
            app.insert_resource(ModHotReloadHistory::with_max_history(self.max_history));
        }
        ensure_mod_lifecycle_flush_schedule(app);
        match ModHotReloadWatcher::watch_with_options(
            &self.watch_dir,
            self.debounce,
            self.recursive,
            self.load_options.clone(),
        ) {
            Ok(watcher) => {
                let watcher = watcher.with_unload_on_delete(self.unload_on_delete);
                if self.sync_on_start {
                    let initial = watcher.sync_dir(app.world_mut());
                    if let Some(mut hist) =
                        app.world_mut().get_resource_mut::<ModHotReloadHistory>()
                    {
                        hist.extend_outcomes(initial);
                    }
                }
                app.insert_resource(watcher);
                app.add_systems(ModLifecycleFlush, poll_hot_reload_watcher_system);
            }
            Err(err) => {
                tracing::error!(
                    dir = %self.watch_dir.display(),
                    error = %err,
                    "failed to initialize ModHotReloadWatcher"
                );
                if let Some(mut hist) = app.world_mut().get_resource_mut::<ModHotReloadHistory>() {
                    hist.push(ModHotReloadOutcome {
                        path: self.watch_dir.clone(),
                        mod_id: None,
                        action: ModHotReloadAction::Failed(format!(
                            "failed to initialize ModHotReloadWatcher: {err}"
                        )),
                    });
                }
            }
        }
    }
}

fn poll_hot_reload_watcher_system(world: &mut World) {
    let Some((poll, watch_dir, options)) = world.get_resource::<ModHotReloadWatcher>().map(|w| {
        (
            w.poll_events_inner(true),
            w.watch_dir().to_path_buf(),
            w.load_options().clone(),
        )
    }) else {
        return;
    };

    let mut outcomes = Vec::new();
    if let Some(disconnect_outcome) = poll.disconnect_outcome {
        outcomes.push(disconnect_outcome);
    }

    if !poll.changed.is_empty() || !poll.deleted.is_empty() {
        let batch =
            apply_hot_reload_events(world, &watch_dir, &poll.changed, &poll.deleted, &options);
        if let Some(watcher) = world.get_resource::<ModHotReloadWatcher>() {
            watcher.record_outcome_fingerprints(&batch.outcomes);
            if !batch.retry_changes.is_empty() || !batch.retry_deletes.is_empty() {
                watcher.requeue_pending(batch.retry_changes, batch.retry_deletes);
            }
        }
        outcomes.extend(batch.outcomes);
    }

    if !outcomes.is_empty() {
        if !world.contains_resource::<ModHotReloadHistory>() {
            world.init_resource::<ModHotReloadHistory>();
        }
        if let Some(mut history) = world.get_resource_mut::<ModHotReloadHistory>() {
            history.extend_outcomes(outcomes);
        }
    }
}

fn is_schedule_leased_load_error(err: &ModLoadError) -> bool {
    matches!(err, ModLoadError::ScheduleLeased { .. })
}

fn is_schedule_leased_dag_error(err: &ModDagError) -> bool {
    matches!(
        err,
        ModDagError::LoadFailed { source, .. } if is_schedule_leased_load_error(source)
    )
}

fn is_relevant_event_kind(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(CreateKind::File | CreateKind::Any)
            | EventKind::Modify(
                ModifyKind::Data(_)
                    | ModifyKind::Metadata(_)
                    | ModifyKind::Name(_)
                    | ModifyKind::Any
            )
    )
}

fn is_dylib_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            ext.eq_ignore_ascii_case("so")
                || ext.eq_ignore_ascii_case("dylib")
                || ext.eq_ignore_ascii_case("dll")
        })
}

fn is_sig_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("sig"))
}

fn is_bampack_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("bampack"))
}

fn is_schedule_delta_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| name.ends_with(".schedule_delta.json"))
}

fn is_watchable_mod_path(path: &Path) -> bool {
    is_dylib_path(path)
        || is_manifest_path(path)
        || is_sig_path(path)
        || is_bampack_path(path)
        || is_schedule_delta_path(path)
}

fn has_valid_native_binary_magic(path: &Path) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|err| {
        format!(
            "failed to open '{}' for header check: {err}",
            path.display()
        )
    })?;
    let mut hdr = [0u8; 4];
    let n = file
        .read(&mut hdr)
        .map_err(|err| format!("failed to read '{}' header: {err}", path.display()))?;
    if n < 2 {
        return Err(format!(
            "mod binary '{}' is truncated ({n} bytes)",
            path.display()
        ));
    }
    if hdr[..2] == *b"MZ" {
        return Ok(());
    }
    if n < 4 {
        return Err(format!(
            "mod binary '{}' is truncated ({n} bytes)",
            path.display()
        ));
    }
    let magic_be = u32::from_be_bytes(hdr);
    let is_elf = hdr == *b"\x7fELF";
    let is_macho = matches!(
        magic_be,
        0xFEED_FACE
            | 0xCEFA_EDFE
            | 0xFEED_FACF
            | 0xCFFA_EDFE
            | 0xCAFE_BABE
            | 0xBEBA_FECA
            | 0xCAFE_BABF
            | 0xBFBA_FECA
    );
    if is_elf || is_macho {
        Ok(())
    } else {
        Err(format!(
            "mod binary '{}' has invalid ELF/Mach-O/PE magic header",
            path.display()
        ))
    }
}

fn has_valid_bampack_magic(path: &Path) -> Result<(), String> {
    let mut file = std::fs::File::open(path).map_err(|err| {
        format!(
            "failed to open '{}' for bampack header check: {err}",
            path.display()
        )
    })?;
    let mut hdr = [0u8; 8];
    let n = file
        .read(&mut hdr)
        .map_err(|err| format!("failed to read '{}' bampack header: {err}", path.display()))?;
    #[cfg(feature = "signature-verification")]
    let expected = *BAMPACK_MAGIC;
    #[cfg(not(feature = "signature-verification"))]
    let expected = *b"BAMPACK\0";
    if n == 8 && hdr == expected {
        Ok(())
    } else {
        Err(format!(
            "bampack archive '{}' has invalid BAMPACK magic header",
            path.display()
        ))
    }
}

fn resolve_sig_target_path(world: &World, sig_path: &Path) -> Option<PathBuf> {
    let candidate = sig_path.with_extension("");
    if candidate.is_file() && (is_dylib_path(&candidate) || is_bampack_path(&candidate)) {
        return Some(candidate);
    }
    if let Some(host) = world.get_resource::<ModHost>() {
        for m in host.loaded_mods() {
            if !m.is_active() {
                continue;
            }
            let src = m.source_path();
            if same_path(
                &src.with_extension(format!(
                    "{}.sig",
                    src.extension().and_then(|e| e.to_str()).unwrap_or("")
                )),
                sig_path,
            ) {
                return Some(src.to_path_buf());
            }
        }
    }
    None
}

struct JsonCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonCursor<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            bytes: input.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if b.is_ascii_whitespace() {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.bytes.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let b = self.peek()?;
        self.pos += 1;
        Some(b)
    }

    fn parse_string(&mut self) -> Result<String, String> {
        if self.next() != Some(b'"') {
            return Err("expected JSON string".to_string());
        }
        let mut out = String::new();
        while let Some(&b) = self.bytes.get(self.pos) {
            self.pos += 1;
            match b {
                b'"' => return Ok(out),
                b'\\' => {
                    let esc = self
                        .bytes
                        .get(self.pos)
                        .copied()
                        .ok_or_else(|| "unterminated string escape".to_string())?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        _ => return Err(format!("unsupported escape '\\{}'", esc as char)),
                    }
                }
                _ => out.push(b as char),
            }
        }
        Err("unterminated JSON string".to_string())
    }

    fn parse_bool(&mut self) -> Result<bool, String> {
        self.skip_ws();
        if self.bytes.get(self.pos..self.pos + 4) == Some(b"true") {
            self.pos += 4;
            Ok(true)
        } else if self.bytes.get(self.pos..self.pos + 5) == Some(b"false") {
            self.pos += 5;
            Ok(false)
        } else {
            Err("expected JSON boolean".to_string())
        }
    }

    fn parse_u32(&mut self) -> Result<u32, String> {
        self.skip_ws();
        let start = self.pos;
        while let Some(&b) = self.bytes.get(self.pos) {
            if b.is_ascii_digit() {
                self.pos += 1;
            } else {
                break;
            }
        }
        let slice = self
            .bytes
            .get(start..self.pos)
            .ok_or_else(|| "invalid integer span".to_string())?;
        let text = core::str::from_utf8(slice).map_err(|_| "invalid UTF-8 integer".to_string())?;
        text.parse::<u32>()
            .map_err(|_| format!("invalid u32 integer '{text}'"))
    }

    fn parse_string_array(&mut self) -> Result<Vec<String>, String> {
        if self.next() != Some(b'[') {
            return Err("expected '[' for string array".to_string());
        }
        let mut items = Vec::new();
        if self.peek() == Some(b']') {
            let _ = self.next();
            return Ok(items);
        }
        loop {
            items.push(self.parse_string()?);
            match self.next() {
                Some(b',') => continue,
                Some(b']') => return Ok(items),
                _ => return Err("expected ',' or ']' in string array".to_string()),
            }
        }
    }

    fn skip_value(&mut self) -> Result<(), String> {
        match self.peek() {
            Some(b'"') => {
                let _ = self.parse_string()?;
                Ok(())
            }
            Some(b'{') => {
                let _ = self.next();
                if self.peek() == Some(b'}') {
                    let _ = self.next();
                    return Ok(());
                }
                loop {
                    let _ = self.parse_string()?;
                    if self.next() != Some(b':') {
                        return Err("expected ':' in JSON object".to_string());
                    }
                    self.skip_value()?;
                    match self.next() {
                        Some(b',') => continue,
                        Some(b'}') => return Ok(()),
                        _ => return Err("expected ',' or '}' in JSON object".to_string()),
                    }
                }
            }
            Some(b'[') => {
                let _ = self.next();
                if self.peek() == Some(b']') {
                    let _ = self.next();
                    return Ok(());
                }
                loop {
                    self.skip_value()?;
                    match self.next() {
                        Some(b',') => continue,
                        Some(b']') => return Ok(()),
                        _ => return Err("expected ',' or ']' in JSON array".to_string()),
                    }
                }
            }
            Some(b't') | Some(b'f') => {
                let _ = self.parse_bool()?;
                Ok(())
            }
            Some(b'n') => {
                if self.bytes.get(self.pos..self.pos + 4) == Some(b"null") {
                    self.pos += 4;
                    Ok(())
                } else {
                    Err("invalid JSON literal".to_string())
                }
            }
            Some(b'-') | Some(b'0'..=b'9') => {
                if self.bytes.get(self.pos) == Some(&b'-') {
                    self.pos += 1;
                }
                while let Some(&b) = self.bytes.get(self.pos) {
                    if b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-') {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                Ok(())
            }
            _ => Err("unexpected JSON token".to_string()),
        }
    }
}

#[derive(Default)]
struct ScheduleDeltaSetEntry {
    schedule_override: Option<ScheduleId>,
    set: String,
    mode: Option<ScheduleSetMode>,
    before: Option<Vec<String>>,
    after: Option<Vec<String>>,
    before_weak: Option<bool>,
    after_weak: Option<bool>,
    parent_sets: Option<Vec<String>>,
}

struct ScheduleDeltaDocument {
    schedule: ScheduleId,
    sets: Vec<ScheduleDeltaSetEntry>,
}

fn parse_schedule_id_from_cursor(cur: &mut JsonCursor<'_>) -> Result<ScheduleId, String> {
    if cur.peek() == Some(b'"') {
        let name = cur.parse_string()?;
        match name.as_str() {
            "Update" | "update" => Ok(ScheduleId::UPDATE),
            "Startup" | "startup" => Ok(ScheduleId::STARTUP),
            "PreUpdate" | "pre_update" => Ok(ScheduleId::PRE_UPDATE),
            "PostUpdate" | "post_update" => Ok(ScheduleId::POST_UPDATE),
            "FixedUpdate" | "fixed_update" => Ok(ScheduleId::FIXED_UPDATE),
            "Last" | "last" => Ok(ScheduleId::LAST),
            _ => Err(format!("unknown schedule name '{name}'")),
        }
    } else {
        Ok(ScheduleId(cur.parse_u32()?))
    }
}

fn parse_schedule_mode(raw: &str) -> Result<ScheduleSetMode, String> {
    match raw.to_ascii_lowercase().as_str() {
        "parallel" => Ok(ScheduleSetMode::Parallel),
        "sequence" => Ok(ScheduleSetMode::Sequence),
        "sequence_weak" | "sequenceweak" => Ok(ScheduleSetMode::SequenceWeak),
        _ => Err(format!("unknown schedule set mode '{raw}'")),
    }
}

fn parse_delta_set_object(cur: &mut JsonCursor<'_>) -> Result<ScheduleDeltaSetEntry, String> {
    if cur.next() != Some(b'{') {
        return Err("expected '{' for schedule delta set entry".to_string());
    }
    let mut entry = ScheduleDeltaSetEntry::default();
    if cur.peek() == Some(b'}') {
        let _ = cur.next();
        return Err("schedule delta set entry is missing 'set'".to_string());
    }
    loop {
        let key = cur.parse_string()?;
        if cur.next() != Some(b':') {
            return Err("expected ':' after key".to_string());
        }
        match key.as_str() {
            "schedule" => entry.schedule_override = Some(parse_schedule_id_from_cursor(cur)?),
            "set" | "name" => entry.set = cur.parse_string()?,
            "mode" => {
                let mode_str = cur.parse_string()?;
                entry.mode = Some(parse_schedule_mode(&mode_str)?);
            }
            "before" | "before_sets" => entry.before = Some(cur.parse_string_array()?),
            "after" | "after_sets" => entry.after = Some(cur.parse_string_array()?),
            "before_weak" => entry.before_weak = Some(cur.parse_bool()?),
            "after_weak" => entry.after_weak = Some(cur.parse_bool()?),
            "parent_sets" => entry.parent_sets = Some(cur.parse_string_array()?),
            _ => cur.skip_value()?,
        }
        match cur.next() {
            Some(b',') => continue,
            Some(b'}') => break,
            _ => return Err("expected ',' or '}' in schedule delta entry".to_string()),
        }
    }
    if entry.set.is_empty() {
        return Err("schedule delta set entry has empty 'set'".to_string());
    }
    Ok(entry)
}

fn parse_schedule_delta_json(input: &str) -> Result<ScheduleDeltaDocument, String> {
    let mut cur = JsonCursor::new(input);
    if cur.next() != Some(b'{') {
        return Err("schedule delta JSON root must be an object".to_string());
    }
    let mut schedule = ScheduleId::UPDATE;
    let mut sets = Vec::new();
    let mut top_single = ScheduleDeltaSetEntry::default();

    if cur.peek() != Some(b'}') {
        loop {
            let key = cur.parse_string()?;
            if cur.next() != Some(b':') {
                return Err("expected ':' after key".to_string());
            }
            match key.as_str() {
                "schedule" => schedule = parse_schedule_id_from_cursor(&mut cur)?,
                "sets" => {
                    if cur.next() != Some(b'[') {
                        return Err("expected '[' for 'sets'".to_string());
                    }
                    if cur.peek() != Some(b']') {
                        loop {
                            sets.push(parse_delta_set_object(&mut cur)?);
                            match cur.next() {
                                Some(b',') => continue,
                                Some(b']') => break,
                                _ => return Err("expected ',' or ']' in 'sets'".to_string()),
                            }
                        }
                    } else {
                        let _ = cur.next();
                    }
                }
                "set" | "name" => top_single.set = cur.parse_string()?,
                "mode" => {
                    let s = cur.parse_string()?;
                    top_single.mode = Some(parse_schedule_mode(&s)?);
                }
                "before" | "before_sets" => top_single.before = Some(cur.parse_string_array()?),
                "after" | "after_sets" => top_single.after = Some(cur.parse_string_array()?),
                "before_weak" => top_single.before_weak = Some(cur.parse_bool()?),
                "after_weak" => top_single.after_weak = Some(cur.parse_bool()?),
                "parent_sets" => top_single.parent_sets = Some(cur.parse_string_array()?),
                _ => cur.skip_value()?,
            }
            match cur.next() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err("expected ',' or '}' at JSON root".to_string()),
            }
        }
    } else {
        let _ = cur.next();
    }

    if sets.is_empty() && !top_single.set.is_empty() {
        sets.push(top_single);
    }
    if sets.is_empty() {
        return Err("schedule delta JSON defines no sets".to_string());
    }
    Ok(ScheduleDeltaDocument { schedule, sets })
}

enum DeltaApplyError {
    Leased,
    Failed(String),
}

fn find_set_handle_by_name(
    registry: &ModScheduleSetRegistry,
    schedule: ScheduleId,
    raw_name: &str,
) -> Option<bamabi_sys::BamSystemSet> {
    if let Some((mod_part, set_part)) = raw_name.split_once("::") {
        registry
            .sets_for_schedule(schedule)
            .find(|e| e.mod_name.as_ref() == mod_part && e.set_name.as_ref() == set_part)
            .map(|e| e.handle)
    } else {
        let mut matches = registry
            .sets_for_schedule(schedule)
            .filter(|e| e.set_name.as_ref() == raw_name);
        let first = matches.next()?.handle;
        if matches.next().is_some() {
            None
        } else {
            Some(first)
        }
    }
}

fn apply_schedule_delta_file(world: &mut World, path: &Path) -> Result<(), DeltaApplyError> {
    let contents = std::fs::read_to_string(path).map_err(|err| {
        DeltaApplyError::Failed(format!("failed to read '{}': {err}", path.display()))
    })?;
    let doc = parse_schedule_delta_json(&contents).map_err(DeltaApplyError::Failed)?;

    let mut target_schedules = Vec::new();
    for entry in &doc.sets {
        let sid = entry.schedule_override.unwrap_or(doc.schedule);
        if !target_schedules.contains(&sid) {
            target_schedules.push(sid);
        }
    }

    let leased = leased_schedules(world);
    if !leased.is_empty() {
        for &sid in &target_schedules {
            if let Some(label) = resolve_schedule_id_in_world(world, sid)
                && leased.contains(&label)
            {
                return Err(DeltaApplyError::Leased);
            }
        }
    }

    let Some(original_reg) = world.get_resource::<ModScheduleSetRegistry>().cloned() else {
        return Err(DeltaApplyError::Failed(
            "ModScheduleSetRegistry resource not present in World".to_string(),
        ));
    };
    let mut staged_reg = original_reg.clone();

    for delta in doc.sets {
        let sid = delta.schedule_override.unwrap_or(doc.schedule);
        let handle = find_set_handle_by_name(&staged_reg, sid, &delta.set).ok_or_else(|| {
            DeltaApplyError::Failed(format!(
                "schedule set '{}' not found in schedule {}",
                delta.set, sid.0
            ))
        })?;
        let Some(existing) = staged_reg.get(handle).cloned() else {
            return Err(DeltaApplyError::Failed(format!(
                "schedule set handle {} missing",
                handle.0
            )));
        };

        if let Some(mode) = delta.mode {
            staged_reg
                .set_mode(handle, mode)
                .map_err(|err| DeltaApplyError::Failed(format!("set_mode failed ({err})")))?;
        }
        if delta.before.is_some()
            || delta.after.is_some()
            || delta.before_weak.is_some()
            || delta.after_weak.is_some()
        {
            let before_handles = match delta.before {
                Some(ref names) => {
                    let mut out = Vec::with_capacity(names.len());
                    for n in names {
                        let h = find_set_handle_by_name(&staged_reg, sid, n).ok_or_else(|| {
                            DeltaApplyError::Failed(format!("unknown before target set '{n}'"))
                        })?;
                        out.push(h);
                    }
                    out
                }
                None => existing.before_sets.clone(),
            };
            let after_handles = match delta.after {
                Some(ref names) => {
                    let mut out = Vec::with_capacity(names.len());
                    for n in names {
                        let h = find_set_handle_by_name(&staged_reg, sid, n).ok_or_else(|| {
                            DeltaApplyError::Failed(format!("unknown after target set '{n}'"))
                        })?;
                        out.push(h);
                    }
                    out
                }
                None => existing.after_sets.clone(),
            };
            staged_reg
                .set_edges(
                    handle,
                    before_handles,
                    after_handles,
                    delta.before_weak.unwrap_or(existing.before_weak),
                    delta.after_weak.unwrap_or(existing.after_weak),
                )
                .map_err(|err| DeltaApplyError::Failed(format!("set_edges failed ({err})")))?;
        }
        if let Some(parents) = delta.parent_sets {
            for parent_raw in &parents {
                if let Some(stripped) = parent_raw.strip_prefix("host::") {
                    let valid_host = world
                        .get_resource::<ModVisibleSets>()
                        .is_some_and(|sets| sets.resolve(stripped).is_some());
                    if !valid_host {
                        return Err(DeltaApplyError::Failed(format!(
                            "unknown host parent set '{parent_raw}' in schedule {}",
                            sid.0
                        )));
                    }
                } else {
                    let Some(parsed) =
                        ModNamedSystemSet::parse_target(parent_raw, &existing.mod_name)
                    else {
                        return Err(DeltaApplyError::Failed(format!(
                            "invalid parent set target '{parent_raw}' in schedule {}",
                            sid.0
                        )));
                    };
                    let exists_in_reg = staged_reg.sets_for_schedule(sid).any(|cand| {
                        cand.mod_name == parsed.mod_name && cand.set_name == parsed.system_name
                    });
                    if !exists_in_reg {
                        return Err(DeltaApplyError::Failed(format!(
                            "unknown parent set '{parent_raw}' in schedule {}",
                            sid.0
                        )));
                    }
                }
            }
            if let Some(entry_mut) = staged_reg.get_mut(handle) {
                entry_mut.parent_sets = parents;
            }
        }
    }

    for &sid in &target_schedules {
        if let Err((_code, msg)) = staged_reg.validate_parent_sets_in_schedule(sid) {
            return Err(DeltaApplyError::Failed(msg));
        }
        if let Err(err) = prevalidate_mod_schedule_set_rewire_in_world(world, &staged_reg, sid) {
            return Err(DeltaApplyError::Failed(format!(
                "schedule pre-validation failed: {err}"
            )));
        }
    }

    if let Some(mut set_reg) = world.get_resource_mut::<ModScheduleSetRegistry>() {
        *set_reg = staged_reg;
    }

    let mut rewired_sids = Vec::with_capacity(target_schedules.len());
    for sid in target_schedules {
        if let Err(err) = rewire_mod_schedule_sets_in_world(world, sid) {
            if let Some(mut set_reg) = world.get_resource_mut::<ModScheduleSetRegistry>() {
                *set_reg = original_reg;
            }
            for prev_sid in rewired_sids {
                let _ = rewire_mod_schedule_sets_in_world(world, prev_sid);
            }
            return Err(DeltaApplyError::Failed(format!(
                "schedule rewire failed: {err}"
            )));
        }
        rewired_sids.push(sid);
    }
    Ok(())
}

fn is_manifest_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == "mod.toml")
}

fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

fn find_loaded_mod_for_dylib(world: &World, lib_path: &Path) -> Option<(ModId, String)> {
    let host = world.get_resource::<ModHost>()?;
    let target_stem = canonical_mod_name_from_path(lib_path);

    for m in host.loaded_mods() {
        if !m.is_active() {
            continue;
        }
        if same_path(m.source_path(), lib_path) {
            return Some((m.id(), m.name().to_string()));
        }
        if canonical_mod_name_from_path(m.source_path()) == target_stem
            && m.source_path().parent() == lib_path.parent()
        {
            return Some((m.id(), m.name().to_string()));
        }
    }
    None
}

fn find_loaded_mod_for_manifest_dir(
    world: &World,
    manifest_path: &Path,
    parsed_name: Option<&str>,
) -> Option<(ModId, String, PathBuf)> {
    let host = world.get_resource::<ModHost>()?;
    let dir = manifest_path.parent()?;
    for m in host.loaded_mods() {
        if !m.is_active() {
            continue;
        }
        if let Some(name) = parsed_name
            && m.name() == name
        {
            return Some((m.id(), m.name().to_string(), m.source_path().to_path_buf()));
        }
        if let Some(src_dir) = m.source_path().parent()
            && same_path(src_dir, dir)
        {
            return Some((m.id(), m.name().to_string(), m.source_path().to_path_buf()));
        }
    }
    None
}

type DiscoveredDirResult = Result<(Vec<ModTomlManifest>, Vec<ModDagError>), ModDagError>;

#[cfg(test)]
fn validate_dir_dag_for_reload(
    world: &World,
    watch_dir: &Path,
    target_mod_name: &str,
    target_manifest_path: Option<&Path>,
) -> Result<Option<ModTomlManifest>, String> {
    let discovery = discover_mod_manifests(watch_dir);
    validate_dir_dag_for_reload_from_discovery(
        world,
        &discovery,
        target_mod_name,
        target_manifest_path,
    )
}

fn validate_dir_dag_for_reload_from_discovery(
    world: &World,
    discovery: &DiscoveredDirResult,
    target_mod_name: &str,
    target_manifest_path: Option<&Path>,
) -> Result<Option<ModTomlManifest>, String> {
    let (manifests, parse_errors) = match discovery {
        Ok((manifests, errs)) => (manifests.clone(), errs),
        Err(err) => return Err(err.to_string()),
    };

    if let Some(t_path) = target_manifest_path {
        for err in parse_errors {
            let err_str = err.to_string();
            if err_str.contains(&t_path.display().to_string()) {
                return Err(err_str);
            }
        }
    }
    if !parse_errors.is_empty() && manifests.is_empty() {
        return Err(parse_errors[0].to_string());
    }

    let target_manifest = manifests
        .iter()
        .find(|m| m.name == target_mod_name)
        .cloned();

    // Include any currently loaded mods outside `watch_dir` in `already_loaded` so external
    // SemVer dependencies resolve properly.
    let manifest_names: HashSet<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
    let mut external_loaded = BTreeMap::new();
    if let Some(host) = world.get_resource::<ModHost>() {
        for m in host.loaded_mods() {
            if m.is_active()
                && !manifest_names.contains(m.name())
                && let Ok(ver) = semver::Version::parse(m.version())
            {
                external_loaded.insert(m.name().to_string(), ver);
            }
        }
    }

    let resolution = resolve_dependency_dag(manifests, &external_loaded);
    if resolution
        .quarantined_cycles
        .iter()
        .any(|name| name == target_mod_name)
    {
        return Err(format!(
            "mod '{target_mod_name}' participates in a dependency cycle: {}",
            resolution.quarantined_cycles.join(", ")
        ));
    }

    for diag in &resolution.diagnostics {
        let msg = diag.to_string();
        if msg.contains(&format!("'{target_mod_name}'")) {
            return Err(msg);
        }
    }

    if target_manifest.is_some()
        && !resolution
            .load_order
            .iter()
            .any(|m| m.name == target_mod_name)
    {
        return Err(format!(
            "mod '{target_mod_name}' failed DAG dependency validation"
        ));
    }

    Ok(target_manifest)
}

fn load_unloaded_dag_mods(
    world: &mut World,
    watch_dir: &Path,
    load_options: &LoadOptions,
) -> SyncDirResult {
    let discovery = discover_mod_manifests(watch_dir);
    load_unloaded_dag_mods_from_discovery(world, watch_dir, load_options, &discovery)
}

fn load_unloaded_dag_mods_from_discovery(
    world: &mut World,
    watch_dir: &Path,
    load_options: &LoadOptions,
    discovery: &DiscoveredDirResult,
) -> SyncDirResult {
    let mut outcomes = Vec::new();
    let mut retry_changes = Vec::new();
    let mut loaded_paths = Vec::new();
    let (manifests, parse_errors) = match discovery {
        Ok((manifests, parse_errors)) => (manifests.clone(), parse_errors),
        Err(err) => {
            if is_schedule_leased_dag_error(err) {
                if let Some(p) = err.path() {
                    retry_changes.push(p.to_path_buf());
                }
            } else {
                outcomes.push(ModHotReloadOutcome {
                    path: watch_dir.to_path_buf(),
                    mod_id: None,
                    action: ModHotReloadAction::Failed(err.to_string()),
                });
            }
            return SyncDirResult {
                outcomes,
                retry_changes,
                loaded_paths,
            };
        }
    };

    for err in parse_errors {
        if is_schedule_leased_dag_error(err) {
            if let Some(p) = err.path() {
                retry_changes.push(p.to_path_buf());
            }
            continue;
        }
        outcomes.push(ModHotReloadOutcome {
            path: watch_dir.to_path_buf(),
            mod_id: None,
            action: ModHotReloadAction::Failed(err.to_string()),
        });
    }

    let mut loaded_names: BTreeSet<String> = BTreeSet::new();
    let mut already_loaded: BTreeMap<String, semver::Version> = BTreeMap::new();
    if let Some(host) = world.get_resource::<ModHost>() {
        for loaded in host.loaded_mods() {
            if !loaded.is_active() {
                continue;
            }
            loaded_names.insert(loaded.name().to_string());
            if let Ok(ver) = semver::Version::parse(loaded.version()) {
                already_loaded.insert(loaded.name().to_string(), ver);
            }
        }
    }

    let unloaded_manifests: Vec<ModTomlManifest> = manifests
        .into_iter()
        .filter(|m| !loaded_names.contains(&m.name))
        .collect();

    if unloaded_manifests.is_empty() {
        return SyncDirResult {
            outcomes,
            retry_changes,
            loaded_paths,
        };
    }

    let resolution = resolve_dependency_dag(unloaded_manifests, &already_loaded);
    for diag in resolution.diagnostics {
        if is_schedule_leased_dag_error(&diag) {
            if let Some(p) = diag.path() {
                retry_changes.push(p.to_path_buf());
            }
            continue;
        }
        outcomes.push(ModHotReloadOutcome {
            path: watch_dir.to_path_buf(),
            mod_id: None,
            action: ModHotReloadAction::Failed(diag.to_string()),
        });
    }

    let opts = load_options.clone().with_shadow_copy(true);
    let mut failed_mods: BTreeSet<String> = BTreeSet::new();
    let mut deferred_mods: BTreeSet<String> = BTreeSet::new();
    for manifest in resolution.load_order {
        if let Some(dep) = manifest
            .dependencies
            .keys()
            .find(|d| failed_mods.contains(*d))
        {
            outcomes.push(ModHotReloadOutcome {
                path: manifest.manifest_path.clone(),
                mod_id: None,
                action: ModHotReloadAction::Failed(format!(
                    "mod '{}' skipped because dependency '{}' failed or is missing",
                    manifest.name, dep
                )),
            });
            failed_mods.insert(manifest.name);
            continue;
        }
        if let Some(dep) = manifest
            .dependencies
            .keys()
            .find(|d| deferred_mods.contains(*d))
        {
            tracing::debug!(
                mod_name = %manifest.name,
                dependency = %dep,
                "deferring dependent mod load because dependency awaits leased schedule"
            );
            deferred_mods.insert(manifest.name);
            retry_changes.push(manifest.manifest_path);
            continue;
        }
        if let Some(dep) = manifest
            .dependencies
            .keys()
            .find(|d| !loaded_names.contains(*d))
        {
            outcomes.push(ModHotReloadOutcome {
                path: manifest.manifest_path.clone(),
                mod_id: None,
                action: ModHotReloadAction::Failed(format!(
                    "mod '{}' skipped because dependency '{}' failed or is missing",
                    manifest.name, dep
                )),
            });
            failed_mods.insert(manifest.name);
            continue;
        }

        let lib_path = match manifest.resolve_library_path() {
            Ok(p) => p,
            Err(err) => {
                outcomes.push(ModHotReloadOutcome {
                    path: manifest.manifest_path.clone(),
                    mod_id: None,
                    action: ModHotReloadAction::Failed(err.to_string()),
                });
                failed_mods.insert(manifest.name);
                continue;
            }
        };

        if let Err(reason) = has_valid_native_binary_magic(&lib_path) {
            outcomes.push(ModHotReloadOutcome {
                path: lib_path,
                mod_id: None,
                action: ModHotReloadAction::Failed(reason),
            });
            failed_mods.insert(manifest.name);
            continue;
        }

        match world.load_mod_with(&lib_path, opts.clone()) {
            Ok(id) => {
                loaded_names.insert(manifest.name);
                loaded_paths.push(manifest.manifest_path);
                loaded_paths.push(lib_path.clone());
                outcomes.push(ModHotReloadOutcome {
                    path: lib_path,
                    mod_id: Some(id),
                    action: ModHotReloadAction::Loaded,
                });
            }
            Err(source) => {
                let dag_err = ModDagError::LoadFailed {
                    mod_name: manifest.name.clone(),
                    path: lib_path.clone(),
                    source: Box::new(source),
                };
                if is_schedule_leased_dag_error(&dag_err) {
                    tracing::debug!(
                        mod_name = %manifest.name,
                        path = %lib_path.display(),
                        error = %dag_err,
                        "deferring mod load until leased schedule returns"
                    );
                    deferred_mods.insert(manifest.name);
                    retry_changes.push(manifest.manifest_path);
                } else {
                    let msg = dag_err
                        .load_error()
                        .map_or_else(|| dag_err.to_string(), ToString::to_string);
                    outcomes.push(ModHotReloadOutcome {
                        path: lib_path,
                        mod_id: None,
                        action: ModHotReloadAction::Failed(msg),
                    });
                    failed_mods.insert(manifest.name);
                }
            }
        }
    }

    SyncDirResult {
        outcomes,
        retry_changes,
        loaded_paths,
    }
}

fn collect_cascade_names(world: &World, root_id: ModId) -> Vec<String> {
    let active_ids: BTreeSet<ModId> = world
        .get_resource::<ModHost>()
        .map(|h| h.loaded_mods().iter().map(|m| m.id()).collect())
        .unwrap_or_default();
    let forward_ids = world
        .get_resource::<ModDependencyGraph>()
        .map(|dag| dag.cascading_order(root_id, &active_ids))
        .unwrap_or_else(|| vec![root_id]);
    let Some(host) = world.get_resource::<ModHost>() else {
        return Vec::new();
    };
    forward_ids
        .into_iter()
        .filter_map(|id| host.get_mod(id).map(|m| m.name().to_string()))
        .collect()
}

fn apply_hot_reload_events(
    world: &mut World,
    watch_dir: &Path,
    changed_paths: &[PathBuf],
    deleted_paths: &[PathBuf],
    load_options: &LoadOptions,
) -> ApplyHotReloadResult {
    let mut outcomes = Vec::new();
    let mut retry_changes = Vec::new();
    let mut retry_deletes = Vec::new();
    let opts = load_options.clone().with_shadow_copy(true);

    // 1. Handle confirmed deletions via cascading `unload_mod`.
    for del_path in deleted_paths {
        let target = if is_manifest_path(del_path) {
            find_loaded_mod_for_manifest_dir(world, del_path, None).map(|(id, _name, _lib)| id)
        } else if is_dylib_path(del_path) || is_bampack_path(del_path) {
            find_loaded_mod_for_dylib(world, del_path).map(|(id, _)| id)
        } else {
            None
        };
        if let Some(mod_id) = target {
            match world.unload_mod(mod_id) {
                Ok(()) => outcomes.push(ModHotReloadOutcome {
                    path: del_path.clone(),
                    mod_id: Some(mod_id),
                    action: ModHotReloadAction::Unloaded,
                }),
                Err(err) if is_schedule_leased_load_error(&err) => {
                    tracing::debug!(
                        path = %del_path.display(),
                        mod_id = ?mod_id,
                        error = %err,
                        "deferring mod unload until leased schedule returns"
                    );
                    retry_deletes.push(del_path.clone());
                }
                Err(err) => outcomes.push(ModHotReloadOutcome {
                    path: del_path.clone(),
                    mod_id: None,
                    action: ModHotReloadAction::Failed(err.to_string()),
                }),
            }
        }
    }

    // Expand `.sig` sidecar updates into their sibling mod binary/bampack paths (deduplicated).
    let mut expanded_changes: Vec<PathBuf> = Vec::with_capacity(changed_paths.len());
    let mut seen_expanded: HashSet<PathBuf> = HashSet::new();
    for path in changed_paths {
        if is_sig_path(path) {
            if let Some(target) = resolve_sig_target_path(world, path)
                && seen_expanded.insert(canonical_key(&target))
            {
                expanded_changes.push(target);
            }
            continue;
        }
        if seen_expanded.insert(canonical_key(path)) {
            expanded_changes.push(path.clone());
        }
    }

    // 2. Handle changes to already-loaded mods, `.schedule_delta.json`, and `.bampack` archives
    //    using at most ONE recursive `discover_mod_manifests` directory walk per batch.
    let mut reloaded_in_batch: HashSet<String> = HashSet::new();
    let mut deferred_in_batch: HashSet<String> = HashSet::new();
    let mut needs_dag_sync = false;
    let mut standalone_dylibs: Vec<PathBuf> = Vec::new();
    let mut cached_discovery: Option<DiscoveredDirResult> = None;

    for path in &expanded_changes {
        if is_schedule_delta_path(path) && path.is_file() {
            match apply_schedule_delta_file(world, path) {
                Ok(()) => outcomes.push(ModHotReloadOutcome {
                    path: path.clone(),
                    mod_id: None,
                    action: ModHotReloadAction::Reloaded,
                }),
                Err(DeltaApplyError::Leased) => retry_changes.push(path.clone()),
                Err(DeltaApplyError::Failed(msg)) => outcomes.push(ModHotReloadOutcome {
                    path: path.clone(),
                    mod_id: None,
                    action: ModHotReloadAction::Failed(msg),
                }),
            }
        } else if is_bampack_path(path) && path.is_file() {
            if let Err(reason) = has_valid_bampack_magic(path) {
                outcomes.push(ModHotReloadOutcome {
                    path: path.clone(),
                    mod_id: None,
                    action: ModHotReloadAction::Failed(reason),
                });
                continue;
            }
            #[cfg(feature = "signature-verification")]
            {
                if let Some((mod_id, mod_name)) = find_loaded_mod_for_dylib(world, path) {
                    if reloaded_in_batch.contains(&mod_name)
                        || deferred_in_batch.contains(&mod_name)
                    {
                        continue;
                    }
                    let cascade_names = collect_cascade_names(world, mod_id);
                    match world.reload_bampack(mod_id, path, opts.clone()) {
                        Ok(new_id) => {
                            reloaded_in_batch.extend(cascade_names);
                            outcomes.push(ModHotReloadOutcome {
                                path: path.clone(),
                                mod_id: Some(new_id),
                                action: ModHotReloadAction::Reloaded,
                            });
                        }
                        Err(err) if is_schedule_leased_load_error(&err) => {
                            deferred_in_batch.insert(mod_name);
                            retry_changes.push(path.clone());
                        }
                        Err(err) => outcomes.push(ModHotReloadOutcome {
                            path: path.clone(),
                            mod_id: None,
                            action: ModHotReloadAction::Failed(err.to_string()),
                        }),
                    }
                } else {
                    match world.load_bampack_with(path, opts.clone()) {
                        Ok(id) => outcomes.push(ModHotReloadOutcome {
                            path: path.clone(),
                            mod_id: Some(id),
                            action: ModHotReloadAction::Loaded,
                        }),
                        Err(err) if is_schedule_leased_load_error(&err) => {
                            retry_changes.push(path.clone());
                        }
                        Err(err) => outcomes.push(ModHotReloadOutcome {
                            path: path.clone(),
                            mod_id: None,
                            action: ModHotReloadAction::Failed(err.to_string()),
                        }),
                    }
                }
            }
            #[cfg(not(feature = "signature-verification"))]
            {
                outcomes.push(ModHotReloadOutcome {
                    path: path.clone(),
                    mod_id: None,
                    action: ModHotReloadAction::Failed(
                        "loading .bampack bundles requires the `signature-verification` feature"
                            .to_string(),
                    ),
                });
            }
        } else if is_manifest_path(path) {
            let parsed = match ModTomlManifest::from_file(path) {
                Ok(m) => m,
                Err(err) => {
                    outcomes.push(ModHotReloadOutcome {
                        path: path.clone(),
                        mod_id: None,
                        action: ModHotReloadAction::Failed(err.to_string()),
                    });
                    continue;
                }
            };
            needs_dag_sync = true;
            if let Some((mod_id, mod_name, lib_path)) =
                find_loaded_mod_for_manifest_dir(world, path, Some(&parsed.name))
            {
                if reloaded_in_batch.contains(&mod_name) || deferred_in_batch.contains(&mod_name) {
                    continue;
                }
                let discovery =
                    cached_discovery.get_or_insert_with(|| discover_mod_manifests(watch_dir));
                match validate_dir_dag_for_reload_from_discovery(
                    world,
                    discovery,
                    &mod_name,
                    Some(path),
                ) {
                    Ok(validated_opt) => {
                        let resolved_lib = validated_opt
                            .as_ref()
                            .and_then(|m| m.resolve_library_path().ok())
                            .unwrap_or(lib_path);
                        if let Err(reason) = has_valid_native_binary_magic(&resolved_lib) {
                            outcomes.push(ModHotReloadOutcome {
                                path: resolved_lib,
                                mod_id: None,
                                action: ModHotReloadAction::Failed(reason),
                            });
                            continue;
                        }
                        let cascade_names = collect_cascade_names(world, mod_id);
                        match world.reload_mod_from(mod_id, &resolved_lib, opts.clone()) {
                            Ok(new_id) => {
                                reloaded_in_batch.extend(cascade_names);
                                if let Some(m) = validated_opt {
                                    let deps: Vec<String> = m
                                        .dependencies
                                        .keys()
                                        .chain(m.optional_dependencies.keys())
                                        .cloned()
                                        .collect();
                                    if let Some(mut dag) =
                                        world.get_resource_mut::<ModDependencyGraph>()
                                    {
                                        dag.register_loaded_mod(
                                            new_id,
                                            m.name,
                                            resolved_lib.clone(),
                                            opts.clone(),
                                            deps,
                                        );
                                    }
                                }
                                outcomes.push(ModHotReloadOutcome {
                                    path: resolved_lib,
                                    mod_id: Some(new_id),
                                    action: ModHotReloadAction::Reloaded,
                                });
                            }
                            Err(err) if is_schedule_leased_load_error(&err) => {
                                tracing::debug!(
                                    path = %path.display(),
                                    mod_id = ?mod_id,
                                    mod_name = %mod_name,
                                    error = %err,
                                    "deferring manifest-triggered mod reload until leased schedule returns"
                                );
                                deferred_in_batch.insert(mod_name);
                                retry_changes.push(path.clone());
                            }
                            Err(err) => outcomes.push(ModHotReloadOutcome {
                                path: resolved_lib,
                                mod_id: None,
                                action: ModHotReloadAction::Failed(err.to_string()),
                            }),
                        }
                    }
                    Err(err) => outcomes.push(ModHotReloadOutcome {
                        path: path.clone(),
                        mod_id: None,
                        action: ModHotReloadAction::Failed(err),
                    }),
                }
            }
        } else if is_dylib_path(path) && path.is_file() {
            if let Some((mod_id, mod_name)) = find_loaded_mod_for_dylib(world, path) {
                if reloaded_in_batch.contains(&mod_name) || deferred_in_batch.contains(&mod_name) {
                    continue;
                }
                let sidecar_manifest = path.parent().map(|d| d.join("mod.toml"));
                let sidecar_opt = sidecar_manifest.as_deref().filter(|p| p.is_file());
                let discovery =
                    cached_discovery.get_or_insert_with(|| discover_mod_manifests(watch_dir));
                if let Err(err) = validate_dir_dag_for_reload_from_discovery(
                    world,
                    discovery,
                    &mod_name,
                    sidecar_opt,
                ) {
                    outcomes.push(ModHotReloadOutcome {
                        path: path.clone(),
                        mod_id: None,
                        action: ModHotReloadAction::Failed(err),
                    });
                    continue;
                }
                if let Err(reason) = has_valid_native_binary_magic(path) {
                    outcomes.push(ModHotReloadOutcome {
                        path: path.clone(),
                        mod_id: None,
                        action: ModHotReloadAction::Failed(reason),
                    });
                    continue;
                }
                let cascade_names = collect_cascade_names(world, mod_id);
                match world.reload_mod_from(mod_id, path, opts.clone()) {
                    Ok(new_id) => {
                        reloaded_in_batch.extend(cascade_names);
                        outcomes.push(ModHotReloadOutcome {
                            path: path.clone(),
                            mod_id: Some(new_id),
                            action: ModHotReloadAction::Reloaded,
                        });
                    }
                    Err(err) if is_schedule_leased_load_error(&err) => {
                        tracing::debug!(
                            path = %path.display(),
                            mod_id = ?mod_id,
                            mod_name = %mod_name,
                            error = %err,
                            "deferring dylib mod reload until leased schedule returns"
                        );
                        deferred_in_batch.insert(mod_name);
                        retry_changes.push(path.clone());
                    }
                    Err(err) => outcomes.push(ModHotReloadOutcome {
                        path: path.clone(),
                        mod_id: None,
                        action: ModHotReloadAction::Failed(err.to_string()),
                    }),
                }
            } else {
                let has_manifest = path
                    .parent()
                    .is_some_and(|dir| dir.join("mod.toml").is_file());
                if has_manifest {
                    needs_dag_sync = true;
                } else {
                    standalone_dylibs.push(path.clone());
                }
            }
        }
    }

    // 3. Incrementally load any newly added `mod.toml` mods (or newly satisfied dependents) in
    //    Kahn topological order reusing the cached manifest walk when already performed.
    if needs_dag_sync {
        let discovery = cached_discovery.get_or_insert_with(|| discover_mod_manifests(watch_dir));
        let sync = load_unloaded_dag_mods_from_discovery(world, watch_dir, &opts, discovery);
        outcomes.extend(sync.outcomes);
        retry_changes.extend(sync.retry_changes);
    }

    // 4. Load any newly added standalone shared libraries without a `mod.toml`.
    let mut seen_standalone = HashSet::new();
    for lib_path in standalone_dylibs {
        if find_loaded_mod_for_dylib(world, &lib_path).is_some() {
            continue;
        }
        let key = canonical_key(&lib_path);
        if !seen_standalone.insert(key) {
            continue;
        }
        if let Err(reason) = has_valid_native_binary_magic(&lib_path) {
            outcomes.push(ModHotReloadOutcome {
                path: lib_path,
                mod_id: None,
                action: ModHotReloadAction::Failed(reason),
            });
            continue;
        }

        match world.load_mod_with(&lib_path, opts.clone()) {
            Ok(id) => outcomes.push(ModHotReloadOutcome {
                path: lib_path,
                mod_id: Some(id),
                action: ModHotReloadAction::Loaded,
            }),
            Err(err) if is_schedule_leased_load_error(&err) => {
                tracing::debug!(
                    path = %lib_path.display(),
                    error = %err,
                    "deferring standalone mod load until leased schedule returns"
                );
                retry_changes.push(lib_path);
            }
            Err(err) => outcomes.push(ModHotReloadOutcome {
                path: lib_path,
                mod_id: None,
                action: ModHotReloadAction::Failed(err.to_string()),
            }),
        }
    }

    ApplyHotReloadResult {
        outcomes,
        retry_changes,
        retry_deletes,
    }
}

#[cfg(test)]
#[path = "../tests/unit/watcher_tests.rs"]
mod tests;
