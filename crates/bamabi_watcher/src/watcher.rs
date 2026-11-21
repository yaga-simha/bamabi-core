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
use bamabi_host::world_ext::ModWorldExt;
use bamabi_host::{ModLifecycleFlush, ensure_mod_lifecycle_flush_schedule};
use bamabi_host_sys::{LoadOptions, ModHost, ModId, ModLoadError, canonical_mod_name_from_path};
use bevy_app::{App, Plugin};
use bevy_ecs::prelude::Resource;
use bevy_ecs::world::World;
use notify_debouncer_full::notify::event::{CreateKind, ModifyKind, RemoveKind};
use notify_debouncer_full::notify::{EventKind, RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{DebounceEventResult, Debouncer, RecommendedCache, new_debouncer};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

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

/// World resource recording outcomes emitted by [`ModHotReloadPlugin`]'s polling system.
#[derive(Resource, Default, Debug, Clone)]
pub struct ModHotReloadHistory {
    /// Chronological list of hot-reload outcomes observed in this `World`.
    pub outcomes: Vec<ModHotReloadOutcome>,
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
    rx: Mutex<mpsc::Receiver<DebounceEventResult>>,
    pending_deletes: Mutex<HashMap<PathBuf, Instant>>,
    pending_retries: Mutex<PendingRetries>,
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
            rx: Mutex::new(rx),
            pending_deletes: Mutex::new(HashMap::new()),
            pending_retries: Mutex::new(PendingRetries::default()),
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
            rx: Mutex::new(rx),
            pending_deletes: Mutex::new(HashMap::new()),
            pending_retries: Mutex::new(PendingRetries::default()),
            sync_fingerprints: Mutex::new(HashMap::new()),
            disconnected_logged: AtomicBool::new(false),
            disconnected_outcome_recorded: AtomicBool::new(false),
            _debouncer: Mutex::new(None),
        }
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

        for path in retries.changes {
            if path.is_file() && !pending.contains_key(&path) && seen_changed.insert(path.clone()) {
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
            app.init_resource::<ModHotReloadHistory>();
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
                        hist.outcomes.extend(initial);
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
            history.outcomes.extend(outcomes);
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

fn is_watchable_mod_path(path: &Path) -> bool {
    is_dylib_path(path) || is_manifest_path(path)
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

fn validate_dir_dag_for_reload(
    world: &World,
    watch_dir: &Path,
    target_mod_name: &str,
    target_manifest_path: Option<&Path>,
) -> Result<Option<ModTomlManifest>, String> {
    let (manifests, parse_errors) = match discover_mod_manifests(watch_dir) {
        Ok(res) => res,
        Err(err) => return Err(err.to_string()),
    };

    if let Some(t_path) = target_manifest_path {
        for err in &parse_errors {
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
    let mut outcomes = Vec::new();
    let mut retry_changes = Vec::new();
    let mut loaded_paths = Vec::new();
    let (manifests, parse_errors) = match discover_mod_manifests(watch_dir) {
        Ok(res) => res,
        Err(err) => {
            if is_schedule_leased_dag_error(&err) {
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
        if is_schedule_leased_dag_error(&err) {
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
        } else if is_dylib_path(del_path) {
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

    // 2. Handle changes to already-loaded mods (re-validating `mod.toml` SemVer + DAG and
    //    cascading reload across dependents in reverse/forward topological order).
    let mut reloaded_in_batch: HashSet<String> = HashSet::new();
    let mut deferred_in_batch: HashSet<String> = HashSet::new();
    let mut needs_dag_sync = false;
    let mut standalone_dylibs: Vec<PathBuf> = Vec::new();

    for path in changed_paths {
        if is_manifest_path(path) {
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
                match validate_dir_dag_for_reload(world, watch_dir, &mod_name, Some(path)) {
                    Ok(validated_opt) => {
                        let resolved_lib = validated_opt
                            .as_ref()
                            .and_then(|m| m.resolve_library_path().ok())
                            .unwrap_or(lib_path);
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
                if let Err(err) =
                    validate_dir_dag_for_reload(world, watch_dir, &mod_name, sidecar_opt)
                {
                    outcomes.push(ModHotReloadOutcome {
                        path: path.clone(),
                        mod_id: None,
                        action: ModHotReloadAction::Failed(err),
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
    //    Kahn topological order.
    if needs_dag_sync {
        let sync = load_unloaded_dag_mods(world, watch_dir, &opts);
        outcomes.extend(sync.outcomes);
        retry_changes.extend(sync.retry_changes);
    }

    // 4. Load any newly added standalone shared libraries without a `mod.toml`.
    let mut seen_standalone = HashSet::new();
    for lib_path in standalone_dylibs {
        if find_loaded_mod_for_dylib(world, &lib_path).is_some() {
            continue;
        }
        let key = std::fs::canonicalize(&lib_path).unwrap_or_else(|_| lib_path.clone());
        if !seen_standalone.insert(key) {
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
