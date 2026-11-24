//! [`ModRegistryAppExt`], [`ModLoaderAppExt`], and unified [`ModAppExt`] extension traits on
//! Bevy [`App`], delegating to [`ModWorldExt`].

#[cfg(feature = "dynamic-loader")]
use crate::mod_dag::{DirectoryLoadReport, ModDagError};
use crate::plugin::ensure_startup_tracker;
use crate::world_ext::ModWorldExt;
#[cfg(feature = "signature-verification")]
use bamabi_host_sys::ModTrustPolicy;
use bamabi_host_sys::{
    ComponentCollisionError, ComponentOptions, InMemoryModEntrypoints, InMemoryObjectLoader,
    LoadOptions, ModErrorPolicy, ModId, ModLoadError,
};
use bamabi_sys::{AbiPod, BamComponent, ModComponent, ScheduleId};
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::event::{EntityEvent, Event};
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{ScheduleLabel, SystemSet};
use std::path::Path;

/// Static-only (`--no-default-features`), component/resource/message/event/schedule registration,
/// in-memory entrypoint loading, and mod-unload extension methods on Bevy [`App`].
///
/// Always available regardless of whether `dynamic-loader` (`libloading` / `dlopen`) is enabled.
///
/// ```compile_fail
/// use bamabi_host::{ModHost, ModId};
/// use bevy_ecs::system::ResMut;
///
/// fn bad_parallel_unload_system(mut host: ResMut<ModHost>) {
///     // `ModHost` does not expose `unload_mod` to `ResMut<ModHost>` systems.
///     let _ = host.unload_mod(ModId(1));
/// }
/// ```
pub trait ModRegistryAppExt {
    /// Registers a Bevy [`Component`] implementing [`ModComponent`] with [`ComponentOptions`],
    /// returning its 1-based [`BamComponent`] handle.
    fn register_mod_component<T: Component + ModComponent>(
        &mut self,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a singleton Bevy [`Resource`] implementing [`ModComponent`], returning its
    /// 1-based [`BamComponent`] handle.
    fn register_mod_resource<R: Resource + ModComponent>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a buffered POD Bevy [`Message`] type (`Messages<M>`), adding `Messages<M>` frame
    /// maintenance to this [`App`] and returning its 1-based [`BamComponent`] handle.
    fn register_mod_message<M: Message + ModComponent + AbiPod>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a VTable-backed non-POD Bevy [`Message`] type (`Messages<M>`, e.g. containing
    /// `String` or `Vec<T>` fields), adding `Messages<M>` frame maintenance to this [`App`] and
    /// returning its 1-based [`BamComponent`] handle.
    fn register_mod_non_pod_message<M: Message + ModComponent + Default>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a triggerable host Bevy [`Event`] implementing [`ModComponent`] + [`Copy`],
    /// returning its 1-based [`BamComponent`] handle.
    fn register_mod_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: Event<Trigger<'static>: Default> + ModComponent + Copy;

    /// Registers a triggerable entity-targeted Bevy [`EntityEvent`] implementing [`ModComponent`]
    /// + [`Copy`], returning its 1-based [`BamComponent`] handle.
    fn register_mod_entity_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + ModComponent + Copy,
        for<'a> E::Trigger<'a>: Default;

    /// Registers a non-component [`ModComponent`] helper/nested struct type so its static VTable
    /// and field accessors are resolvable via `get_proc_address`.
    fn register_mod_type<T: ModComponent>(&mut self) -> &mut Self;

    /// Maps a game-defined custom [`ScheduleId`] (`>= 1024`) to a Bevy [`ScheduleLabel`].
    fn register_mod_schedule<S: ScheduleLabel>(
        &mut self,
        id: ScheduleId,
        label: S,
    ) -> Result<(), ModLoadError>;

    /// Exposes a host [`SystemSet`] under `"host::<name>"` in [`crate::ModVisibleSets`] so guest
    /// systems can declare `.after(c"host::<name>")` or `.before(c"host::<name>")`.
    fn register_mod_visible_set<S: SystemSet>(&mut self, name: &str, set: S) -> &mut Self;

    /// Sets the default [`ModErrorPolicy`] for guest systems in this [`App`].
    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self;

    /// Configures the [`ModTrustPolicy`] enforced when loading or reloading native mods in this
    /// [`App`].
    #[cfg(feature = "signature-verification")]
    fn set_mod_trust_policy(&mut self, policy: ModTrustPolicy) -> &mut Self;

    /// Re-evaluates all currently loaded mods against the active [`ModTrustPolicy`] and unloads
    /// any mod whose verification receipt is no longer trusted, returning the evicted [`ModId`]s.
    #[cfg(feature = "signature-verification")]
    fn evict_revoked_mods(&mut self) -> Vec<ModId>;

    /// Unloads a loaded mod by [`ModId`] under exclusive [`App`] access.
    fn unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError>;

    /// Unloads all currently loaded mods in LIFO order.
    fn unload_all_mods(&mut self) -> &mut Self;

    /// Loads a static or JIT-compiled mod directly from [`InMemoryModEntrypoints`] (`dlopen`-free).
    fn load_mod_from_entrypoints<P: AsRef<Path>>(
        &mut self,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
    ) -> Result<ModId, ModLoadError>;

    /// Loads a static or JIT-compiled mod directly from [`InMemoryModEntrypoints`] with explicit
    /// [`LoadOptions`] (`dlopen`-free).
    fn load_mod_from_entrypoints_with<P: AsRef<Path>>(
        &mut self,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Atomically hot-reloads `id` from replacement [`InMemoryModEntrypoints`].
    fn reload_mod_from_entrypoints<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Materializes entry points from raw compiled object/bytecode bytes via an
    /// [`InMemoryObjectLoader`] and loads the resulting mod (`dlopen`-free).
    fn load_mod_from_object_loader<L: InMemoryObjectLoader + ?Sized>(
        &mut self,
        loader: &L,
        label: &str,
        object_bytes: &[u8],
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Atomically hot-reloads `id` from raw compiled object/bytecode bytes via an
    /// [`InMemoryObjectLoader`].
    fn reload_mod_from_object_loader<L: InMemoryObjectLoader + ?Sized>(
        &mut self,
        id: ModId,
        loader: &L,
        label: &str,
        object_bytes: &[u8],
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;
}

/// Filesystem dynamic-library (`dlopen`) and `.bampack` extension methods on Bevy [`App`]
/// (`feature = "dynamic-loader"`).
#[cfg(feature = "dynamic-loader")]
pub trait ModLoaderAppExt {
    /// Loads a native shared-library mod using [`LoadOptions::default()`], returning its [`ModId`].
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError>;

    /// Loads a native shared-library mod with explicit [`LoadOptions`], returning its [`ModId`].
    fn load_mod_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Hot-reloads `id` from its original source path with shadow-copying enabled, transferring
    /// optional `save_state` / `restore_state` bytes.
    fn reload_mod(&mut self, id: ModId) -> Result<ModId, ModLoadError>;

    /// Hot-reloads `id` from its original source path with explicit [`LoadOptions`], transferring
    /// optional `save_state` / `restore_state` bytes.
    fn reload_mod_with(&mut self, id: ModId, options: LoadOptions) -> Result<ModId, ModLoadError>;

    /// Hot-reloads `id` from a replacement shared-library `path` with explicit [`LoadOptions`],
    /// transferring optional `save_state` / `restore_state` bytes.
    fn reload_mod_from<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Discovers `mod.toml` manifests inside `dir`, validates SemVer constraints, quarantines
    /// cyclic dependency subgraphs, and loads all valid acyclic mods in Kahn topological order
    /// using [`LoadOptions::default()`].
    fn load_mods_dir<P: AsRef<Path>>(&mut self, dir: P)
    -> Result<DirectoryLoadReport, ModDagError>;

    /// Discovers `mod.toml` manifests inside `dir`, validates SemVer constraints, quarantines
    /// cyclic dependency subgraphs, and loads all valid acyclic mods in Kahn topological order
    /// using `options`.
    fn load_mods_dir_with<P: AsRef<Path>>(
        &mut self,
        dir: P,
        options: LoadOptions,
    ) -> Result<DirectoryLoadReport, ModDagError>;

    /// Verifies (when `ModTrustPolicy` is enforced) and loads a `.bampack` archive mod using
    /// [`LoadOptions::default()`].
    #[cfg(feature = "signature-verification")]
    fn load_bampack<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError>;

    /// Verifies (when `ModTrustPolicy` is enforced) and loads a `.bampack` archive mod with
    /// explicit [`LoadOptions`].
    #[cfg(feature = "signature-verification")]
    fn load_bampack_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Atomically hot-reloads `id` from a replacement `.bampack` archive path.
    #[cfg(feature = "signature-verification")]
    fn reload_bampack<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;
}

/// Unified backwards-compatible convenience trait combining [`ModRegistryAppExt`] and (when
/// `dynamic-loader` is enabled) [`ModLoaderAppExt`] directly under a single trait import.
pub trait ModAppExt {
    /// Registers a Bevy [`Component`] implementing [`ModComponent`] with [`ComponentOptions`],
    /// returning its 1-based [`BamComponent`] handle.
    fn register_mod_component<T: Component + ModComponent>(
        &mut self,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a singleton Bevy [`Resource`] implementing [`ModComponent`], returning its
    /// 1-based [`BamComponent`] handle.
    fn register_mod_resource<R: Resource + ModComponent>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a buffered POD Bevy [`Message`] type (`Messages<M>`), adding `Messages<M>` frame
    /// maintenance to this [`App`] and returning its 1-based [`BamComponent`] handle.
    fn register_mod_message<M: Message + ModComponent + AbiPod>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a VTable-backed non-POD Bevy [`Message`] type (`Messages<M>`), adding
    /// `Messages<M>` frame maintenance to this [`App`] and returning its 1-based [`BamComponent`] handle.
    fn register_mod_non_pod_message<M: Message + ModComponent + Default>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError>;

    /// Registers a triggerable host Bevy [`Event`] implementing [`ModComponent`] + [`Copy`],
    /// returning its 1-based [`BamComponent`] handle.
    fn register_mod_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: Event<Trigger<'static>: Default> + ModComponent + Copy;

    /// Registers a triggerable entity-targeted Bevy [`EntityEvent`] implementing [`ModComponent`]
    /// + [`Copy`], returning its 1-based [`BamComponent`] handle.
    fn register_mod_entity_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + ModComponent + Copy,
        for<'a> E::Trigger<'a>: Default;

    /// Registers a non-component [`ModComponent`] helper/nested struct type.
    fn register_mod_type<T: ModComponent>(&mut self) -> &mut Self;

    /// Maps a game-defined custom [`ScheduleId`] (`>= 1024`) to a Bevy [`ScheduleLabel`].
    fn register_mod_schedule<S: ScheduleLabel>(
        &mut self,
        id: ScheduleId,
        label: S,
    ) -> Result<(), ModLoadError>;

    /// Exposes a host [`SystemSet`] under `"host::<name>"` in [`crate::ModVisibleSets`].
    fn register_mod_visible_set<S: SystemSet>(&mut self, name: &str, set: S) -> &mut Self;

    /// Sets the default [`ModErrorPolicy`] for guest systems in this [`App`].
    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self;

    /// Configures the [`ModTrustPolicy`] enforced when loading or reloading native mods.
    #[cfg(feature = "signature-verification")]
    fn set_mod_trust_policy(&mut self, policy: ModTrustPolicy) -> &mut Self;

    /// Re-evaluates loaded mods against the active [`ModTrustPolicy`] and unloads revoked ones.
    #[cfg(feature = "signature-verification")]
    fn evict_revoked_mods(&mut self) -> Vec<ModId>;

    /// Unloads a loaded mod by [`ModId`] under exclusive [`App`] access.
    fn unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError>;

    /// Unloads all currently loaded mods in LIFO order.
    fn unload_all_mods(&mut self) -> &mut Self;

    /// Loads a static or JIT-compiled mod directly from [`InMemoryModEntrypoints`] (`dlopen`-free).
    fn load_mod_from_entrypoints<P: AsRef<Path>>(
        &mut self,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
    ) -> Result<ModId, ModLoadError>;

    /// Loads a static or JIT-compiled mod directly from [`InMemoryModEntrypoints`] with explicit
    /// [`LoadOptions`] (`dlopen`-free).
    fn load_mod_from_entrypoints_with<P: AsRef<Path>>(
        &mut self,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Atomically hot-reloads `id` from replacement [`InMemoryModEntrypoints`].
    fn reload_mod_from_entrypoints<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Materializes entry points via an [`InMemoryObjectLoader`] and loads the mod (`dlopen`-free).
    fn load_mod_from_object_loader<L: InMemoryObjectLoader + ?Sized>(
        &mut self,
        loader: &L,
        label: &str,
        object_bytes: &[u8],
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Atomically hot-reloads `id` from object/bytecode bytes via an [`InMemoryObjectLoader`].
    fn reload_mod_from_object_loader<L: InMemoryObjectLoader + ?Sized>(
        &mut self,
        id: ModId,
        loader: &L,
        label: &str,
        object_bytes: &[u8],
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Loads a native shared-library mod using [`LoadOptions::default()`], returning its [`ModId`].
    #[cfg(feature = "dynamic-loader")]
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError>;

    /// Loads a native shared-library mod with explicit [`LoadOptions`], returning its [`ModId`].
    #[cfg(feature = "dynamic-loader")]
    fn load_mod_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Hot-reloads `id` from its original source path with shadow-copying enabled.
    #[cfg(feature = "dynamic-loader")]
    fn reload_mod(&mut self, id: ModId) -> Result<ModId, ModLoadError>;

    /// Hot-reloads `id` from its original source path with explicit [`LoadOptions`].
    #[cfg(feature = "dynamic-loader")]
    fn reload_mod_with(&mut self, id: ModId, options: LoadOptions) -> Result<ModId, ModLoadError>;

    /// Hot-reloads `id` from a replacement shared-library `path` with explicit [`LoadOptions`].
    #[cfg(feature = "dynamic-loader")]
    fn reload_mod_from<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Discovers `mod.toml` manifests inside `dir` and loads all valid acyclic mods.
    #[cfg(feature = "dynamic-loader")]
    fn load_mods_dir<P: AsRef<Path>>(&mut self, dir: P)
    -> Result<DirectoryLoadReport, ModDagError>;

    /// Discovers `mod.toml` manifests inside `dir` and loads all valid acyclic mods using `options`.
    #[cfg(feature = "dynamic-loader")]
    fn load_mods_dir_with<P: AsRef<Path>>(
        &mut self,
        dir: P,
        options: LoadOptions,
    ) -> Result<DirectoryLoadReport, ModDagError>;

    /// Verifies (when `ModTrustPolicy` is enforced) and loads a `.bampack` archive mod.
    #[cfg(all(feature = "dynamic-loader", feature = "signature-verification"))]
    fn load_bampack<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError>;

    /// Verifies (when `ModTrustPolicy` is enforced) and loads a `.bampack` archive mod with `options`.
    #[cfg(all(feature = "dynamic-loader", feature = "signature-verification"))]
    fn load_bampack_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;

    /// Atomically hot-reloads `id` from a replacement `.bampack` archive path.
    #[cfg(all(feature = "dynamic-loader", feature = "signature-verification"))]
    fn reload_bampack<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError>;
}

impl ModAppExt for App {
    #[inline]
    fn register_mod_component<T: Component + ModComponent>(
        &mut self,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        <Self as ModRegistryAppExt>::register_mod_component::<T>(self, options)
    }

    #[inline]
    fn register_mod_resource<R: Resource + ModComponent>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        <Self as ModRegistryAppExt>::register_mod_resource::<R>(self)
    }

    #[inline]
    fn register_mod_message<M: Message + ModComponent + AbiPod>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        <Self as ModRegistryAppExt>::register_mod_message::<M>(self)
    }

    #[inline]
    fn register_mod_non_pod_message<M: Message + ModComponent + Default>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        <Self as ModRegistryAppExt>::register_mod_non_pod_message::<M>(self)
    }

    #[inline]
    fn register_mod_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: Event<Trigger<'static>: Default> + ModComponent + Copy,
    {
        <Self as ModRegistryAppExt>::register_mod_event::<E>(self)
    }

    #[inline]
    fn register_mod_entity_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + ModComponent + Copy,
        for<'a> E::Trigger<'a>: Default,
    {
        <Self as ModRegistryAppExt>::register_mod_entity_event::<E>(self)
    }

    #[inline]
    fn register_mod_type<T: ModComponent>(&mut self) -> &mut Self {
        <Self as ModRegistryAppExt>::register_mod_type::<T>(self)
    }

    #[inline]
    fn register_mod_schedule<S: ScheduleLabel>(
        &mut self,
        id: ScheduleId,
        label: S,
    ) -> Result<(), ModLoadError> {
        <Self as ModRegistryAppExt>::register_mod_schedule(self, id, label)
    }

    #[inline]
    fn register_mod_visible_set<S: SystemSet>(&mut self, name: &str, set: S) -> &mut Self {
        <Self as ModRegistryAppExt>::register_mod_visible_set(self, name, set)
    }

    #[inline]
    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self {
        <Self as ModRegistryAppExt>::set_mod_error_policy(self, policy)
    }

    #[cfg(feature = "signature-verification")]
    #[inline]
    fn set_mod_trust_policy(&mut self, policy: ModTrustPolicy) -> &mut Self {
        <Self as ModRegistryAppExt>::set_mod_trust_policy(self, policy)
    }

    #[cfg(feature = "signature-verification")]
    #[inline]
    fn evict_revoked_mods(&mut self) -> Vec<ModId> {
        <Self as ModRegistryAppExt>::evict_revoked_mods(self)
    }

    #[inline]
    fn unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError> {
        <Self as ModRegistryAppExt>::unload_mod(self, id)
    }

    #[inline]
    fn unload_all_mods(&mut self) -> &mut Self {
        <Self as ModRegistryAppExt>::unload_all_mods(self)
    }

    #[inline]
    fn load_mod_from_entrypoints<P: AsRef<Path>>(
        &mut self,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
    ) -> Result<ModId, ModLoadError> {
        <Self as ModRegistryAppExt>::load_mod_from_entrypoints(self, virtual_path, entrypoints)
    }

    #[inline]
    fn load_mod_from_entrypoints_with<P: AsRef<Path>>(
        &mut self,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        <Self as ModRegistryAppExt>::load_mod_from_entrypoints_with(
            self,
            virtual_path,
            entrypoints,
            options,
        )
    }

    #[inline]
    fn reload_mod_from_entrypoints<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        <Self as ModRegistryAppExt>::reload_mod_from_entrypoints(
            self,
            id,
            virtual_path,
            entrypoints,
            options,
        )
    }

    #[inline]
    fn load_mod_from_object_loader<L: InMemoryObjectLoader + ?Sized>(
        &mut self,
        loader: &L,
        label: &str,
        object_bytes: &[u8],
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        <Self as ModRegistryAppExt>::load_mod_from_object_loader(
            self,
            loader,
            label,
            object_bytes,
            options,
        )
    }

    #[inline]
    fn reload_mod_from_object_loader<L: InMemoryObjectLoader + ?Sized>(
        &mut self,
        id: ModId,
        loader: &L,
        label: &str,
        object_bytes: &[u8],
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        <Self as ModRegistryAppExt>::reload_mod_from_object_loader(
            self,
            id,
            loader,
            label,
            object_bytes,
            options,
        )
    }

    #[cfg(feature = "dynamic-loader")]
    #[inline]
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        <Self as ModLoaderAppExt>::load_mod(self, path)
    }

    #[cfg(feature = "dynamic-loader")]
    #[inline]
    fn load_mod_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        <Self as ModLoaderAppExt>::load_mod_with(self, path, options)
    }

    #[cfg(feature = "dynamic-loader")]
    #[inline]
    fn reload_mod(&mut self, id: ModId) -> Result<ModId, ModLoadError> {
        <Self as ModLoaderAppExt>::reload_mod(self, id)
    }

    #[cfg(feature = "dynamic-loader")]
    #[inline]
    fn reload_mod_with(&mut self, id: ModId, options: LoadOptions) -> Result<ModId, ModLoadError> {
        <Self as ModLoaderAppExt>::reload_mod_with(self, id, options)
    }

    #[cfg(feature = "dynamic-loader")]
    #[inline]
    fn reload_mod_from<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        <Self as ModLoaderAppExt>::reload_mod_from(self, id, path, options)
    }

    #[cfg(feature = "dynamic-loader")]
    #[inline]
    fn load_mods_dir<P: AsRef<Path>>(
        &mut self,
        dir: P,
    ) -> Result<DirectoryLoadReport, ModDagError> {
        <Self as ModLoaderAppExt>::load_mods_dir(self, dir)
    }

    #[cfg(feature = "dynamic-loader")]
    #[inline]
    fn load_mods_dir_with<P: AsRef<Path>>(
        &mut self,
        dir: P,
        options: LoadOptions,
    ) -> Result<DirectoryLoadReport, ModDagError> {
        <Self as ModLoaderAppExt>::load_mods_dir_with(self, dir, options)
    }

    #[cfg(all(feature = "dynamic-loader", feature = "signature-verification"))]
    #[inline]
    fn load_bampack<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        <Self as ModLoaderAppExt>::load_bampack(self, path)
    }

    #[cfg(all(feature = "dynamic-loader", feature = "signature-verification"))]
    #[inline]
    fn load_bampack_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        <Self as ModLoaderAppExt>::load_bampack_with(self, path, options)
    }

    #[cfg(all(feature = "dynamic-loader", feature = "signature-verification"))]
    #[inline]
    fn reload_bampack<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        <Self as ModLoaderAppExt>::reload_bampack(self, id, path, options)
    }
}

impl ModRegistryAppExt for App {
    #[inline]
    fn register_mod_component<T: Component + ModComponent>(
        &mut self,
        options: ComponentOptions<T>,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.world_mut().register_mod_component::<T>(options)
    }

    #[inline]
    fn register_mod_resource<R: Resource + ModComponent>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        self.world_mut().register_mod_resource::<R>()
    }

    #[inline]
    fn register_mod_message<M: Message + ModComponent + AbiPod>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        if !self.world().contains_resource::<Messages<M>>() {
            self.add_message::<M>();
        }
        self.world_mut().register_mod_message::<M>()
    }

    fn register_mod_non_pod_message<M: Message + ModComponent + Default>(
        &mut self,
    ) -> Result<BamComponent, ComponentCollisionError> {
        if !self.world().contains_resource::<Messages<M>>() {
            self.add_message::<M>();
        }
        self.world_mut().register_mod_non_pod_message::<M>()
    }

    #[inline]
    fn register_mod_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: Event<Trigger<'static>: Default> + ModComponent + Copy,
    {
        self.world_mut().register_mod_event::<E>()
    }

    #[inline]
    fn register_mod_entity_event<E>(&mut self) -> Result<BamComponent, ComponentCollisionError>
    where
        E: EntityEvent + Event + ModComponent + Copy,
        for<'a> E::Trigger<'a>: Default,
    {
        self.world_mut().register_mod_entity_event::<E>()
    }

    #[inline]
    fn register_mod_type<T: ModComponent>(&mut self) -> &mut Self {
        self.world_mut().register_mod_type::<T>();
        self
    }

    #[inline]
    fn register_mod_schedule<S: ScheduleLabel>(
        &mut self,
        id: ScheduleId,
        label: S,
    ) -> Result<(), ModLoadError> {
        self.world_mut().register_mod_schedule(id, label)
    }

    #[inline]
    fn register_mod_visible_set<S: SystemSet>(&mut self, name: &str, set: S) -> &mut Self {
        let _ = self.world_mut().register_mod_visible_set(name, set);
        self
    }

    #[inline]
    fn set_mod_error_policy(&mut self, policy: ModErrorPolicy) -> &mut Self {
        self.world_mut().set_mod_error_policy(policy);
        self
    }

    #[cfg(feature = "signature-verification")]
    #[inline]
    fn set_mod_trust_policy(&mut self, policy: ModTrustPolicy) -> &mut Self {
        self.world_mut().set_mod_trust_policy(policy);
        self
    }

    #[cfg(feature = "signature-verification")]
    #[inline]
    fn evict_revoked_mods(&mut self) -> Vec<ModId> {
        self.world_mut().evict_revoked_mods()
    }

    #[inline]
    fn unload_mod(&mut self, id: ModId) -> Result<(), ModLoadError> {
        self.world_mut().unload_mod(id)
    }

    #[inline]
    fn unload_all_mods(&mut self) -> &mut Self {
        self.world_mut().unload_all_mods();
        self
    }

    #[inline]
    fn load_mod_from_entrypoints<P: AsRef<Path>>(
        &mut self,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
    ) -> Result<ModId, ModLoadError> {
        <Self as ModRegistryAppExt>::load_mod_from_entrypoints_with(
            self,
            virtual_path,
            entrypoints,
            LoadOptions::default(),
        )
    }

    #[inline]
    fn load_mod_from_entrypoints_with<P: AsRef<Path>>(
        &mut self,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut()
            .load_mod_from_entrypoints_with(virtual_path, entrypoints, options)
    }

    #[inline]
    fn reload_mod_from_entrypoints<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        virtual_path: P,
        entrypoints: InMemoryModEntrypoints,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut()
            .reload_mod_from_entrypoints(id, virtual_path, entrypoints, options)
    }

    #[inline]
    fn load_mod_from_object_loader<L: InMemoryObjectLoader + ?Sized>(
        &mut self,
        loader: &L,
        label: &str,
        object_bytes: &[u8],
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut()
            .load_mod_from_object_loader(loader, label, object_bytes, options)
    }

    #[inline]
    fn reload_mod_from_object_loader<L: InMemoryObjectLoader + ?Sized>(
        &mut self,
        id: ModId,
        loader: &L,
        label: &str,
        object_bytes: &[u8],
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut()
            .reload_mod_from_object_loader(id, loader, label, object_bytes, options)
    }
}

#[cfg(feature = "dynamic-loader")]
impl ModLoaderAppExt for App {
    #[inline]
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        <Self as ModLoaderAppExt>::load_mod_with(self, path, LoadOptions::default())
    }

    #[inline]
    fn load_mod_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut().load_mod_with(path, options)
    }

    #[inline]
    fn reload_mod(&mut self, id: ModId) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut().reload_mod(id)
    }

    #[inline]
    fn reload_mod_with(&mut self, id: ModId, options: LoadOptions) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut().reload_mod_with(id, options)
    }

    #[inline]
    fn reload_mod_from<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut().reload_mod_from(id, path, options)
    }

    #[inline]
    fn load_mods_dir<P: AsRef<Path>>(
        &mut self,
        dir: P,
    ) -> Result<DirectoryLoadReport, ModDagError> {
        ensure_startup_tracker(self);
        self.world_mut().load_mods_dir(dir)
    }

    #[inline]
    fn load_mods_dir_with<P: AsRef<Path>>(
        &mut self,
        dir: P,
        options: LoadOptions,
    ) -> Result<DirectoryLoadReport, ModDagError> {
        ensure_startup_tracker(self);
        self.world_mut().load_mods_dir_with(dir, options)
    }

    #[cfg(feature = "signature-verification")]
    #[inline]
    fn load_bampack<P: AsRef<Path>>(&mut self, path: P) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut().load_bampack(path)
    }

    #[cfg(feature = "signature-verification")]
    #[inline]
    fn load_bampack_with<P: AsRef<Path>>(
        &mut self,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut().load_bampack_with(path, options)
    }

    #[cfg(feature = "signature-verification")]
    #[inline]
    fn reload_bampack<P: AsRef<Path>>(
        &mut self,
        id: ModId,
        path: P,
        options: LoadOptions,
    ) -> Result<ModId, ModLoadError> {
        ensure_startup_tracker(self);
        self.world_mut().reload_bampack(id, path, options)
    }
}

#[cfg(test)]
#[path = "../tests/unit/app_ext_coverage_tests.rs"]
mod coverage_tests;
