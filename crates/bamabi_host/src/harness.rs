//! Ergonomic headless Bevy [`App`] test harness (`HeadlessModTestHarness`) for writing zero-window
//! integration tests over native C-ABI mods, POD state hashing, table-column inspection,
//! telemetry digests, and in-memory entrypoints.

#[cfg(feature = "dynamic-loader")]
use crate::app_ext::ModLoaderAppExt;
use crate::app_ext::ModRegistryAppExt;
use crate::budgets::ModBudgetPolicy;
use crate::events::{ModLoadFailed, ModLoaded, ModSystemFaulted, ModUnloaded};
use crate::plugin::AbiModPlugin;
use crate::telemetry::{ModTelemetryRing, ModTelemetrySinkPlugin};
#[cfg(feature = "signature-verification")]
use bamabi_host_sys::ModTrustPolicy;
use bamabi_host_sys::{
    InMemoryModEntrypoints, LoadOptions, LoadedMod, ModErrorPolicy, ModHost, ModId, ModLoadError,
    hash_world_pod_state, read_table_column_bytes, spawn_entity_pod_bundle,
};
use bamabi_sys::SystemTelemetryDigest64;
use bamabi_sys::{BamComponent, FfiResult};
use bevy_app::App;
use bevy_ecs::entity::Entity;
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::world::World;
use std::path::Path;

/// Headless Bevy [`App`] test driver preconfigured with [`AbiModPlugin`] and ergonomic helpers for
/// stepping frames, spawning/inspecting POD state, collecting lifecycle messages, and testing
/// both disk and in-memory mods.
pub struct HeadlessModTestHarness {
    app: App,
}

impl Default for HeadlessModTestHarness {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl HeadlessModTestHarness {
    /// Creates a fresh headless [`App`] initialized with [`AbiModPlugin`].
    pub fn new() -> Self {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        Self { app }
    }

    /// Creates a harness initialized with [`AbiModPlugin::with_error_policy`].
    pub fn with_error_policy(policy: ModErrorPolicy) -> Self {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::with_error_policy(policy));
        Self { app }
    }

    /// Creates a harness initialized with [`AbiModPlugin::with_budget_policy`].
    pub fn with_budget_policy(policy: ModBudgetPolicy) -> Self {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::with_budget_policy(policy));
        Self { app }
    }

    /// Creates a harness initialized with [`AbiModPlugin::with_trust_policy`].
    #[cfg(feature = "signature-verification")]
    pub fn with_trust_policy(policy: ModTrustPolicy) -> Self {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::with_trust_policy(policy));
        Self { app }
    }

    /// Enables [`ModTelemetrySinkPlugin`] on this harness using default capacity.
    #[must_use]
    pub fn with_telemetry(mut self) -> Self {
        self.app.add_plugins(ModTelemetrySinkPlugin::default());
        self
    }

    /// Enables [`ModTelemetrySinkPlugin`] on this harness with an explicit ring capacity.
    #[must_use]
    pub fn with_telemetry_capacity(mut self, capacity: usize) -> Self {
        self.app
            .add_plugins(ModTelemetrySinkPlugin::new().with_capacity(capacity));
        self
    }

    /// Borrows the underlying Bevy [`App`].
    #[inline]
    pub const fn app(&self) -> &App {
        &self.app
    }

    /// Mutably borrows the underlying Bevy [`App`].
    #[inline]
    pub const fn app_mut(&mut self) -> &mut App {
        &mut self.app
    }

    /// Borrows the underlying ECS [`World`].
    #[inline]
    pub fn world(&self) -> &World {
        self.app.world()
    }

    /// Mutably borrows the underlying ECS [`World`].
    #[inline]
    pub fn world_mut(&mut self) -> &mut World {
        self.app.world_mut()
    }

    /// Advances the headless [`App`] by one update tick.
    #[inline]
    pub fn step(&mut self) {
        self.app.update();
    }

    /// Advances the headless [`App`] by `frames` update ticks.
    #[inline]
    pub fn step_n(&mut self, frames: usize) {
        for _ in 0..frames {
            self.app.update();
        }
    }

    /// Computes a deterministic 64-bit FNV-1a checksum over all live POD components and resources.
    #[inline]
    pub fn pod_state_hash(&self) -> u64 {
        hash_world_pod_state(self.app.world())
    }

    /// Captures sorted `(Entity, Vec<u8>)` raw POD payloads across all tables for `component`.
    #[inline]
    pub fn read_table_column(
        &self,
        component: BamComponent,
    ) -> Result<Vec<(Entity, Vec<u8>)>, FfiResult> {
        read_table_column_bytes(self.app.world(), component)
    }

    /// Validates and spawns an entity carrying all POD `(BamComponent, &[u8])` entries atomically.
    #[inline]
    pub fn spawn_pod_bundle(
        &mut self,
        components: &[(BamComponent, &[u8])],
    ) -> Result<Entity, FfiResult> {
        spawn_entity_pod_bundle(self.app.world_mut(), components)
    }

    /// Returns the slice of currently active [`LoadedMod`] descriptors.
    pub fn loaded_mods(&self) -> &[LoadedMod] {
        self.app
            .world()
            .get_resource::<ModHost>()
            .map_or(&[], ModHost::loaded_mods)
    }

    /// Loads an in-memory mod directly from [`InMemoryModEntrypoints`] (`dlopen`-free).
    pub fn load_mod_from_entrypoints<P: AsRef<Path>>(
        &mut self,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
    ) -> Result<ModId, ModLoadError> {
        self.app
            .load_mod_from_entrypoints(virtual_path, entrypoints)
    }

    /// Loads an in-memory mod directly from [`InMemoryModEntrypoints`] with explicit [`LoadOptions`].
    pub fn load_mod_from_entrypoints_with<P: AsRef<Path>>(
        &mut self,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        self.app
            .load_mod_from_entrypoints_with(virtual_path, entrypoints, options)
    }

    /// Atomically hot-reloads `id` from replacement [`InMemoryModEntrypoints`].
    pub fn reload_mod_from_entrypoints<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        self.app
            .reload_mod_from_entrypoints(id, virtual_path, entrypoints, options)
    }

    /// Loads a native shared library from `path` with [`LoadOptions::default()`].
    #[cfg(feature = "dynamic-loader")]
    pub fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        self.app.load_mod(path)
    }

    /// Loads a native shared library from `path` with explicit [`LoadOptions`].
    #[cfg(feature = "dynamic-loader")]
    pub fn load_mod_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        self.app.load_mod_with(path, options)
    }

    /// Hot-reloads `id` from disk using default shadow-copying options.
    #[cfg(feature = "dynamic-loader")]
    pub fn reload_mod(&mut self, id: ModId) -> Result<ModId, ModLoadError> {
        self.app.reload_mod(id)
    }

    /// Hot-reloads `id` from disk with explicit [`LoadOptions`].
    #[cfg(feature = "dynamic-loader")]
    pub fn reload_mod_with(
        &mut self,
        id: ModId,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        self.app.reload_mod_with(id, options)
    }

    /// Unloads a loaded mod by [`ModId`].
    pub fn unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError> {
        self.app.unload_mod(id)
    }

    /// Drains all buffered Bevy [`Messages<M>`] items currently queued in the harness world.
    pub fn drain_messages<M: Message + Clone>(&mut self) -> Vec<M> {
        self.app
            .world_mut()
            .get_resource_mut::<Messages<M>>()
            .map(|mut msgs| msgs.drain().collect())
            .unwrap_or_default()
    }

    /// Drains all queued [`ModLoaded`] notifications.
    #[inline]
    pub fn drain_loaded_events(&mut self) -> Vec<ModLoaded> {
        self.drain_messages()
    }

    /// Drains all queued [`ModUnloaded`] notifications.
    #[inline]
    pub fn drain_unloaded_events(&mut self) -> Vec<ModUnloaded> {
        self.drain_messages()
    }

    /// Drains all queued [`ModLoadFailed`] notifications.
    #[inline]
    pub fn drain_load_failed_events(&mut self) -> Vec<ModLoadFailed> {
        self.drain_messages()
    }

    /// Drains all queued [`ModSystemFaulted`] notifications.
    #[inline]
    pub fn drain_fault_events(&mut self) -> Vec<ModSystemFaulted> {
        self.drain_messages()
    }

    /// Returns a snapshot of buffered [`SystemTelemetryDigest64`] frames from [`ModTelemetryRing`].
    pub fn telemetry_digests(&self) -> Vec<SystemTelemetryDigest64> {
        self.app
            .world()
            .get_resource::<ModTelemetryRing>()
            .map_or_else(Vec::new, ModTelemetryRing::snapshot)
    }
}
