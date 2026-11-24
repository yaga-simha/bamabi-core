//! Schedule mapping (`ModScheduleMap`), system sets, decoupled ABI `0.3` schedule set registry
//! (`ModScheduleSetRegistry`), fast schedule-only hot-rewiring, and startup tracking.

use crate::handle_header::HostContext;
use crate::loader::{ModHost, ModId};
use crate::registry::{ModRegistry, RegistrySnapshot};
use crate::sync_ptr::{GuestFnPtr, GuestUserData};
use crate::system::access_builder::initialize_query_groups_in_world_with_base_idx;
use crate::system::guest_system::{
    HostSystemCtx, RandomQueryView, build_query_slot_iters_bucketed,
};
use crate::system::query_group::CachedQueryGroup;
use crate::system::{BamQuerySpecOwned, GuestSystem};
use bamabi_sys::{
    BAM_SCHEDULE_SET_FLAG_PARALLEL, BAM_SCHEDULE_SET_FLAG_SEQUENCE,
    BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK, BamIter, BamSystemFn, BamSystemSet, FfiResult, ScheduleId,
};
use bevy_app::{FixedUpdate, Last, PostUpdate, PreUpdate, Startup, Update};
use bevy_ecs::change_detection::{CheckChangeTicks, Tick};
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::graph::{
    Dag, DagCrossDependencyError, DagOverlappingGroupError, DagRedundancyError,
    DiGraphToposortError, Direction, GraphNodeId,
};
use bevy_ecs::schedule::passes::AutoInsertApplyDeferredPass;
use bevy_ecs::schedule::{
    FlattenedDependencies, InternedScheduleLabel, InternedSystemSet, IntoScheduleConfigs, LogLevel,
    NodeId, Schedule, ScheduleBuildError, ScheduleBuildMetadata, ScheduleBuildPass,
    ScheduleBuildWarning, ScheduleConfigs, ScheduleGraph, ScheduleLabel, Schedules, SystemKey,
    SystemSet, SystemSetKey,
};
use bevy_ecs::system::{
    ReadOnlySystem, RunSystemError, ScheduleSystem, System, SystemAccess, SystemIn,
    SystemStateFlags,
};
use bevy_ecs::utils::DebugName;
use bevy_ecs::world::{DeferredWorld, World, unsafe_world_cell::UnsafeWorldCell};
use bevy_platform::collections::{HashMap, HashSet};
use core::sync::atomic::{AtomicBool, Ordering};
use std::collections::VecDeque;
use std::sync::Arc;

/// Bevy [`SystemSet`] labels automatically attached to dynamic guest systems.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModSystemSet {
    /// Contains all dynamic guest systems across all loaded mods.
    All,
    /// Contains all dynamic guest systems belonging to a specific [`ModId`].
    Mod(ModId),
}

/// Module-scoped Bevy [`SystemSet`] attached to every guest system for intra-mod and cross-mod
/// ordering (`"system_name"` within the same mod, or `"other_mod::system_name"` across mods).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModNamedSystemSet {
    /// Logical module name owning the system.
    pub mod_name: Arc<str>,
    /// Guest system name registered by the mod.
    pub system_name: Arc<str>,
    /// Monotonic reload generation for `mod_name` so hot-reloads allocate a fresh
    /// `InternedSystemSet` rather than reusing a removed `SlotMap` key in Bevy `0.20`'s `SystemSets`.
    pub generation: u64,
}

impl ModNamedSystemSet {
    /// Creates a module-scoped named system set at generation `0`.
    #[inline]
    pub fn new(mod_name: impl Into<Arc<str>>, system_name: impl Into<Arc<str>>) -> Self {
        Self::with_generation(mod_name, system_name, 0)
    }

    /// Creates a module-scoped named system set with an explicit reload `generation`.
    #[inline]
    pub fn with_generation(
        mod_name: impl Into<Arc<str>>,
        system_name: impl Into<Arc<str>>,
        generation: u64,
    ) -> Self {
        Self {
            mod_name: mod_name.into(),
            system_name: system_name.into(),
            generation,
        }
    }

    /// Returns a copy of this named set bound to `generation`.
    #[inline]
    pub fn at_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// Returns `true` if this named system set is currently active in `schedule`'s `ScheduleGraph`.
    #[inline]
    pub fn is_present_in(&self, schedule: &Schedule) -> bool {
        let sets = &schedule.graph().system_sets;
        sets.get_key(self.clone().intern())
            .and_then(|key| sets.get(key))
            .is_some()
    }

    /// Parses an ordering target string (`"sys"` -> `(default_mod_name, "sys")`,
    /// `"other_mod::sys"` -> `("other_mod", "sys")`, or multi-segment
    /// `"game::physics::step"` -> `("game", "physics::step")` or `("game::physics", "step")`
    /// when matching `default_mod_name`) at generation `0`.
    ///
    /// Returns `None` if `raw` is empty or contains empty `"::"` segments.
    pub fn parse_target(raw: &str, default_mod_name: &str) -> Option<Self> {
        if raw.is_empty() || raw.split("::").any(str::is_empty) {
            return None;
        }
        if !default_mod_name.is_empty()
            && let Some(rest) = raw
                .strip_prefix(default_mod_name)
                .and_then(|s| s.strip_prefix("::"))
        {
            return Some(Self::new(default_mod_name, rest));
        }
        if let Some((mod_part, sys_part)) = raw.split_once("::") {
            Some(Self::new(mod_part, sys_part))
        } else {
            Some(Self::new(default_mod_name, raw))
        }
    }
}

/// Resource mapping ABI [`ScheduleId`] discriminants (`0..=5` built-in and `>= 1024` custom game
/// schedules) onto Bevy [`InternedScheduleLabel`]s.
#[derive(Resource, Default, Clone, Debug)]
pub struct ModScheduleMap {
    custom: HashMap<ScheduleId, InternedScheduleLabel>,
}

impl ModScheduleMap {
    /// Creates an empty custom schedule map (built-in IDs `0..=5` are always available).
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a game-defined custom [`ScheduleId`] (`id.0 >= ScheduleId::CUSTOM_BASE` = `1024`)
    /// mapped to `label`.
    ///
    /// Returns `false` if `id` is not in the custom range (`< 1024`).
    #[inline]
    pub fn register<S: ScheduleLabel>(&mut self, id: ScheduleId, label: S) -> bool {
        if !id.is_custom() {
            return false;
        }
        self.custom.insert(id, label.intern());
        true
    }

    /// Resolves `id` to its [`InternedScheduleLabel`], handling built-in IDs (`0..=5`) directly
    /// and consulting registered custom entries for `id.0 >= 1024`.
    #[inline]
    pub fn resolve(&self, id: ScheduleId) -> Option<InternedScheduleLabel> {
        if let Some(builtin) = resolve_builtin_schedule_id(id) {
            return Some(builtin);
        }
        self.custom.get(&id).copied()
    }

    /// Returns `true` if `id` is a built-in schedule (`0..=5`) or a registered custom schedule.
    #[inline]
    pub fn contains(&self, id: ScheduleId) -> bool {
        id.is_builtin() || self.custom.contains_key(&id)
    }
}

/// Resource mapping logical host set names (referenced by guests as `"host::<name>"`) onto Bevy
/// [`InternedSystemSet`]s so guest systems can order themselves `.after(c"host::physics")` or
/// `.before(c"host::render")` without risking host-set excision on mod unload.
#[derive(Resource, Default, Clone, Debug)]
pub struct ModVisibleSets {
    sets: HashMap<Arc<str>, InternedSystemSet>,
}

impl ModVisibleSets {
    /// Creates an empty [`ModVisibleSets`] map.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Exposes `set` to guest ordering targets under `"host::<name>"` (accepts either bare `"physics"`
    /// or `"host::physics"`).
    ///
    /// Returns `false` if `name` is empty or contains `"::"` other than a single leading `"host::"`.
    pub fn register<S: SystemSet>(&mut self, name: &str, set: S) -> bool {
        let stripped = name.strip_prefix("host::").unwrap_or(name);
        if stripped.is_empty() || stripped.contains("::") {
            return false;
        }
        self.sets.insert(Arc::from(stripped), set.intern());
        true
    }

    /// Resolves a bare host set name (`"physics"`) or prefixed `"host::physics"` to its [`InternedSystemSet`].
    #[inline]
    pub fn resolve(&self, name: &str) -> Option<InternedSystemSet> {
        let stripped = name.strip_prefix("host::").unwrap_or(name);
        self.sets.get(stripped).copied()
    }

    /// Returns `true` if `name` (bare or `"host::"`-prefixed) is registered.
    #[inline]
    pub fn contains(&self, name: &str) -> bool {
        self.resolve(name).is_some()
    }
}

/// Resolves one of the six built-in ABI [`ScheduleId`]s (`0..=5`) into its Bevy [`InternedScheduleLabel`].
#[inline]
pub fn resolve_builtin_schedule_id(id: ScheduleId) -> Option<InternedScheduleLabel> {
    match id {
        ScheduleId::UPDATE => Some(Update.intern()),
        ScheduleId::STARTUP => Some(Startup.intern()),
        ScheduleId::PRE_UPDATE => Some(PreUpdate.intern()),
        ScheduleId::POST_UPDATE => Some(PostUpdate.intern()),
        ScheduleId::FIXED_UPDATE => Some(FixedUpdate.intern()),
        ScheduleId::LAST => Some(Last.intern()),
        _ => None,
    }
}

/// Resource tracking whether Bevy's one-time `Startup` schedule has already completed.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StartupCompleted(pub bool);

impl StartupCompleted {
    /// Returns `true` once `PostStartup` has executed.
    #[inline]
    pub const fn has_run(self) -> bool {
        self.0
    }
}

/// Exclusive `PostStartup` system that marks [`StartupCompleted`] as having executed.
pub fn mark_startup_completed(world: &mut World) {
    if let Some(mut state) = world.get_resource_mut::<StartupCompleted>() {
        state.0 = true;
    } else {
        world.insert_resource(StartupCompleted(true));
    }
}

// ============================================================================
// ABI 0.3 Decoupled Schedule Sets & Fast Schedule-Only Hot-Rewiring
// ============================================================================

/// Execution ordering mode for a dynamically configured ABI `0.3` [`BamSystemSet`] container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ScheduleSetMode {
    /// Unordered parallel container (`BAM_SCHEDULE_SET_FLAG_PARALLEL` or default `0`).
    #[default]
    Parallel,
    /// Strict sequential ordering across mounted member systems (Bevy `.chain()`).
    Sequence,
    /// Weak sequential ordering across mounted member systems (Bevy `0.20` `.chain_weak()`).
    SequenceWeak,
}

impl ScheduleSetMode {
    /// Extracts the container execution mode from validated `BAM_SCHEDULE_SET_FLAG_*` bitflags.
    #[inline]
    pub const fn from_abi_flags(flags: u32) -> Self {
        if (flags & BAM_SCHEDULE_SET_FLAG_SEQUENCE) != 0 {
            Self::Sequence
        } else if (flags & BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK) != 0 {
            Self::SequenceWeak
        } else {
            Self::Parallel
        }
    }

    /// Returns the canonical `BAM_SCHEDULE_SET_FLAG_*` bit for this execution mode.
    #[inline]
    pub const fn to_abi_flags(self) -> u32 {
        match self {
            Self::Parallel => BAM_SCHEDULE_SET_FLAG_PARALLEL,
            Self::Sequence => BAM_SCHEDULE_SET_FLAG_SEQUENCE,
            Self::SequenceWeak => BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK,
        }
    }
}

/// Metadata for a decoupled [`BamSystemSet`] container configured via
/// `host_configure_schedule_set`.
#[derive(Debug, Clone)]
pub struct ConfiguredScheduleSet {
    /// Monotonic handle allocated by [`ModScheduleSetRegistry`] (`0, 1, ...`).
    pub handle: BamSystemSet,
    /// Owning [`ModId`], if configured during a tracked mod load session.
    pub mod_id: Option<ModId>,
    /// Logical module namespace scoping [`Self::named_set`].
    pub mod_name: Arc<str>,
    /// Logical set name within `mod_name`.
    pub set_name: Arc<str>,
    /// Reload generation of `mod_name` at configuration time.
    pub generation: u64,
    /// Scoped Bevy [`ModNamedSystemSet`] container bound to `(mod_name, set_name, generation)`.
    pub named_set: ModNamedSystemSet,
    /// Pre-interned handle for [`Self::named_set`] (avoids global interner lookups during hot-rewire).
    interned_set: InternedSystemSet,
    /// Target ABI [`ScheduleId`] where this container lives.
    pub schedule: ScheduleId,
    /// Intra-set execution ordering mode (`Parallel`, `Sequence`, or `SequenceWeak`).
    pub mode: ScheduleSetMode,
    /// Sets ordered after this set (`self.before(target)` / `self.before_weak(target)`).
    pub before_sets: Vec<BamSystemSet>,
    /// Sets ordered before this set (`self.after(target)` / `self.after_weak(target)`).
    pub after_sets: Vec<BamSystemSet>,
    /// When `true`, `before_sets` edges apply Bevy `.before_weak(...)` instead of `.before(...)`.
    pub before_weak: bool,
    /// When `true`, `after_sets` edges apply Bevy `.after_weak(...)` instead of `.after(...)`.
    pub after_weak: bool,
    /// Optional context-aware C-ABI run condition (`BamScheduleSetDesc::run_if_ctx`).
    pub run_if_ctx: Option<BamSystemFn>,
    /// Optional read-only query clause passed to `run_if_ctx` (`BamScheduleSetDesc::run_if_query`).
    pub run_if_query: Option<BamQuerySpecOwned>,
    /// Opaque user context pointer forwarded to `run_if_ctx`.
    pub run_if_ctx_user_data: GuestUserData,
    /// Canonical parent set names (`in_set`) from `BamScheduleSetDesc::parent_sets`.
    pub parent_sets: Vec<String>,
    /// Ordered list of mounted member systems (their scoped [`ModNamedSystemSet`]s in mount order).
    pub mounted_systems: Vec<ModNamedSystemSet>,
    /// Pre-interned handles mirroring [`Self::mounted_systems`] for lock-free hot-rewiring.
    mounted_interned: Vec<InternedSystemSet>,
}

impl PartialEq for ConfiguredScheduleSet {
    fn eq(&self, other: &Self) -> bool {
        self.handle == other.handle
            && self.mod_id == other.mod_id
            && self.mod_name == other.mod_name
            && self.set_name == other.set_name
            && self.generation == other.generation
            && self.named_set == other.named_set
            && self.schedule == other.schedule
            && self.mode == other.mode
            && self.before_sets == other.before_sets
            && self.after_sets == other.after_sets
            && self.before_weak == other.before_weak
            && self.after_weak == other.after_weak
            && self.run_if_ctx.map(|f| f as usize) == other.run_if_ctx.map(|f| f as usize)
            && self.run_if_query == other.run_if_query
            && self.run_if_ctx_user_data == other.run_if_ctx_user_data
            && self.parent_sets == other.parent_sets
            && self.mounted_systems == other.mounted_systems
    }
}

impl Eq for ConfiguredScheduleSet {}

impl ConfiguredScheduleSet {
    /// Returns the canonical `"<mod_name>::<set_name>"` target string for `GuestSystem::in_sets`.
    #[inline]
    pub fn canonical_name(&self) -> String {
        format!("{}::{}", self.mod_name, self.set_name)
    }
}

/// Parameters for configuring a new decoupled [`BamSystemSet`] container in
/// [`ModScheduleSetRegistry::configure_set`].
#[derive(Debug, Clone)]
pub struct ScheduleSetConfig {
    /// Owning [`ModId`], if configured during a tracked mod load session.
    pub mod_id: Option<ModId>,
    /// Logical module namespace scoping the set.
    pub mod_name: Arc<str>,
    /// Logical set name within `mod_name`.
    pub set_name: Arc<str>,
    /// Reload generation of `mod_name` at configuration time.
    pub generation: u64,
    /// Target ABI [`ScheduleId`] where this container lives.
    pub schedule: ScheduleId,
    /// Intra-set execution ordering mode (`Parallel`, `Sequence`, or `SequenceWeak`).
    pub mode: ScheduleSetMode,
    /// Sets ordered after this set (`before` / `before_weak`).
    pub before_sets: Vec<BamSystemSet>,
    /// Sets ordered before this set (`after` / `after_weak`).
    pub after_sets: Vec<BamSystemSet>,
    /// When `true`, `before_sets` edges use weak ordering.
    pub before_weak: bool,
    /// When `true`, `after_sets` edges use weak ordering.
    pub after_weak: bool,
    /// Optional context-aware run condition callback (`BamScheduleSetDesc::run_if_ctx`).
    pub run_if_ctx: Option<BamSystemFn>,
    /// Optional read-only query clause passed to `run_if_ctx`.
    pub run_if_query: Option<BamQuerySpecOwned>,
    /// Opaque user context pointer forwarded to `run_if_ctx`.
    pub run_if_ctx_user_data: GuestUserData,
    /// Optional parent system sets (`in_set`) from `BamScheduleSetDesc::parent_sets`.
    pub parent_sets: Vec<String>,
}

impl Default for ScheduleSetConfig {
    fn default() -> Self {
        Self {
            mod_id: None,
            mod_name: Arc::from("unscoped"),
            set_name: Arc::from("default"),
            generation: 0,
            schedule: ScheduleId::UPDATE,
            mode: ScheduleSetMode::Parallel,
            before_sets: Vec::new(),
            after_sets: Vec::new(),
            before_weak: false,
            after_weak: false,
            run_if_ctx: None,
            run_if_query: None,
            run_if_ctx_user_data: GuestUserData::NULL,
            parent_sets: Vec::new(),
        }
    }
}

/// World resource tracking all dynamically configured ABI `0.3` [`BamSystemSet`] containers,
/// their scoped [`ModNamedSystemSet`] mappings, inter-set ordering edges, and mounted member
/// system sequences.
#[derive(Resource, Default, Clone, Debug)]
pub struct ModScheduleSetRegistry {
    next_handle: u32,
    sets: Vec<Option<ConfiguredScheduleSet>>,
    by_named_set: HashMap<(ScheduleId, ModNamedSystemSet), BamSystemSet>,
}

impl ModScheduleSetRegistry {
    /// Creates an empty [`ModScheduleSetRegistry`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of active configured sets in this registry.
    #[inline]
    pub fn len(&self) -> usize {
        self.by_named_set.len()
    }

    /// Returns `true` if no schedule sets are currently registered.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.by_named_set.is_empty()
    }

    /// Looks up a configured schedule set by its [`BamSystemSet`] handle.
    #[inline]
    pub fn get(&self, handle: BamSystemSet) -> Option<&ConfiguredScheduleSet> {
        if !handle.is_valid() {
            return None;
        }
        self.sets.get(handle.0 as usize).and_then(Option::as_ref)
    }

    /// Mutably looks up a configured schedule set by its [`BamSystemSet`] handle.
    #[inline]
    pub fn get_mut(&mut self, handle: BamSystemSet) -> Option<&mut ConfiguredScheduleSet> {
        if !handle.is_valid() {
            return None;
        }
        self.sets
            .get_mut(handle.0 as usize)
            .and_then(Option::as_mut)
    }

    /// Looks up a configured schedule set by `(schedule, named_set)`.
    #[inline]
    pub fn get_by_named_set(
        &self,
        schedule: ScheduleId,
        named_set: &ModNamedSystemSet,
    ) -> Option<&ConfiguredScheduleSet> {
        let handle = *self.by_named_set.get(&(schedule, named_set.clone()))?;
        self.get(handle)
    }

    /// Iterates over all active configured schedule sets belonging to `schedule` in handle order.
    pub fn sets_for_schedule(
        &self,
        schedule: ScheduleId,
    ) -> impl Iterator<Item = &ConfiguredScheduleSet> + '_ {
        self.sets
            .iter()
            .flatten()
            .filter(move |entry| entry.schedule == schedule)
    }

    /// Allocates a monotonic [`BamSystemSet`] handle (`0, 1, ...`) mapped to a scoped
    /// [`ModNamedSystemSet`] container and validates all inter-set ordering edges for existence,
    /// same-schedule consistency, and acyclicity.
    pub fn configure_set(
        &mut self,
        cfg: ScheduleSetConfig,
    ) -> Result<BamSystemSet, (FfiResult, String)> {
        let ScheduleSetConfig {
            mod_id,
            mod_name,
            set_name,
            generation,
            schedule,
            mode,
            before_sets,
            after_sets,
            before_weak,
            after_weak,
            run_if_ctx,
            run_if_query,
            run_if_ctx_user_data,
            parent_sets,
        } = cfg;
        if self.next_handle == u32::MAX {
            return Err((
                FfiResult::CAPACITY_EXCEEDED,
                String::from("ModScheduleSetRegistry exhausted u32 BamSystemSet handles"),
            ));
        }

        let named_set = ModNamedSystemSet::with_generation(
            Arc::clone(&mod_name),
            Arc::clone(&set_name),
            generation,
        );
        if self
            .by_named_set
            .contains_key(&(schedule, named_set.clone()))
        {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                format!(
                    "schedule set '{}::{}' (gen={generation}) already configured in schedule {}",
                    mod_name, set_name, schedule.0
                ),
            ));
        }

        let dedup_before = self.validate_edge_targets(schedule, &before_sets, "before_sets")?;
        let dedup_after = self.validate_edge_targets(schedule, &after_sets, "after_sets")?;

        if self.new_node_would_create_cycle(schedule, &dedup_before, &dedup_after) {
            return Err((
                FfiResult::CYCLE_DETECTED,
                format!(
                    "schedule set '{}::{}' edges would create an ordering cycle in schedule {}",
                    mod_name, set_name, schedule.0
                ),
            ));
        }
        if self.new_node_parent_sets_would_create_cycle(
            schedule,
            mod_name.as_ref(),
            set_name.as_ref(),
            generation,
            &parent_sets,
        ) {
            return Err((
                FfiResult::CYCLE_DETECTED,
                format!(
                    "schedule set '{}::{}' parent_sets would create a hierarchy cycle in schedule {}",
                    mod_name, set_name, schedule.0
                ),
            ));
        }

        let handle = BamSystemSet::from_raw(self.next_handle);
        self.next_handle += 1;

        let interned_set = named_set.intern();
        let entry = ConfiguredScheduleSet {
            handle,
            mod_id,
            mod_name,
            set_name,
            generation,
            named_set: named_set.clone(),
            interned_set,
            schedule,
            mode,
            before_sets: dedup_before,
            after_sets: dedup_after,
            before_weak,
            after_weak,
            run_if_ctx,
            run_if_query,
            run_if_ctx_user_data,
            parent_sets,
            mounted_systems: Vec::new(),
            mounted_interned: Vec::new(),
        };
        debug_assert_eq!(self.sets.len(), handle.0 as usize);
        self.sets.push(Some(entry));
        self.by_named_set.insert((schedule, named_set), handle);
        Ok(handle)
    }

    /// Mounts `system_set` into the configured [`BamSystemSet`] container `set` in `schedule`,
    /// preserving mount order for `SEQUENCE` and `SEQUENCE_WEAK` containers.
    pub fn mount_system(
        &mut self,
        set: BamSystemSet,
        schedule: ScheduleId,
        system_set: ModNamedSystemSet,
    ) -> Result<&ConfiguredScheduleSet, (FfiResult, String)> {
        if !set.is_valid() {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                String::from("mount_system received invalid BamSystemSet handle"),
            ));
        }
        let Some(entry) = self.sets.get_mut(set.0 as usize).and_then(Option::as_mut) else {
            return Err((
                FfiResult::NOT_FOUND,
                format!("schedule set handle {} is not registered", set.0),
            ));
        };
        if entry.schedule != schedule {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                format!(
                    "schedule set handle {} belongs to schedule {}, not schedule {}",
                    set.0, entry.schedule.0, schedule.0
                ),
            ));
        }
        if !entry.mounted_systems.contains(&system_set) {
            let interned = system_set.intern();
            entry.mounted_systems.push(system_set);
            entry.mounted_interned.push(interned);
        }
        Ok(entry)
    }

    /// Removes `system_set` from all configured sets belonging to `schedule`.
    pub fn unmount_system_from_schedule(
        &mut self,
        schedule: ScheduleId,
        system_set: &ModNamedSystemSet,
    ) {
        for entry in self.sets.iter_mut().flatten() {
            if entry.schedule == schedule {
                let mut i = 0;
                while i < entry.mounted_systems.len() {
                    if &entry.mounted_systems[i] == system_set {
                        entry.mounted_systems.remove(i);
                        entry.mounted_interned.remove(i);
                    } else {
                        i += 1;
                    }
                }
            }
        }
    }

    /// Removes `system_set` from all configured sets across every schedule.
    pub fn unmount_system_anywhere(&mut self, system_set: &ModNamedSystemSet) {
        for entry in self.sets.iter_mut().flatten() {
            let mut i = 0;
            while i < entry.mounted_systems.len() {
                if &entry.mounted_systems[i] == system_set {
                    entry.mounted_systems.remove(i);
                    entry.mounted_interned.remove(i);
                } else {
                    i += 1;
                }
            }
        }
    }

    /// Updates the intra-set execution mode (`Parallel`, `Sequence`, `SequenceWeak`) of `handle`
    /// for fast schedule-only hot-rewiring.
    pub fn set_mode(
        &mut self,
        handle: BamSystemSet,
        mode: ScheduleSetMode,
    ) -> Result<(), FfiResult> {
        let Some(entry) = self.get_mut(handle) else {
            return Err(if handle.is_valid() {
                FfiResult::NOT_FOUND
            } else {
                FfiResult::INVALID_ARGUMENT
            });
        };
        entry.mode = mode;
        Ok(())
    }

    /// Replaces the inter-set ordering edges and edge strength flags of `handle`, rejecting
    /// cross-schedule handles, unknown handles, and ordering cycles with [`FfiResult::CYCLE_DETECTED`].
    pub fn set_edges(
        &mut self,
        handle: BamSystemSet,
        before_sets: Vec<BamSystemSet>,
        after_sets: Vec<BamSystemSet>,
        before_weak: bool,
        after_weak: bool,
    ) -> Result<(), FfiResult> {
        let Some(schedule) = self.get(handle).map(|e| e.schedule) else {
            return Err(if handle.is_valid() {
                FfiResult::NOT_FOUND
            } else {
                FfiResult::INVALID_ARGUMENT
            });
        };

        if before_sets.contains(&handle) || after_sets.contains(&handle) {
            return Err(FfiResult::CYCLE_DETECTED);
        }
        let dedup_before = self
            .validate_edge_targets(schedule, &before_sets, "before_sets")
            .map_err(|(code, _)| code)?;
        let dedup_after = self
            .validate_edge_targets(schedule, &after_sets, "after_sets")
            .map_err(|(code, _)| code)?;

        if self.replacement_would_create_cycle(schedule, handle, &dedup_before, &dedup_after) {
            return Err(FfiResult::CYCLE_DETECTED);
        }

        let Some(entry) = self.get_mut(handle) else {
            return Err(FfiResult::NOT_FOUND);
        };
        entry.before_sets = dedup_before;
        entry.after_sets = dedup_after;
        entry.before_weak = before_weak;
        entry.after_weak = after_weak;
        Ok(())
    }

    /// Replaces the ordered list of mounted member systems for `handle` (e.g., to reorder a
    /// `SEQUENCE` or `SEQUENCE_WEAK` set during schedule-only hot-rewiring).
    pub fn reorder_mounted_systems(
        &mut self,
        handle: BamSystemSet,
        mounted_systems: Vec<ModNamedSystemSet>,
    ) -> Result<(), FfiResult> {
        let Some(entry) = self.get_mut(handle) else {
            return Err(if handle.is_valid() {
                FfiResult::NOT_FOUND
            } else {
                FfiResult::INVALID_ARGUMENT
            });
        };
        let mut deduped = Vec::with_capacity(mounted_systems.len());
        let mut deduped_interned = Vec::with_capacity(mounted_systems.len());
        for s in mounted_systems {
            if !deduped.contains(&s) {
                let interned = s.intern();
                deduped.push(s);
                deduped_interned.push(interned);
            }
        }
        entry.mounted_systems = deduped;
        entry.mounted_interned = deduped_interned;
        Ok(())
    }

    /// Removes all configured schedule sets owned by `mod_id` and prunes edges targeting them.
    pub fn unregister_mod(&mut self, mod_id: ModId) {
        let mut removed: HashSet<BamSystemSet> = HashSet::default();
        for slot in &mut self.sets {
            if let Some(entry) = slot
                && entry.mod_id == Some(mod_id)
            {
                removed.insert(entry.handle);
                self.by_named_set
                    .remove(&(entry.schedule, entry.named_set.clone()));
                *slot = None;
            }
        }
        if !removed.is_empty() {
            for entry in self.sets.iter_mut().flatten() {
                entry.before_sets.retain(|h| !removed.contains(h));
                entry.after_sets.retain(|h| !removed.contains(h));
            }
        }
    }

    fn validate_edge_targets(
        &self,
        schedule: ScheduleId,
        targets: &[BamSystemSet],
        field_name: &str,
    ) -> Result<Vec<BamSystemSet>, (FfiResult, String)> {
        let mut deduped = Vec::with_capacity(targets.len());
        for &target in targets {
            if !target.is_valid() {
                return Err((
                    FfiResult::INVALID_ARGUMENT,
                    format!("{field_name} contains BamSystemSet::INVALID"),
                ));
            }
            let Some(entry) = self.get(target) else {
                return Err((
                    FfiResult::NOT_FOUND,
                    format!(
                        "{field_name} references unregistered BamSystemSet({})",
                        target.0
                    ),
                ));
            };
            if entry.schedule != schedule {
                return Err((
                    FfiResult::INVALID_ARGUMENT,
                    format!(
                        "{field_name} handle {} belongs to schedule {}, not schedule {}",
                        target.0, entry.schedule.0, schedule.0
                    ),
                ));
            }
            if !deduped.contains(&target) {
                deduped.push(target);
            }
        }
        Ok(deduped)
    }

    fn build_outgoing_adjacency(
        &self,
        schedule: ScheduleId,
        override_entry: Option<(BamSystemSet, &[BamSystemSet], &[BamSystemSet])>,
    ) -> HashMap<BamSystemSet, Vec<BamSystemSet>> {
        let mut adj: HashMap<BamSystemSet, Vec<BamSystemSet>> = HashMap::default();
        for entry in self.sets_for_schedule(schedule) {
            adj.entry(entry.handle).or_default();
            let (before, after): (&[BamSystemSet], &[BamSystemSet]) = match override_entry {
                Some((ov_h, ov_before, ov_after)) if ov_h == entry.handle => (ov_before, ov_after),
                _ => (&entry.before_sets, &entry.after_sets),
            };
            for &b in before {
                adj.entry(entry.handle).or_default().push(b);
            }
            for &a in after {
                adj.entry(a).or_default().push(entry.handle);
            }
        }
        adj
    }

    fn new_node_would_create_cycle(
        &self,
        schedule: ScheduleId,
        before_sets: &[BamSystemSet],
        after_sets: &[BamSystemSet],
    ) -> bool {
        if before_sets.is_empty() || after_sets.is_empty() {
            return false;
        }
        let after_lookup: HashSet<BamSystemSet> = after_sets.iter().copied().collect();
        let adj = self.build_outgoing_adjacency(schedule, None);
        let mut visited: HashSet<BamSystemSet> = HashSet::default();
        let mut queue: VecDeque<BamSystemSet> = VecDeque::new();
        for &b in before_sets {
            if after_lookup.contains(&b) {
                return true;
            }
            if visited.insert(b) {
                queue.push_back(b);
            }
        }
        while let Some(cur) = queue.pop_front() {
            if let Some(neighbors) = adj.get(&cur) {
                for &next in neighbors {
                    if after_lookup.contains(&next) {
                        return true;
                    }
                    if visited.insert(next) {
                        queue.push_back(next);
                    }
                }
            }
        }
        false
    }

    fn replacement_would_create_cycle(
        &self,
        schedule: ScheduleId,
        handle: BamSystemSet,
        before_sets: &[BamSystemSet],
        after_sets: &[BamSystemSet],
    ) -> bool {
        let adj = self.build_outgoing_adjacency(schedule, Some((handle, before_sets, after_sets)));
        // Kahn's topological sort over all nodes in `schedule`.
        let mut indegree: HashMap<BamSystemSet, usize> = adj.keys().map(|&k| (k, 0usize)).collect();
        for neighbors in adj.values() {
            for &dst in neighbors {
                *indegree.entry(dst).or_insert(0) += 1;
            }
        }
        let mut queue: VecDeque<BamSystemSet> = indegree
            .iter()
            .filter_map(|(&k, &deg)| (deg == 0).then_some(k))
            .collect();
        let mut visited = 0usize;
        while let Some(node) = queue.pop_front() {
            visited += 1;
            if let Some(neighbors) = adj.get(&node) {
                for &dst in neighbors {
                    if let Some(deg) = indegree.get_mut(&dst) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push_back(dst);
                        }
                    }
                }
            }
        }
        visited != indegree.len()
    }

    /// Validates that all `parent_sets` hierarchy edges (`in_set`) among configured sets in
    /// `schedule` are acyclic, rejecting self-parenting and multi-node cycles with
    /// [`FfiResult::CYCLE_DETECTED`].
    pub fn validate_parent_sets_in_schedule(
        &self,
        schedule: ScheduleId,
    ) -> Result<(), (FfiResult, String)> {
        let entries: Vec<&ConfiguredScheduleSet> = self.sets_for_schedule(schedule).collect();
        if entries.is_empty() {
            return Ok(());
        }
        let find_target_idx = |mod_name: &str, set_name: &str, prefer_gen: u64| -> Option<usize> {
            entries
                .iter()
                .position(|cand| {
                    cand.mod_name.as_ref() == mod_name
                        && cand.set_name.as_ref() == set_name
                        && cand.generation == prefer_gen
                })
                .or_else(|| {
                    entries.iter().position(|cand| {
                        cand.mod_name.as_ref() == mod_name && cand.set_name.as_ref() == set_name
                    })
                })
        };

        let n = entries.len();
        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut indegree: Vec<usize> = vec![0; n];
        for (idx, entry) in entries.iter().enumerate() {
            for parent_raw in &entry.parent_sets {
                if parent_raw.starts_with("host::") {
                    continue;
                }
                let Some(parsed) = ModNamedSystemSet::parse_target(parent_raw, &entry.mod_name)
                else {
                    continue;
                };
                let Some(parent_idx) = find_target_idx(
                    parsed.mod_name.as_ref(),
                    parsed.system_name.as_ref(),
                    entry.generation,
                ) else {
                    continue;
                };
                if parent_idx == idx {
                    return Err((
                        FfiResult::CYCLE_DETECTED,
                        format!(
                            "schedule set '{}::{}' cannot list itself in parent_sets in schedule {}",
                            entry.mod_name, entry.set_name, schedule.0
                        ),
                    ));
                }
                if !adj[parent_idx].contains(&idx) {
                    adj[parent_idx].push(idx);
                    indegree[idx] += 1;
                }
            }
        }

        let mut queue: VecDeque<usize> = indegree
            .iter()
            .enumerate()
            .filter_map(|(i, &deg)| (deg == 0).then_some(i))
            .collect();
        let mut visited = 0usize;
        while let Some(cur) = queue.pop_front() {
            visited += 1;
            for &dst in &adj[cur] {
                indegree[dst] -= 1;
                if indegree[dst] == 0 {
                    queue.push_back(dst);
                }
            }
        }
        if visited != n {
            return Err((
                FfiResult::CYCLE_DETECTED,
                format!(
                    "parent_sets hierarchy contains a cycle in schedule {}",
                    schedule.0
                ),
            ));
        }
        Ok(())
    }

    /// Returns `true` if `parent_sets` hierarchy edges among configured sets in `schedule` form
    /// any self-loop or multi-node cycle.
    #[inline]
    pub fn has_parent_hierarchy_cycle(&self, schedule: ScheduleId) -> bool {
        self.validate_parent_sets_in_schedule(schedule).is_err()
    }

    fn new_node_parent_sets_would_create_cycle(
        &self,
        schedule: ScheduleId,
        mod_name: &str,
        set_name: &str,
        generation: u64,
        parent_sets: &[String],
    ) -> bool {
        for parent_raw in parent_sets {
            if parent_raw.starts_with("host::") {
                continue;
            }
            if let Some(parsed) = ModNamedSystemSet::parse_target(parent_raw, mod_name)
                && parsed.mod_name.as_ref() == mod_name
                && parsed.system_name.as_ref() == set_name
            {
                return true;
            }
        }

        let existing: Vec<&ConfiguredScheduleSet> = self.sets_for_schedule(schedule).collect();
        let new_idx = existing.len();
        let n = new_idx + 1;
        let find_idx = |m: &str, s: &str, prefer_gen: u64| -> Option<usize> {
            if m == mod_name && s == set_name {
                return Some(new_idx);
            }
            existing
                .iter()
                .position(|cand| {
                    cand.mod_name.as_ref() == m
                        && cand.set_name.as_ref() == s
                        && cand.generation == prefer_gen
                })
                .or_else(|| {
                    existing
                        .iter()
                        .position(|cand| cand.mod_name.as_ref() == m && cand.set_name.as_ref() == s)
                })
        };

        let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
        let mut indegree: Vec<usize> = vec![0; n];
        for (idx, entry) in existing.iter().enumerate() {
            for parent_raw in &entry.parent_sets {
                if parent_raw.starts_with("host::") {
                    continue;
                }
                let Some(parsed) = ModNamedSystemSet::parse_target(parent_raw, &entry.mod_name)
                else {
                    continue;
                };
                let Some(parent_idx) = find_idx(
                    parsed.mod_name.as_ref(),
                    parsed.system_name.as_ref(),
                    entry.generation,
                ) else {
                    continue;
                };
                if parent_idx == idx {
                    return true;
                }
                if !adj[parent_idx].contains(&idx) {
                    adj[parent_idx].push(idx);
                    indegree[idx] += 1;
                }
            }
        }
        for parent_raw in parent_sets {
            if parent_raw.starts_with("host::") {
                continue;
            }
            let Some(parsed) = ModNamedSystemSet::parse_target(parent_raw, mod_name) else {
                continue;
            };
            let Some(parent_idx) = find_idx(
                parsed.mod_name.as_ref(),
                parsed.system_name.as_ref(),
                generation,
            ) else {
                continue;
            };
            if parent_idx == new_idx {
                return true;
            }
            if !adj[parent_idx].contains(&new_idx) {
                adj[parent_idx].push(new_idx);
                indegree[new_idx] += 1;
            }
        }

        let mut queue: VecDeque<usize> = indegree
            .iter()
            .enumerate()
            .filter_map(|(i, &deg)| (deg == 0).then_some(i))
            .collect();
        let mut visited = 0usize;
        while let Some(cur) = queue.pop_front() {
            visited += 1;
            for &dst in &adj[cur] {
                indegree[dst] -= 1;
                if indegree[dst] == 0 {
                    queue.push_back(dst);
                }
            }
        }
        visited != n
    }
}

/// Resolves a built-in (`0..=5`) or custom (`>= 1024`) [`ScheduleId`] against `world`'s
/// [`ModScheduleMap`].
#[inline]
pub fn resolve_schedule_id_in_world(
    world: &World,
    id: ScheduleId,
) -> Option<InternedScheduleLabel> {
    if let Some(builtin) = resolve_builtin_schedule_id(id) {
        return Some(builtin);
    }
    world
        .get_resource::<ModScheduleMap>()
        .and_then(|map| map.resolve(id))
}

/// Applies all [`ConfiguredScheduleSet`] definitions for `schedule_id` directly onto `schedule`
/// using Bevy `0.20`'s native `.chain()`, `.chain_weak()`, `.before()`, `.after()`,
/// `.before_weak()`, and `.after_weak()` APIs.
pub fn apply_mod_schedule_sets_to_schedule(
    registry: &ModScheduleSetRegistry,
    schedule_id: ScheduleId,
    schedule: &mut Schedule,
) {
    for entry in registry.sets_for_schedule(schedule_id) {
        schedule.configure_sets(entry.named_set.clone());

        for parent_raw in &entry.parent_sets {
            if let Some(parsed) = ModNamedSystemSet::parse_target(parent_raw, &entry.mod_name) {
                let parent_gen = registry
                    .sets_for_schedule(schedule_id)
                    .find(|cand| {
                        cand.mod_name == parsed.mod_name && cand.set_name == parsed.system_name
                    })
                    .map_or(entry.generation, |cand| cand.generation);
                schedule.configure_sets(
                    entry
                        .named_set
                        .clone()
                        .in_set(parsed.at_generation(parent_gen)),
                );
            }
        }

        for &target_h in &entry.before_sets {
            if let Some(target) = registry.get(target_h) {
                if entry.before_weak {
                    schedule.configure_sets(
                        entry
                            .named_set
                            .clone()
                            .before_weak(target.named_set.clone()),
                    );
                } else {
                    schedule
                        .configure_sets(entry.named_set.clone().before(target.named_set.clone()));
                }
            }
        }

        for &target_h in &entry.after_sets {
            if let Some(target) = registry.get(target_h) {
                if entry.after_weak {
                    schedule.configure_sets(
                        entry.named_set.clone().after_weak(target.named_set.clone()),
                    );
                } else {
                    schedule
                        .configure_sets(entry.named_set.clone().after(target.named_set.clone()));
                }
            }
        }

        match entry.mode {
            ScheduleSetMode::Parallel => {}
            ScheduleSetMode::Sequence => {
                for pair in entry.mounted_systems.windows(2) {
                    schedule.configure_sets((pair[0].clone(), pair[1].clone()).chain());
                }
            }
            ScheduleSetMode::SequenceWeak => {
                for pair in entry.mounted_systems.windows(2) {
                    schedule.configure_sets((pair[0].clone(), pair[1].clone()).chain_weak());
                }
            }
        }
    }
}

/// Internal marker set proving [`ModScheduleSetRewirePass`] is already installed on a [`Schedule`].
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ModSchedulePassMarker(ScheduleId);

/// Internal empty sentinel set touched during [`rewire_mod_schedule_sets_in_schedule`] to mark
/// `ScheduleGraph` dirty without adding or re-initializing any systems.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ModScheduleRewireMarker(ScheduleId);

/// Marker set guarding idempotent `run_if_ctx` condition attachment on a dynamic [`BamSystemSet`].
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ModScheduleSetConditionMarker(BamSystemSet, u64);

/// Marker set guarding idempotent `in_set` parent-set linkage on a dynamic [`BamSystemSet`].
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
struct ModScheduleSetParentMarker(BamSystemSet, u64, String);

/// Read-only Bevy condition [`System`] evaluating a configured [`ScheduleSetConfig::run_if_ctx`]
/// C-ABI predicate against an optional read-only query clause.
struct ScheduleSetRunIfCtxCondition {
    mod_id: Option<ModId>,
    mod_name: Arc<str>,
    debug_name: DebugName,
    callback: GuestFnPtr<BamSystemFn>,
    query_spec: Option<BamQuerySpecOwned>,
    query_group: Option<CachedQueryGroup>,
    user_data: GuestUserData,
    snapshot: Arc<RegistrySnapshot>,
    is_active: Arc<AtomicBool>,
    access: SystemAccess,
    last_run: Tick,
    this_run: Tick,
    init_error: bool,
    last_error_buf: String,
}

impl ScheduleSetRunIfCtxCondition {
    fn new(entry: &ConfiguredScheduleSet, callback: BamSystemFn) -> Self {
        let debug_name = DebugName::owned(format!(
            "bam_set_run_if_ctx({}::{})",
            entry.mod_name, entry.set_name
        ));
        Self {
            mod_id: entry.mod_id,
            mod_name: Arc::clone(&entry.mod_name),
            debug_name,
            callback: GuestFnPtr(callback),
            query_spec: entry.run_if_query.clone(),
            query_group: entry.run_if_query.clone().map(CachedQueryGroup::new),
            user_data: entry.run_if_ctx_user_data,
            snapshot: Arc::new(RegistrySnapshot::default()),
            is_active: Arc::new(AtomicBool::new(true)),
            access: SystemAccess::None,
            last_run: Tick::new(0),
            this_run: Tick::new(0),
            init_error: false,
            last_error_buf: String::new(),
        }
    }
}

impl System for ScheduleSetRunIfCtxCondition {
    type In = ();
    type Out = bool;

    fn name(&self) -> DebugName {
        self.debug_name.clone()
    }

    fn flags(&self) -> SystemStateFlags {
        SystemStateFlags::empty()
    }

    fn default_system_sets(&self) -> Vec<InternedSystemSet> {
        Vec::new()
    }

    fn initialize(&mut self, world: &mut World) -> SystemAccess {
        self.this_run = world.change_tick();
        self.last_run = Tick::new(world.change_tick().get().wrapping_sub(Tick::MAX.get()));
        self.init_error = false;
        self.access = SystemAccess::None;

        if let Some(world_reg) = world.get_resource::<ModRegistry>() {
            self.snapshot = world_reg.snapshot_arc();
        }
        if let Some(mod_id) = self.mod_id {
            let active = world
                .get_resource::<ModHost>()
                .and_then(|h| h.get_mod(mod_id))
                .map(crate::loader::LoadedMod::is_active_flag)
                .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
            self.is_active = active;
        }

        if let Some(ref spec) = self.query_spec {
            if spec.terms.iter().any(|term| term.mode.is_mutable()) {
                self.init_error = true;
                return self.access.clone();
            }
            let mut group = self
                .query_group
                .take()
                .unwrap_or_else(|| CachedQueryGroup::new(spec.clone()));
            match initialize_query_groups_in_world_with_base_idx(
                world,
                &self.snapshot,
                core::slice::from_mut(&mut group),
                usize::MAX,
            ) {
                Ok(access_set) => {
                    group.update_archetypes(world.as_unsafe_world_cell());
                    self.query_group = Some(group);
                    self.access = SystemAccess::Shared(access_set);
                }
                Err(_) => {
                    self.init_error = true;
                    self.query_group = None;
                }
            }
        }
        self.access.clone()
    }

    /// Evaluates this schedule-set `run_if_ctx` predicate over scheduler-verified read-only
    /// `UnsafeWorldCell` borrows.
    ///
    /// # Safety
    /// Caller (Bevy's schedule executor) must ensure `world` grants all read accesses declared in
    /// `self.access` returned by `System::initialize`, and that `run_unsafe` is not invoked
    /// concurrently with overlapping mutable accesses to the same `ScheduleSetRunIfCtxCondition`
    /// instance or the world data it reads.
    unsafe fn run_unsafe(
        &mut self,
        _input: SystemIn<'_, Self>,
        world: UnsafeWorldCell,
    ) -> Result<Self::Out, RunSystemError> {
        if self.init_error || !self.is_active.load(Ordering::Acquire) {
            return Ok(false);
        }

        let current_tick = world.change_tick();
        self.this_run = current_tick;
        let last_run = self.last_run;

        let cond_gen = crate::handle_header::next_non_zero_generation();
        let mut iter_storage = if let Some(ref mut group) = self.query_group {
            group.update_archetypes(world);
            // SAFETY: Scheduler guarantees read-only access declared in `self.access`.
            let (count, mut iters, views) = unsafe {
                build_query_slot_iters_bucketed::<1>(
                    world,
                    core::slice::from_mut(group),
                    last_run,
                    current_tick,
                    cond_gen,
                )
            };
            Some((count, iters[0].take(), views))
        } else {
            None
        };

        let mut iter_ptrs: [*mut BamIter; 1] = [core::ptr::null_mut(); 1];
        let mut query_views: [Option<RandomQueryView<'_>>; 1] = [None; 1];
        let mut cond_query_count = 0usize;
        if let Some((count, ref mut first_iter, views)) = iter_storage {
            cond_query_count = count;
            if let Some(it) = first_iter.as_mut() {
                iter_ptrs[0] = it.as_ffi_ptr();
            }
            query_views = views;
        }

        let err_buf = core::mem::take(&mut self.last_error_buf);
        let host_ctx = HostContext::with_error_buffer(Arc::clone(&self.snapshot), err_buf)
            .with_namespace(Some(Arc::clone(&self.mod_name)));
        let mut sys_ctx = HostSystemCtx::new_reused_full(
            host_ctx,
            cond_gen,
            &iter_ptrs[..cond_query_count],
            &query_views[..cond_query_count],
            core::ptr::null_mut(),
            (world, last_run, current_tick),
            (&[], Vec::new(), Vec::new()),
        );
        let sys_ctx_ptr = sys_ctx.as_ffi_ptr();
        let GuestFnPtr(cond_fn) = self.callback;
        // SAFETY: Invoking read-only C-ABI `run_if_ctx` predicate with stack-scoped
        // `HostSystemCtx` (`commands_ptr = null`) while `self.is_active` is armed.
        let status = unsafe { cond_fn(sys_ctx_ptr, self.user_data.as_ptr()) };
        self.last_error_buf = sys_ctx.reclaim_error_buffer();
        self.last_run = current_tick;

        Ok(status.is_ok())
    }

    fn apply_deferred(&mut self, _world: &mut World) {}

    fn queue_deferred(&mut self, _world: DeferredWorld) {}

    fn check_change_tick(&mut self, check: CheckChangeTicks) {
        self.last_run.check_tick(check);
        self.this_run.check_tick(check);
    }

    fn get_last_run(&self) -> Tick {
        self.last_run
    }

    fn set_last_run(&mut self, last_run: Tick) {
        self.last_run = last_run;
    }
}

// SAFETY: `ScheduleSetRunIfCtxCondition` validates all terms are non-mutable in `initialize()`,
// provides no `BamCommands` handle (`commands_ptr = null`), and only performs read-only queries.
unsafe impl ReadOnlySystem for ScheduleSetRunIfCtxCondition {}

type PendingParentEdge = (
    ModNamedSystemSet,
    InternedSystemSet,
    InternedSystemSet,
    ModScheduleSetParentMarker,
);

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum SimSetId {
    Existing(SystemSetKey),
    Pending(usize),
}

impl GraphNodeId for SimSetId {
    type Adjacent = (SimSetId, Direction);
    type Edge = (SimSetId, SimSetId);

    fn kind(&self) -> &'static str {
        "system set"
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
enum SimNodeId {
    System(SystemKey),
    Set(SimSetId),
}

impl GraphNodeId for SimNodeId {
    type Adjacent = (SimNodeId, Direction);
    type Edge = (SimNodeId, SimNodeId);

    fn kind(&self) -> &'static str {
        match self {
            Self::System(_) => "system",
            Self::Set(_) => "system set",
        }
    }
}

impl From<SimSetId> for SimNodeId {
    #[inline]
    fn from(value: SimSetId) -> Self {
        Self::Set(value)
    }
}

impl From<SystemKey> for SimNodeId {
    #[inline]
    fn from(value: SystemKey) -> Self {
        Self::System(value)
    }
}

impl TryFrom<SimNodeId> for SimSetId {
    type Error = SystemKey;

    #[inline]
    fn try_from(value: SimNodeId) -> Result<Self, Self::Error> {
        match value {
            SimNodeId::Set(s) => Ok(s),
            SimNodeId::System(sys) => Err(sys),
        }
    }
}

impl TryFrom<SimNodeId> for SystemKey {
    type Error = SimSetId;

    #[inline]
    fn try_from(value: SimNodeId) -> Result<Self, Self::Error> {
        match value {
            SimNodeId::System(sys) => Ok(sys),
            SimNodeId::Set(s) => Err(s),
        }
    }
}

fn collect_pending_parent_sets(
    world: &World,
    registry: &ModScheduleSetRegistry,
    schedule_id: ScheduleId,
    schedule: &Schedule,
) -> Vec<PendingParentEdge> {
    let visible_sets = world.get_resource::<ModVisibleSets>();
    let mod_host = world.get_resource::<ModHost>();
    let system_sets = &schedule.graph().system_sets;
    let mut pending_parents = Vec::new();

    for entry in registry.sets_for_schedule(schedule_id) {
        for parent_raw in &entry.parent_sets {
            let parent_marker =
                ModScheduleSetParentMarker(entry.handle, entry.generation, parent_raw.clone());
            if system_sets.contains(parent_marker.clone()) {
                continue;
            }
            let parent_interned = if let Some(stripped) = parent_raw.strip_prefix("host::") {
                visible_sets.and_then(|sets| sets.resolve(stripped))
            } else if let Some(parsed) =
                ModNamedSystemSet::parse_target(parent_raw, &entry.mod_name)
            {
                let target_gen = if parsed.mod_name == entry.mod_name {
                    registry
                        .sets_for_schedule(schedule_id)
                        .find(|cand| {
                            cand.mod_name == parsed.mod_name && cand.set_name == parsed.system_name
                        })
                        .map_or(entry.generation, |cand| cand.generation)
                } else {
                    mod_host
                        .map(|h| h.mod_generation(parsed.mod_name.as_ref()))
                        .unwrap_or(0)
                };
                Some(parsed.at_generation(target_gen).intern())
            } else {
                None
            };
            if let Some(parent) = parent_interned {
                pending_parents.push((
                    entry.named_set.clone(),
                    entry.interned_set,
                    parent,
                    parent_marker,
                ));
            }
        }
    }
    pending_parents
}

fn prevalidate_mod_schedule_set_rewire_inner(
    world: &World,
    registry: &ModScheduleSetRegistry,
    schedule_id: ScheduleId,
    schedule: &Schedule,
    pending_parents: &[PendingParentEdge],
) -> Result<(), ScheduleBuildError> {
    if registry.has_parent_hierarchy_cycle(schedule_id) {
        return Err(ScheduleBuildError::HierarchySort(
            DiGraphToposortError::Cycle(Vec::new()),
        ));
    }

    let graph = schedule.graph();
    let mod_host = world.get_resource::<ModHost>();

    let mut pending_interned_map: HashMap<InternedSystemSet, SimSetId> = HashMap::default();
    let mut next_pending_idx = 0usize;
    let mut ensure_sim_set = |interned: InternedSystemSet| -> SimSetId {
        if let Some(key) = graph.system_sets.get_key(interned)
            && graph.system_sets.get(key).is_some()
        {
            return SimSetId::Existing(key);
        }
        *pending_interned_map.entry(interned).or_insert_with(|| {
            let id = SimSetId::Pending(next_pending_idx);
            next_pending_idx += 1;
            id
        })
    };

    let to_sim_node = |node: NodeId| -> SimNodeId {
        match node {
            NodeId::System(k) => SimNodeId::System(k),
            NodeId::Set(k) => SimNodeId::Set(SimSetId::Existing(k)),
        }
    };
    let to_node_id = |node: SimNodeId| -> Option<NodeId> {
        match node {
            SimNodeId::System(k) => Some(NodeId::System(k)),
            SimNodeId::Set(SimSetId::Existing(k)) => Some(NodeId::Set(k)),
            SimNodeId::Set(SimSetId::Pending(_)) => None,
        }
    };

    let mut sim_hierarchy: Dag<SimNodeId> = Dag::new();
    for n in graph.hierarchy().graph().nodes() {
        sim_hierarchy.add_node(to_sim_node(n));
    }
    for (a, b) in graph.hierarchy().graph().all_edges() {
        sim_hierarchy.add_edge(to_sim_node(a), to_sim_node(b));
    }

    let mut sim_dependency: Dag<SimNodeId> = Dag::new();
    for n in graph.dependency().graph().nodes() {
        let sn = to_sim_node(n);
        sim_hierarchy.add_node(sn);
        sim_dependency.add_node(sn);
    }
    for (a, b) in graph.dependency().graph().all_edges() {
        sim_dependency.add_edge(to_sim_node(a), to_sim_node(b));
    }

    for entry in registry.sets_for_schedule(schedule_id) {
        let sim_set = ensure_sim_set(entry.interned_set);
        sim_hierarchy.add_node(SimNodeId::Set(sim_set));
        sim_dependency.add_node(SimNodeId::Set(sim_set));
    }
    for (_, child_interned, parent_interned, _) in pending_parents {
        let child_sim = ensure_sim_set(*child_interned);
        let parent_sim = ensure_sim_set(*parent_interned);
        sim_hierarchy.add_edge(SimNodeId::Set(parent_sim), SimNodeId::Set(child_sim));
        sim_dependency.add_node(SimNodeId::Set(parent_sim));
        sim_dependency.add_node(SimNodeId::Set(child_sim));
    }

    let map_toposort_err = |err: DiGraphToposortError<SimNodeId>| -> DiGraphToposortError<NodeId> {
        match err {
            DiGraphToposortError::Loop(n) => to_node_id(n).map_or_else(
                || DiGraphToposortError::Cycle(Vec::new()),
                DiGraphToposortError::Loop,
            ),
            DiGraphToposortError::Cycle(cycles) => {
                let mapped: Vec<Vec<NodeId>> = cycles
                    .into_iter()
                    .filter_map(|cycle| {
                        let nodes: Vec<NodeId> = cycle.into_iter().filter_map(to_node_id).collect();
                        (!nodes.is_empty()).then_some(nodes)
                    })
                    .collect();
                DiGraphToposortError::Cycle(mapped)
            }
        }
    };

    let hierarchy_analysis = sim_hierarchy
        .analyze()
        .map_err(|err| ScheduleBuildError::HierarchySort(map_toposort_err(err)))?;

    if schedule.get_build_settings().hierarchy_detection == LogLevel::Error
        && let Err(e) = hierarchy_analysis.check_for_redundant_edges()
    {
        let mapped_edges: Vec<(NodeId, NodeId)> =
            e.0.into_iter()
                .filter_map(|(a, b)| Some((to_node_id(a)?, to_node_id(b)?)))
                .collect();
        return Err(
            ScheduleBuildWarning::HierarchyRedundancy(DagRedundancyError(mapped_edges)).into(),
        );
    }
    sim_hierarchy.remove_redundant_edges(&hierarchy_analysis);

    let dependency_analysis = sim_dependency
        .analyze()
        .map_err(|err| ScheduleBuildError::DependencySort(map_toposort_err(err)))?;

    if let Err(DagCrossDependencyError(a, b)) =
        dependency_analysis.check_for_cross_dependencies(&hierarchy_analysis)
    {
        if let (Some(na), Some(nb)) = (to_node_id(a), to_node_id(b)) {
            return Err(ScheduleBuildError::CrossDependency(
                DagCrossDependencyError(na, nb),
            ));
        }
        return Err(ScheduleBuildError::HierarchySort(
            DiGraphToposortError::Cycle(Vec::new()),
        ));
    }

    let set_systems = sim_hierarchy
        .group_by_key::<SimSetId, SystemKey>(graph.system_sets.len() + next_pending_idx)
        .map_err(|err| ScheduleBuildError::HierarchySort(map_toposort_err(err)))?;

    if let Err(DagOverlappingGroupError(a, b)) =
        dependency_analysis.check_for_overlapping_groups(&set_systems)
    {
        if let (SimSetId::Existing(ka), SimSetId::Existing(kb)) = (a, b) {
            return Err(ScheduleBuildError::SetsHaveOrderButIntersect(
                DagOverlappingGroupError(ka, kb),
            ));
        }
        return Err(ScheduleBuildError::HierarchySort(
            DiGraphToposortError::Cycle(Vec::new()),
        ));
    }

    let mut flat_dependency = set_systems.flatten(sim_dependency, |_, _, _, _| {});

    let resolve_sim_set_key =
        |set: &ModNamedSystemSet, interned: InternedSystemSet| -> Option<SimSetId> {
            if let Some(key) = graph.system_sets.get_key(interned)
                && graph.system_sets.get(key).is_some()
            {
                return Some(SimSetId::Existing(key));
            }
            if let Some(&sim_key) = pending_interned_map.get(&interned) {
                return Some(sim_key);
            }
            if let Some(host) = mod_host {
                let live_gen = host.mod_generation(set.mod_name.as_ref());
                if live_gen != set.generation {
                    let alt = set.clone().at_generation(live_gen);
                    let alt_interned = alt.intern();
                    if let Some(key) = graph.system_sets.get_key(alt_interned)
                        && graph.system_sets.get(key).is_some()
                    {
                        return Some(SimSetId::Existing(key));
                    }
                    if let Some(&sim_key) = pending_interned_map.get(&alt_interned) {
                        return Some(sim_key);
                    }
                }
            }
            None
        };

    let systems_in_sim_set = |sim_key: SimSetId| -> Vec<SystemKey> {
        set_systems
            .get(&sim_key)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    };

    let mut strict_edges: Vec<(SystemKey, SystemKey)> = Vec::new();
    let mut weak_edges: Vec<(SystemKey, SystemKey)> = Vec::new();

    for entry in registry.sets_for_schedule(schedule_id) {
        let own_systems: Vec<SystemKey> = resolve_sim_set_key(&entry.named_set, entry.interned_set)
            .map(systems_in_sim_set)
            .unwrap_or_default();

        if !own_systems.is_empty() {
            for &target_h in &entry.before_sets {
                let Some(target) = registry.get(target_h) else {
                    continue;
                };
                let Some(target_key) = resolve_sim_set_key(&target.named_set, target.interned_set)
                else {
                    continue;
                };
                let target_systems = systems_in_sim_set(target_key);
                let bucket = if entry.before_weak {
                    &mut weak_edges
                } else {
                    &mut strict_edges
                };
                for &src in &own_systems {
                    for &dst in &target_systems {
                        if src != dst {
                            bucket.push((src, dst));
                        }
                    }
                }
            }

            for &target_h in &entry.after_sets {
                let Some(target) = registry.get(target_h) else {
                    continue;
                };
                let Some(target_key) = resolve_sim_set_key(&target.named_set, target.interned_set)
                else {
                    continue;
                };
                let target_systems = systems_in_sim_set(target_key);
                let bucket = if entry.after_weak {
                    &mut weak_edges
                } else {
                    &mut strict_edges
                };
                for &prev in &target_systems {
                    for &dst in &own_systems {
                        if prev != dst {
                            bucket.push((prev, dst));
                        }
                    }
                }
            }
        }

        match entry.mode {
            ScheduleSetMode::Parallel => {}
            ScheduleSetMode::Sequence | ScheduleSetMode::SequenceWeak => {
                let resolved_members: Vec<Vec<SystemKey>> = entry
                    .mounted_systems
                    .iter()
                    .zip(entry.mounted_interned.iter().copied())
                    .filter_map(|(member_set, member_interned)| {
                        let key = resolve_sim_set_key(member_set, member_interned)?;
                        let systems = systems_in_sim_set(key);
                        (!systems.is_empty()).then_some(systems)
                    })
                    .collect();
                let bucket = if entry.mode == ScheduleSetMode::SequenceWeak {
                    &mut weak_edges
                } else {
                    &mut strict_edges
                };
                for pair in resolved_members.windows(2) {
                    for &src in &pair[0] {
                        for &dst in &pair[1] {
                            if src != dst {
                                bucket.push((src, dst));
                            }
                        }
                    }
                }
            }
        }
    }

    for (from, to) in strict_edges {
        flat_dependency.add_edge(from, to);
    }
    for (from, to) in weak_edges {
        flat_dependency.add_edge(from, to);
    }

    flat_dependency
        .analyze()
        .map_err(ScheduleBuildError::FlatDependencySort)?;

    Ok(())
}

/// Performs non-mutating pre-validation of `registry`'s `parent_sets` (`in_set`) and
/// [`ModScheduleSetRewirePass`] edges for `schedule_id` against `schedule`'s existing
/// `ScheduleGraph` without calling `schedule.configure_sets(...)`.
pub fn prevalidate_mod_schedule_set_rewire_in_schedule(
    world: &World,
    registry: &ModScheduleSetRegistry,
    schedule_id: ScheduleId,
    schedule: &Schedule,
) -> Result<(), ScheduleBuildError> {
    let pending_parents = collect_pending_parent_sets(world, registry, schedule_id, schedule);
    prevalidate_mod_schedule_set_rewire_inner(
        world,
        registry,
        schedule_id,
        schedule,
        &pending_parents,
    )
}

/// Performs non-mutating pre-validation of `registry`'s `parent_sets` (`in_set`) and
/// [`ModScheduleSetRewirePass`] edges for `schedule_id` against the matching [`Schedule`] in
/// `world`'s [`Schedules`] resource (or a temporary empty schedule if absent) without mutating
/// `world` or any live [`ScheduleGraph`].
pub fn prevalidate_mod_schedule_set_rewire_in_world(
    world: &World,
    registry: &ModScheduleSetRegistry,
    schedule_id: ScheduleId,
) -> Result<(), ScheduleBuildError> {
    let Some(label) = resolve_schedule_id_in_world(world, schedule_id) else {
        return Ok(());
    };
    if let Some(schedules) = world.get_resource::<Schedules>()
        && let Some(schedule) = schedules.get(label)
    {
        return prevalidate_mod_schedule_set_rewire_in_schedule(
            world,
            registry,
            schedule_id,
            schedule,
        );
    }
    let empty_schedule = Schedule::new(label);
    prevalidate_mod_schedule_set_rewire_in_schedule(world, registry, schedule_id, &empty_schedule)
}

/// Validates and synchronizes `parent_sets` (`in_set`) and `run_if_ctx` conditions from
/// [`ModScheduleSetRegistry`] onto `schedule` prior to [`Schedule::initialize`], refusing to
/// mutate `schedule` if `parent_sets` or projected [`ModScheduleSetRewirePass`] edges fail
/// validation (`HierarchySort`, `CrossDependency`, `SetsHaveOrderButIntersect`, or
/// `FlatDependencySort`).
pub fn try_sync_mod_schedule_set_configs_to_schedule(
    world: &World,
    schedule_id: ScheduleId,
    schedule: &mut Schedule,
) -> Result<(), ScheduleBuildError> {
    let Some(registry) = world.get_resource::<ModScheduleSetRegistry>() else {
        return Ok(());
    };
    if registry.has_parent_hierarchy_cycle(schedule_id) {
        return Err(ScheduleBuildError::HierarchySort(
            DiGraphToposortError::Cycle(Vec::new()),
        ));
    }

    let pending_parents = collect_pending_parent_sets(world, registry, schedule_id, schedule);
    let mut has_unregistered_set = false;
    let mut has_pending_condition = false;
    {
        let system_sets = &schedule.graph().system_sets;
        for entry in registry.sets_for_schedule(schedule_id) {
            let is_present = system_sets
                .get_key(entry.interned_set)
                .is_some_and(|key| system_sets.get(key).is_some());
            if !is_present {
                has_unregistered_set = true;
            }
            if entry.run_if_ctx.is_some()
                && !system_sets.contains(ModScheduleSetConditionMarker(
                    entry.handle,
                    entry.generation,
                ))
            {
                has_pending_condition = true;
            }
        }
    }

    if !pending_parents.is_empty() || has_unregistered_set || has_pending_condition {
        prevalidate_mod_schedule_set_rewire_inner(
            world,
            registry,
            schedule_id,
            schedule,
            &pending_parents,
        )?;
    }

    for entry in registry.sets_for_schedule(schedule_id) {
        let is_present = schedule
            .graph()
            .system_sets
            .get_key(entry.interned_set)
            .is_some_and(|key| schedule.graph().system_sets.get(key).is_some());
        if !is_present {
            schedule.configure_sets(entry.named_set.clone());
        }
        if let Some(cond_fn) = entry.run_if_ctx {
            let cond_marker = ModScheduleSetConditionMarker(entry.handle, entry.generation);
            if !schedule.graph().system_sets.contains(cond_marker) {
                let cond = ScheduleSetRunIfCtxCondition::new(entry, cond_fn);
                schedule.configure_sets((entry.named_set.clone().run_if(cond), cond_marker));
            }
        }
    }
    for (named_set, _, parent, parent_marker) in pending_parents {
        schedule.configure_sets((named_set.in_set(parent), parent_marker));
    }
    Ok(())
}

/// Synchronizes `parent_sets` (`in_set`) and `run_if_ctx` conditions from
/// [`ModScheduleSetRegistry`] onto `schedule` prior to [`Schedule::initialize`].
pub fn sync_mod_schedule_set_configs_to_schedule(
    world: &World,
    schedule_id: ScheduleId,
    schedule: &mut Schedule,
) {
    if let Err(err) = try_sync_mod_schedule_set_configs_to_schedule(world, schedule_id, schedule) {
        tracing::error!(
            schedule_id = schedule_id.0,
            error = ?err,
            "refusing to apply cyclic parent_sets onto ScheduleGraph"
        );
    }
}

/// Bevy `0.20` [`ScheduleBuildPass`] that materializes [`ModScheduleSetRegistry`] inter-set edges
/// (`before`/`before_weak`/`after`/`after_weak`) and intra-set ordering modes (`SEQUENCE` /
/// `SEQUENCE_WEAK` / `PARALLEL`) directly onto `FlattenedDependencies` on every schedule build.
///
/// Because edges live in `ModScheduleSetRegistry` rather than permanently in `ScheduleGraph`'s
/// static dependency graph, [`rewire_mod_schedule_sets_in_world`] can reverse edges, reorder
/// members, or switch set modes in `< 0.4 ms` without removing or re-initializing any
/// [`GuestSystem`] kernels.
#[derive(Debug, Clone, Copy)]
pub struct ModScheduleSetRewirePass {
    schedule_id: ScheduleId,
}

impl ModScheduleSetRewirePass {
    /// Creates a dynamic schedule-set rewire pass bound to `schedule_id`.
    #[inline]
    pub const fn new(schedule_id: ScheduleId) -> Self {
        Self { schedule_id }
    }
}

fn resolve_set_key_in_graph(
    graph: &ScheduleGraph,
    set: &ModNamedSystemSet,
    interned: InternedSystemSet,
    mod_host: Option<&ModHost>,
) -> Option<SystemSetKey> {
    if let Some(key) = graph.system_sets.get_key(interned)
        && graph.system_sets.get(key).is_some()
    {
        return Some(key);
    }
    if let Some(host) = mod_host {
        let live_gen = host.mod_generation(set.mod_name.as_ref());
        if live_gen != set.generation {
            let alt = set.clone().at_generation(live_gen);
            if let Some(key) = graph.system_sets.get_key(alt.intern())
                && graph.system_sets.get(key).is_some()
            {
                return Some(key);
            }
        }
    }
    None
}

fn collect_systems_in_set_key(graph: &ScheduleGraph, root: SystemSetKey) -> Vec<SystemKey> {
    let mut out = Vec::new();
    let hg = graph.hierarchy().graph();
    let mut nested_sets: Vec<SystemSetKey> = Vec::new();
    for neighbor in hg.neighbors_directed(NodeId::Set(root), Direction::Outgoing) {
        match neighbor {
            NodeId::System(sys_key) => out.push(sys_key),
            NodeId::Set(child_set) => nested_sets.push(child_set),
        }
    }
    if nested_sets.is_empty() {
        return out;
    }
    let mut visited_sets: HashSet<SystemSetKey> = HashSet::default();
    visited_sets.insert(root);
    while let Some(cur) = nested_sets.pop() {
        if !visited_sets.insert(cur) {
            continue;
        }
        for neighbor in hg.neighbors_directed(NodeId::Set(cur), Direction::Outgoing) {
            match neighbor {
                NodeId::System(sys_key) => {
                    if !out.contains(&sys_key) {
                        out.push(sys_key);
                    }
                }
                NodeId::Set(child_set) => {
                    if !visited_sets.contains(&child_set) {
                        nested_sets.push(child_set);
                    }
                }
            }
        }
    }
    out
}

fn collect_condition_accesses(
    graph: &ScheduleGraph,
) -> Option<HashMap<SystemKey, Vec<&SystemAccess>>> {
    let mut accesses: HashMap<SystemKey, Vec<&SystemAccess>> = HashMap::default();
    for (key, _, conditions) in graph.systems.iter() {
        if !conditions.is_empty() {
            accesses
                .entry(key)
                .or_default()
                .extend(conditions.iter().map(|c| &c.access));
        }
    }
    for (set_key, _, conditions) in graph.system_sets.iter() {
        if conditions.is_empty() {
            continue;
        }
        for sys_key in collect_systems_in_set_key(graph, set_key) {
            accesses
                .entry(sys_key)
                .or_default()
                .extend(conditions.iter().map(|c| &c.access));
        }
    }
    (!accesses.is_empty()).then_some(accesses)
}

fn systems_conflict_for_weak_edge(
    graph: &ScheduleGraph,
    from: SystemKey,
    to: SystemKey,
    condition_accesses: Option<&HashMap<SystemKey, Vec<&SystemAccess>>>,
) -> bool {
    let (Some(from_sys), Some(to_sys)) = (graph.systems.get(from), graph.systems.get(to)) else {
        return false;
    };
    if from_sys
        .system()
        .flags()
        .contains(SystemStateFlags::DEFERRED)
        || from_sys.access().is_exclusive()
        || to_sys.access().is_exclusive()
        || !from_sys.access().is_compatible(to_sys.access())
    {
        return true;
    }
    let Some(cond_map) = condition_accesses else {
        return false;
    };
    let conditions_conflict = |system_access: &SystemAccess, other: SystemKey| {
        cond_map.get(&other).is_some_and(|accesses| {
            accesses
                .iter()
                .any(|access| !system_access.is_compatible(access))
        })
    };
    conditions_conflict(from_sys.access(), to) || conditions_conflict(to_sys.access(), from)
}

type SystemKeyIndexSet = <<bevy_ecs::schedule::graph::DagGroups<
    SystemSetKey,
    SystemKey,
> as core::ops::Deref>::Target as core::ops::Index<&'static SystemSetKey>>::Output;

impl ScheduleBuildPass for ModScheduleSetRewirePass {
    type EdgeOptions = ();

    fn add_dependency(&mut self, _from: NodeId, _to: NodeId, _options: Option<&Self::EdgeOptions>) {
    }

    fn collapse_set(
        &mut self,
        _set: SystemSetKey,
        _systems: &SystemKeyIndexSet,
        _dependency_flattening: &bevy_ecs::schedule::graph::DiGraph<NodeId>,
    ) -> impl Iterator<Item = (NodeId, NodeId)> {
        core::iter::empty()
    }

    fn build(
        &mut self,
        world: &mut World,
        graph: &mut ScheduleGraph,
        mut dependency_flattened: FlattenedDependencies<'_>,
    ) -> Result<(), ScheduleBuildError> {
        let Some(registry) = world.get_resource::<ModScheduleSetRegistry>() else {
            return Ok(());
        };
        let mod_host = world.get_resource::<ModHost>();

        let mut strict_edges: Vec<(SystemKey, SystemKey)> = Vec::new();
        let mut weak_edges: Vec<(SystemKey, SystemKey)> = Vec::new();

        for entry in registry.sets_for_schedule(self.schedule_id) {
            let own_systems: Vec<SystemKey> =
                resolve_set_key_in_graph(graph, &entry.named_set, entry.interned_set, mod_host)
                    .map(|k| collect_systems_in_set_key(graph, k))
                    .unwrap_or_default();

            if !own_systems.is_empty() {
                for &target_h in &entry.before_sets {
                    let Some(target) = registry.get(target_h) else {
                        continue;
                    };
                    let Some(target_key) = resolve_set_key_in_graph(
                        graph,
                        &target.named_set,
                        target.interned_set,
                        mod_host,
                    ) else {
                        continue;
                    };
                    let target_systems = collect_systems_in_set_key(graph, target_key);
                    let bucket = if entry.before_weak {
                        &mut weak_edges
                    } else {
                        &mut strict_edges
                    };
                    for &src in &own_systems {
                        for &dst in &target_systems {
                            if src != dst {
                                bucket.push((src, dst));
                            }
                        }
                    }
                }

                for &target_h in &entry.after_sets {
                    let Some(target) = registry.get(target_h) else {
                        continue;
                    };
                    let Some(target_key) = resolve_set_key_in_graph(
                        graph,
                        &target.named_set,
                        target.interned_set,
                        mod_host,
                    ) else {
                        continue;
                    };
                    let target_systems = collect_systems_in_set_key(graph, target_key);
                    let bucket = if entry.after_weak {
                        &mut weak_edges
                    } else {
                        &mut strict_edges
                    };
                    for &prev in &target_systems {
                        for &dst in &own_systems {
                            if prev != dst {
                                bucket.push((prev, dst));
                            }
                        }
                    }
                }
            }

            match entry.mode {
                ScheduleSetMode::Parallel => {}
                ScheduleSetMode::Sequence | ScheduleSetMode::SequenceWeak => {
                    let resolved_members: Vec<Vec<SystemKey>> = entry
                        .mounted_systems
                        .iter()
                        .zip(entry.mounted_interned.iter().copied())
                        .filter_map(|(member_set, member_interned)| {
                            let key = resolve_set_key_in_graph(
                                graph,
                                member_set,
                                member_interned,
                                mod_host,
                            )?;
                            let systems = collect_systems_in_set_key(graph, key);
                            (!systems.is_empty()).then_some(systems)
                        })
                        .collect();
                    let bucket = if entry.mode == ScheduleSetMode::SequenceWeak {
                        &mut weak_edges
                    } else {
                        &mut strict_edges
                    };
                    for pair in resolved_members.windows(2) {
                        for &src in &pair[0] {
                            for &dst in &pair[1] {
                                if src != dst {
                                    bucket.push((src, dst));
                                }
                            }
                        }
                    }
                }
            }
        }

        for (from, to) in strict_edges {
            dependency_flattened.add_edge(from, to);
        }

        if !weak_edges.is_empty() {
            let condition_accesses = collect_condition_accesses(graph);
            let all_directly_conflict = weak_edges.iter().all(|&(from, to)| {
                systems_conflict_for_weak_edge(graph, from, to, condition_accesses.as_ref())
            });
            if all_directly_conflict {
                // Fast path: every weak edge directly connects conflicting/deferred systems, so
                // materializing them directly avoids cloning or running an extra `Dag::analyze()`.
                for (from, to) in weak_edges {
                    dependency_flattened.add_edge(from, to);
                }
            } else {
                // Slow path (non-conflicting non-deferred weak pairs): compute reachability over
                // the combined DAG so transitive conflicts through non-conflicting intermediates
                // materialize while non-conflicting pairs remain unordered.
                let mut combined = (*dependency_flattened).clone();
                for &(from, to) in &weak_edges {
                    combined.graph_mut().add_edge(from, to);
                }
                let combined_analysis = combined
                    .analyze()
                    .map_err(ScheduleBuildError::FlatDependencySort)?;
                for (from, to) in combined_analysis.transitive_closure().all_edges() {
                    if !dependency_flattened.graph().contains_edge(from, to)
                        && systems_conflict_for_weak_edge(
                            graph,
                            from,
                            to,
                            condition_accesses.as_ref(),
                        )
                    {
                        dependency_flattened.add_edge(from, to);
                    }
                }
            }
        }

        Ok(())
    }
}

/// Idempotently installs [`ModScheduleSetRewirePass`] onto `schedule` ahead of
/// [`AutoInsertApplyDeferredPass`] so dynamic schedule-set edges participate in deferred sync-point
/// insertion, ambiguity checks, and `shuffle_seed` builds.
pub fn install_mod_schedule_set_pass(schedule: &mut Schedule, schedule_id: ScheduleId) {
    if schedule
        .graph()
        .system_sets
        .contains(ModSchedulePassMarker(schedule_id))
    {
        return;
    }
    let auto_sync = schedule.get_build_settings().auto_insert_apply_deferred;
    if auto_sync {
        schedule.remove_build_pass::<AutoInsertApplyDeferredPass>();
    }
    schedule.add_build_pass(ModScheduleSetRewirePass::new(schedule_id));
    if auto_sync {
        schedule.add_build_pass(AutoInsertApplyDeferredPass::default());
    }
    schedule.configure_sets(ModSchedulePassMarker(schedule_id));
}

/// Ensures [`ModScheduleSetRewirePass`] is installed on `schedule_id` inside `world`'s
/// [`Schedules`] resource when `Schedules` already exists in `world`.
pub(crate) fn ensure_mod_schedule_set_pass_if_schedules_present(
    world: &mut World,
    schedule_id: ScheduleId,
) {
    let Some(label) = resolve_schedule_id_in_world(world, schedule_id) else {
        return;
    };
    if let Some(mut schedules) = world.get_resource_mut::<Schedules>() {
        if schedules.get_temporarily_removed().contains(&label) {
            tracing::warn!(
                schedule = ?label,
                "skipping schedule-set pass installation because target schedule is currently leased"
            );
            return;
        }
        let schedule = schedules.entry(label);
        install_mod_schedule_set_pass(schedule, schedule_id);
    }
}

/// Configures a [`GuestSystem`] with its scoped [`ModNamedSystemSet`], `in_sets` container
/// memberships, strong/weak ordering edges, and ambiguity exclusions against `world`.
pub fn configure_guest_system_for_schedule(
    world: &World,
    mod_name: &str,
    generation: u64,
    sys: GuestSystem,
) -> ScheduleConfigs<ScheduleSystem> {
    let sys_name = sys.system_name().to_string();
    let own_set = ModNamedSystemSet::with_generation(mod_name, sys_name.as_str(), generation);
    let in_sets = sys.in_sets().to_vec();
    let after = sys.after_systems().to_vec();
    let before = sys.before_systems().to_vec();
    let after_weak = sys.after_weak_systems().to_vec();
    let before_weak = sys.before_weak_systems().to_vec();
    let ambiguous_with = sys.ambiguous_with_systems().to_vec();
    let ambiguous_with_all = sys.is_ambiguous_with_all();

    let resolve_target = |raw: &str| -> Option<InternedSystemSet> {
        if let Some(stripped) = raw.strip_prefix("host::") {
            return world
                .get_resource::<ModVisibleSets>()
                .and_then(|sets| sets.resolve(stripped));
        }
        let parsed = ModNamedSystemSet::parse_target(raw, mod_name)?;
        let target_gen = if parsed.mod_name.as_ref() == mod_name {
            generation
        } else {
            world
                .get_resource::<ModHost>()
                .map(|h| h.mod_generation(parsed.mod_name.as_ref()))
                .unwrap_or(0)
        };
        Some(parsed.at_generation(target_gen).intern())
    };

    let mut cfg = sys.in_set(own_set);
    for raw in in_sets {
        if let Some(target) = resolve_target(&raw) {
            cfg = IntoScheduleConfigs::in_set(cfg, target);
        }
    }
    for raw in after {
        if let Some(target) = resolve_target(&raw) {
            cfg = IntoScheduleConfigs::after(cfg, target);
        }
    }
    for raw in before {
        if let Some(target) = resolve_target(&raw) {
            cfg = IntoScheduleConfigs::before(cfg, target);
        }
    }
    for raw in after_weak {
        if let Some(target) = resolve_target(&raw) {
            cfg = IntoScheduleConfigs::after_weak(cfg, target);
        }
    }
    for raw in before_weak {
        if let Some(target) = resolve_target(&raw) {
            cfg = IntoScheduleConfigs::before_weak(cfg, target);
        }
    }
    for raw in ambiguous_with {
        if let Some(target) = resolve_target(&raw) {
            cfg = IntoScheduleConfigs::ambiguous_with(cfg, target);
        }
    }
    if ambiguous_with_all {
        cfg = IntoScheduleConfigs::ambiguous_with_all(cfg);
    }
    cfg
}

/// Mounts a batch of [`GuestSystem`]s into `world`'s [`Schedules`], installs
/// [`ModScheduleSetRewirePass`] on each targeted schedule, and initializes each modified schedule.
pub fn mount_mod_systems_in_world(
    world: &mut World,
    mod_name: &str,
    generation: u64,
    systems: Vec<GuestSystem>,
) -> Result<(), ScheduleBuildError> {
    world.init_resource::<Schedules>();
    let mut touched: Vec<(ScheduleId, InternedScheduleLabel)> = Vec::new();
    let mut recorded_sets: Vec<(ModId, InternedScheduleLabel, ModNamedSystemSet)> = Vec::new();
    for sys in systems {
        let sched_id = sys.schedule();
        let Some(label) = resolve_schedule_id_in_world(world, sched_id) else {
            continue;
        };
        let is_leased = world
            .get_resource::<Schedules>()
            .is_some_and(|s| s.get_temporarily_removed().contains(&label));
        if is_leased {
            tracing::error!(
                mod_name = %mod_name,
                schedule = ?label,
                system = %sys.system_name(),
                "skipping mounting guest system into currently leased schedule"
            );
            continue;
        }
        let sys_mod_id = sys.mod_id();
        let sys_name_arc = sys.system_name_arc();
        let cfg = configure_guest_system_for_schedule(world, mod_name, generation, sys);
        if let Some(mut schedules) = world.get_resource_mut::<Schedules>() {
            let sched = schedules.entry(label);
            install_mod_schedule_set_pass(sched, sched_id);
            sched.add_systems(cfg);
        }
        if let Some(mod_id) = sys_mod_id {
            recorded_sets.push((
                mod_id,
                label,
                ModNamedSystemSet::with_generation(mod_name, sys_name_arc, generation),
            ));
        }
        if !touched.iter().any(|&(id, _)| id == sched_id) {
            touched.push((sched_id, label));
        }
    }
    if !recorded_sets.is_empty() {
        let mut by_mod: HashMap<ModId, Vec<(InternedScheduleLabel, ModNamedSystemSet)>> =
            HashMap::default();
        for (mod_id, label, named_set) in recorded_sets {
            by_mod.entry(mod_id).or_default().push((label, named_set));
        }
        for (mod_id, sets) in by_mod {
            crate::loader::record_mod_named_sets_in_world(world, mod_id, sets);
        }
    }
    for (sched_id, label) in touched {
        let removed = world
            .get_resource_mut::<Schedules>()
            .and_then(|mut s| s.remove_temporarily(label));
        if let Some(mut schedule) = removed {
            install_mod_schedule_set_pass(&mut schedule, sched_id);
            let res =
                match try_sync_mod_schedule_set_configs_to_schedule(world, sched_id, &mut schedule)
                {
                    Ok(()) => schedule.initialize(world),
                    Err(err) => Err(err),
                };
            if let Some(mut schedules) = world.get_resource_mut::<Schedules>() {
                schedules.reinsert(schedule);
            }
            res?;
        }
    }
    Ok(())
}

/// Fast host-side schedule-only hot-rewire on a standalone [`Schedule`]: marks only the schedule
/// graph dirty and rebuilds `SystemSchedule` from [`ModScheduleSetRegistry`] without removing or
/// re-initializing any existing [`GuestSystem`] kernels.
pub fn rewire_mod_schedule_sets_in_schedule(
    world: &mut World,
    schedule_id: ScheduleId,
    schedule: &mut Schedule,
) -> Result<Option<ScheduleBuildMetadata>, ScheduleBuildError> {
    try_sync_mod_schedule_set_configs_to_schedule(world, schedule_id, schedule)?;
    install_mod_schedule_set_pass(schedule, schedule_id);
    // Touching an empty sentinel set flips `ScheduleGraph::changed = true` in O(1) while leaving
    // `ScheduleGraph::systems.uninit` empty so zero `GuestSystem::initialize` calls occur.
    schedule.configure_sets(ModScheduleRewireMarker(schedule_id));
    schedule.initialize(world)
}

/// Fast host-side schedule-only hot-rewire on `schedule_id` inside `world`'s [`Schedules`]
/// resource: rebuilds the executable schedule graph from [`ModScheduleSetRegistry`] without
/// re-registering or re-initializing any [`GuestSystem`] kernels.
pub fn rewire_mod_schedule_sets_in_world(
    world: &mut World,
    schedule_id: ScheduleId,
) -> Result<Option<ScheduleBuildMetadata>, ScheduleBuildError> {
    let Some(label) = resolve_schedule_id_in_world(world, schedule_id) else {
        return Ok(None);
    };
    world.init_resource::<Schedules>();
    if world
        .get_resource::<Schedules>()
        .is_some_and(|s| s.get_temporarily_removed().contains(&label))
    {
        tracing::warn!(
            schedule = ?label,
            "skipping rewire_mod_schedule_sets_in_world because target schedule is currently leased"
        );
        return Ok(None);
    }
    let mut schedule = world
        .get_resource_mut::<Schedules>()
        .and_then(|mut s| s.remove_temporarily(label))
        .unwrap_or_else(|| Schedule::new(label));
    let res = rewire_mod_schedule_sets_in_schedule(world, schedule_id, &mut schedule);
    if let Some(mut schedules) = world.get_resource_mut::<Schedules>() {
        schedules.reinsert(schedule);
    }
    res
}

#[cfg(test)]
#[path = "../tests/unit/schedule_coverage_tests.rs"]
mod coverage_tests;
