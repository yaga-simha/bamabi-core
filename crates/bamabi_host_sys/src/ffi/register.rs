//! Transactional `LoadContext` (`BamLoadCtx`), `host_register_component`, `host_resolve_component`,
//! and unified `host_register_system`.

use crate::drop_trampoline::{
    MAX_DROP_TRAMPOLINES, StagedDropBinding, WorldDropGuard, increment_drop_slot_live_values,
    on_insert_hook_for_slot, prune_drop_trampolines_for_guard,
};
use crate::handle_header::{
    AlignedBuf, HandleHeader, HostContext, LOAD_CTX_MAGIC, decode_optional_entity, ffi_try,
    lock_unpoison, validate_host_ctx, validate_load_ctx_mut, validate_load_ctx_ref,
    validate_non_empty_cstr,
};
use crate::loader::schema_migration::{
    StagedColumnMigrationUndo, rollback_staged_column_migrations,
};
use crate::loader::symbols::ensure_symbol_tables_in_world;
use crate::loader::{ModHost, ModId};
use crate::registry::catalog::{DynamicMessageReg, DynamicResourceReg, OwnedFieldStrings};
use crate::registry::{
    DynamicComponentReg, ExtractEventTargetFn, ModRegistry, RegistrySnapshot,
    component_flags_to_storage_flags, extract_owned_dynamic_fields,
};
use crate::schedule::ModNamedSystemSet;
use crate::sync_ptr::{GuestFnPtr, GuestUserData};
use crate::system::access_builder::validate_system_queries_against_snapshot;
use crate::system::{BamQuerySpecOwned, GuestSystem};
use bamabi_sys::{
    BAM_QUERY_FLAG_PARAM_SET, BAM_REQUIRED_FLAG_CUSTOM_BYTES, BAM_REQUIRED_FLAG_INIT_FN,
    BAM_REQUIRED_FLAG_RESOLVE_BY_UUID, BAM_REQUIRED_KNOWN_FLAGS, BAM_STORAGE_FLAG_MESSAGE,
    BAM_STORAGE_FLAG_RESOURCE, BamComponent, BamEventKind, BamFieldDesc, BamHostCtx, BamLoadCtx,
    BamObserverCtxFn, BamObserverCtxMutFn, BamObserverDesc, BamObserverFn, BamObserverMutFn,
    BamQueryDesc, BamRequiredComponent, BamRequiredInitFn, BamSystem, BamSystemDesc, BamTermDesc,
    ComponentCloneFn, ComponentDesc, ComponentHookFn, ComponentUuid, FfiEntity, FfiResult,
    MAX_QUERIES_PER_SYSTEM, read_descriptor,
};
use bevy_ecs::component::{ComponentCloneBehavior, ComponentDescriptor, ComponentId, StorageType};
use bevy_ecs::entity::{ComponentCloneCtx, Entity, SourceComponent};
use bevy_ecs::event::EventKey;
use bevy_ecs::lifecycle::HookContext;
use bevy_ecs::prelude::Resource;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::world::{DeferredWorld, World};
use bevy_platform::collections::{HashMap, HashSet};
use core::alloc::Layout;
use core::ffi::{CStr, c_char, c_void};
use core::sync::atomic::{AtomicBool, Ordering};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Dynamic Bevy ECS observer registration captured during `bamabi_init` or runtime `cmd_observe`.
#[derive(Clone)]
pub struct GuestObserverBinding {
    /// Human-readable observer name for diagnostics.
    pub name: Arc<str>,
    /// Pre-resolved 1-based [`BamComponent`] handle of the observed event (`INVALID` for lifecycle).
    pub event: BamComponent,
    /// Observed event kind (`CUSTOM`, `ADD`, `INSERT`, `DISCARD`, `REMOVE`, `DESPAWN`, or `READY`).
    pub event_kind: BamEventKind,
    /// Underlying Bevy [`ComponentId`] corresponding to `event`, if any.
    pub event_id: Option<ComponentId>,
    /// Bevy [`EventKey`] dispatched by `world.trigger*` / `world.trigger_dynamic*`.
    pub event_key: EventKey,
    /// Payload size (`size_of::<E>()`).
    pub event_size: usize,
    /// Whether `guest_observer_runner` synthesizes an aligned [`FfiEntity`] payload on the stack
    /// rather than passing raw `&E` bytes.
    pub synthesizes_ffi_entity_payload: bool,
    /// Optional watched target entity (`None` for global observers).
    pub target_entity: Option<Entity>,
    /// Watched component IDs for lifecycle / component-scoped observers.
    pub watched_components: Box<[ComponentId]>,
    /// Extractor returning the target entity from typed host `EntityEvent` payloads.
    pub extract_target_fn: Option<ExtractEventTargetFn>,
    /// Optional read-only foreign C-ABI callback (`BamObserverFn`).
    pub callback: Option<GuestFnPtr<BamObserverFn>>,
    /// Optional mutable foreign C-ABI callback (`BamObserverMutFn`).
    pub mut_callback: Option<GuestFnPtr<BamObserverMutFn>>,
    /// Optional [`BamSystemCtx`](bamabi_sys::BamSystemCtx)-aware foreign C-ABI callback (`BamObserverCtxFn`).
    pub ctx_callback: Option<GuestFnPtr<BamObserverCtxFn>>,
    /// Optional [`BamSystemCtx`](bamabi_sys::BamSystemCtx)-aware mutable foreign C-ABI callback
    /// (`BamObserverCtxMutFn`).
    pub ctx_mut_callback: Option<GuestFnPtr<BamObserverCtxMutFn>>,
    /// Validated query/resource clauses backing `ctx_callback`.
    pub query_specs: Vec<BamQuerySpecOwned>,
    /// Whether propagation control is enabled (`BAM_OBSERVER_FLAG_PROPAGATE`).
    pub is_propagating: bool,
    /// Optional relationship component ID for propagation (`None` defaults to `ChildOf`).
    pub propagate_relationship: Option<ComponentId>,
    /// Opaque guest state pointer forwarded to `callback` / `mut_callback`.
    pub user_data: GuestUserData,
    /// Shared module liveness flag disarmed before `dlclose`.
    pub is_active: Arc<AtomicBool>,
    /// CoW registry handle for snapshot lookup inside commands recorded by the observer.
    pub registry: ModRegistry,
    /// Owning [`ModId`].
    pub mod_id: Option<ModId>,
}

// ============================================================================
// Per-World Dynamic Component Lifecycle Hooks & Transitive Required Components
// ============================================================================

#[derive(Clone)]
enum RequiredInitStrategy {
    Default,
    CustomBytes(Arc<[u8]>),
    InitFn {
        init_fn: GuestFnPtr<BamRequiredInitFn>,
        user_data: GuestUserData,
        is_active: Arc<AtomicBool>,
    },
}

#[derive(Clone)]
struct ResolvedRequiredInit {
    handle: BamComponent,
    strategy: RequiredInitStrategy,
}

#[derive(Clone)]
struct DynamicHookBinding {
    on_add: Option<GuestFnPtr<ComponentHookFn>>,
    on_insert: Option<GuestFnPtr<ComponentHookFn>>,
    on_discard: Option<GuestFnPtr<ComponentHookFn>>,
    on_remove: Option<GuestFnPtr<ComponentHookFn>>,
    on_despawn: Option<GuestFnPtr<ComponentHookFn>>,
    user_data: GuestUserData,
    is_active: Arc<AtomicBool>,
    required_inits: Arc<[ResolvedRequiredInit]>,
    registry: ModRegistry,
}

#[derive(Clone)]
struct CloneBindingEntry {
    clone_fn: GuestFnPtr<ComponentCloneFn>,
    user_data: GuestUserData,
    layout: Layout,
    is_active: Arc<AtomicBool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DynamicRelBinding {
    target_cid: ComponentId,
    allow_self_referential: bool,
}

#[derive(Resource, Default)]
pub(crate) struct DynamicComponentHookMap {
    by_id: HashMap<ComponentId, Vec<DynamicHookBinding>>,
    drop_slot_by_id: HashMap<ComponentId, usize>,
    clone_by_id: HashMap<ComponentId, Vec<CloneBindingEntry>>,
    rel_to_target: HashMap<ComponentId, DynamicRelBinding>,
    target_to_rel: HashMap<ComponentId, ComponentId>,
    rel_targets: HashMap<(ComponentId, Entity), Vec<FfiEntity>>,
}

/// Ensures the per-`World` dynamic component hook/clone dispatch table is pre-initialized so
/// registering dynamic components never lazily allocates an `IsResource` entity mid-session.
#[inline]
pub fn ensure_dynamic_hook_map_in_world(world: &mut World) {
    world.init_resource::<DynamicComponentHookMap>();
}

impl DynamicComponentHookMap {
    #[inline]
    pub(crate) fn is_relationship_target(&self, target_comp_id: ComponentId) -> bool {
        self.target_to_rel.contains_key(&target_comp_id)
    }

    #[inline]
    pub(crate) fn relationship_targets_for(
        &self,
        target_comp_id: ComponentId,
        entity: Entity,
    ) -> &[FfiEntity] {
        self.rel_targets
            .get(&(target_comp_id, entity))
            .map_or(&[], Vec::as_slice)
    }

    pub(crate) fn active_clone_binding(
        &self,
        comp_id: ComponentId,
        expected_layout: Layout,
    ) -> Option<(ComponentCloneFn, *mut c_void)> {
        self.clone_by_id
            .get(&comp_id)
            .and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|e| e.is_active.load(Ordering::Acquire))
            })
            .filter(|entry| entry.layout == expected_layout)
            .map(|entry| (entry.clone_fn.0, entry.user_data.as_ptr()))
    }

    fn insert_or_update(&mut self, component_id: ComponentId, binding: DynamicHookBinding) {
        let entries = self.by_id.entry(component_id).or_default();
        entries.retain(|e| e.is_active.load(Ordering::Acquire));
        if let Some(existing) = entries
            .iter_mut()
            .find(|e| Arc::ptr_eq(&e.is_active, &binding.is_active))
        {
            *existing = binding;
        } else {
            entries.push(binding);
        }
    }

    fn set_drop_slot(&mut self, component_id: ComponentId, slot: usize) {
        self.drop_slot_by_id.insert(component_id, slot);
    }

    fn insert_or_update_clone(&mut self, component_id: ComponentId, entry: CloneBindingEntry) {
        let entries = self.clone_by_id.entry(component_id).or_default();
        entries.retain(|e| e.is_active.load(Ordering::Acquire));
        if let Some(existing) = entries
            .iter_mut()
            .find(|e| Arc::ptr_eq(&e.is_active, &entry.is_active))
        {
            *existing = entry;
        } else {
            entries.push(entry);
        }
    }

    fn bind_relationship_pair(
        &mut self,
        rel_cid: ComponentId,
        tgt_cid: ComponentId,
        allow_self_referential: bool,
    ) {
        self.rel_to_target.insert(
            rel_cid,
            DynamicRelBinding {
                target_cid: tgt_cid,
                allow_self_referential,
            },
        );
        self.target_to_rel.insert(tgt_cid, rel_cid);
    }

    fn rollback_for_guard(
        &mut self,
        is_active: &Arc<AtomicBool>,
        fresh_comp_ids: &[ComponentId],
        fresh_rel_bindings: &[(ComponentId, ComponentId)],
    ) {
        self.by_id.retain(|_, entries| {
            entries.retain(|e| {
                !Arc::ptr_eq(&e.is_active, is_active) && e.is_active.load(Ordering::Acquire)
            });
            !entries.is_empty()
        });
        self.clone_by_id.retain(|_, entries| {
            entries.retain(|e| {
                !Arc::ptr_eq(&e.is_active, is_active) && e.is_active.load(Ordering::Acquire)
            });
            !entries.is_empty()
        });
        for comp_id in fresh_comp_ids {
            self.drop_slot_by_id.remove(comp_id);
        }
        for &(rel_cid, tgt_cid) in fresh_rel_bindings {
            self.rel_to_target.remove(&rel_cid);
            self.target_to_rel.remove(&tgt_cid);
        }
    }
}

fn insert_required_raw_buffer(
    world: &mut World,
    entity: Entity,
    req_meta: &crate::registry::RegisteredComponent,
    buf: &AlignedBuf,
) -> bool {
    if !req_meta.is_dynamic {
        if let Some(insert_raw) = req_meta.insert_raw_fn {
            // SAFETY: `buf` holds `req_meta.size` initialized bytes aligned to `req_meta.align`
            // matching `req_meta`'s raw-insertable host layout.
            unsafe {
                insert_raw(world, entity, buf.as_ptr());
            }
            return true;
        }
        return false;
    }

    if let Ok(mut e_mut) = world.get_entity_mut(entity) {
        // SAFETY: `buf.non_null()` points to `req_meta.size` initialized bytes matching
        // `req_meta.component_id`'s layout; `insert_by_id` copies out without freeing `buf`.
        let owning = unsafe { OwningPtr::new(buf.non_null()) };
        // SAFETY: `req_meta.component_id` was registered in `world` with `(size, align)`.
        unsafe {
            e_mut.insert_by_id(req_meta.component_id, owning);
        }
        return true;
    }
    false
}

fn insert_required_components_recursive(
    world: &mut World,
    entity: Entity,
    root_reqs: &[ResolvedRequiredInit],
    registry: &ModRegistry,
) {
    if root_reqs.is_empty() || !world.entities().contains(entity) {
        return;
    }
    let snap = registry.latest_snapshot_arc();
    let mut visited = HashSet::<BamComponent>::new();
    let mut queue = VecDeque::<ResolvedRequiredInit>::new();
    for req in root_reqs {
        if visited.insert(req.handle) {
            queue.push_back(req.clone());
        }
    }

    while let Some(req_item) = queue.pop_front() {
        let Some(req_meta) = snap.get_by_handle(req_item.handle) else {
            continue;
        };

        // Enqueue transitive custom required initializers first (preserving explicit strategies).
        let transitive_custom: Option<Arc<[ResolvedRequiredInit]>> = world
            .get_resource::<DynamicComponentHookMap>()
            .and_then(|map| map.by_id.get(&req_meta.component_id))
            .and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|e| e.is_active.load(Ordering::Acquire))
            })
            .map(|binding| Arc::clone(&binding.required_inits));
        if let Some(custom_reqs) = transitive_custom {
            for transitive in custom_reqs.iter() {
                if visited.insert(transitive.handle) {
                    queue.push_back(transitive.clone());
                }
            }
        }
        for &transitive in snap.required_components(req_item.handle) {
            if visited.insert(transitive) {
                queue.push_back(ResolvedRequiredInit {
                    handle: transitive,
                    strategy: RequiredInitStrategy::Default,
                });
            }
        }

        let Ok(ent_ref) = world.get_entity(entity) else {
            return;
        };
        if ent_ref.contains_id(req_meta.component_id) {
            continue;
        }

        match req_item.strategy {
            RequiredInitStrategy::Default => {
                if let Some(insert_default) = req_meta.insert_default_fn {
                    insert_default(world, entity);
                } else if req_meta.is_abi_pod
                    && req_meta.drop_fn.is_none()
                    && req_meta.drop_slot.is_none()
                {
                    let Ok(zeroed) = AlignedBuf::new_zeroed(req_meta.size, req_meta.align) else {
                        continue;
                    };
                    let _ = insert_required_raw_buffer(world, entity, req_meta, &zeroed);
                }
            }
            RequiredInitStrategy::CustomBytes(ref bytes) => {
                if bytes.len() != req_meta.size
                    || !req_meta.is_abi_pod
                    || req_meta.drop_fn.is_some()
                    || req_meta.drop_slot.is_some()
                {
                    continue;
                }
                let Ok(buf) = AlignedBuf::new_zeroed(req_meta.size, req_meta.align) else {
                    continue;
                };
                if !bytes.is_empty() {
                    // SAFETY: `buf` is allocated for `req_meta.size == bytes.len()` writable
                    // bytes and does not overlap `bytes`.
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            bytes.as_ptr(),
                            buf.as_mut_ptr(),
                            bytes.len(),
                        );
                    }
                }
                let _ = insert_required_raw_buffer(world, entity, req_meta, &buf);
            }
            RequiredInitStrategy::InitFn {
                init_fn: GuestFnPtr(init_fn),
                user_data,
                ref is_active,
            } => {
                if !is_active.load(Ordering::Acquire) {
                    continue;
                }
                let Ok(buf) = AlignedBuf::new_zeroed(req_meta.size, req_meta.align) else {
                    continue;
                };
                let dst_ptr = buf.as_mut_c_void();
                // SAFETY: `is_active` was verified true (`Ordering::Acquire`), `dst_ptr` points to
                // `req_meta.size` writable `req_meta.align`-aligned bytes, and `init_fn` upholds
                // `BamRequiredInitFn`'s nounwind contract.
                let rc = unsafe { init_fn(dst_ptr, user_data.as_ptr()) };
                if !rc.is_ok() {
                    continue;
                }
                let inserted = insert_required_raw_buffer(world, entity, req_meta, &buf);
                if !inserted
                    && let Some(drop_fn) = req_meta.drop_fn
                    && is_active.load(Ordering::Acquire)
                {
                    // SAFETY: `init_fn` returned `OK` initializing `dst_ptr`, and ownership was not
                    // transferred into ECS storage, so run `drop_fn` before `buf` deallocates.
                    unsafe {
                        drop_fn(dst_ptr);
                    }
                }
            }
        }
    }
}

fn active_hook_binding<'a>(
    world: &'a DeferredWorld<'_>,
    component_id: ComponentId,
) -> Option<&'a DynamicHookBinding> {
    let map = world.get_resource::<DynamicComponentHookMap>()?;
    let entries = map.by_id.get(&component_id)?;
    entries
        .iter()
        .rev()
        .find(|e| e.is_active.load(Ordering::Acquire))
}

fn invoke_dynamic_hook(
    world: &DeferredWorld<'_>,
    ctx: HookContext,
    select: fn(&DynamicHookBinding) -> Option<GuestFnPtr<ComponentHookFn>>,
) {
    let Some(entry) = active_hook_binding(world, ctx.component_id) else {
        return;
    };
    let Some(GuestFnPtr(cb)) = select(entry) else {
        return;
    };
    let Ok(comp_ptr) = world
        .entity(ctx.entity)
        .get_by_id(ctx.component_id)
        .map(|p| p.as_ptr().cast::<c_void>())
    else {
        return;
    };
    let ffi_entity = FfiEntity::from_bits(ctx.entity.to_bits());
    // SAFETY: `entry.is_active` is verified true (`Ordering::Acquire`), `cb` remains mapped in
    // memory for this synchronous hook call, and `comp_ptr` points to the live component value.
    unsafe {
        cb(ffi_entity, comp_ptr, entry.user_data.as_ptr());
    }
}

fn dynamic_on_add_hook(mut world: DeferredWorld<'_>, ctx: HookContext) {
    let req_info = active_hook_binding(&world, ctx.component_id).and_then(|entry| {
        if entry.required_inits.is_empty() {
            None
        } else {
            Some((Arc::clone(&entry.required_inits), entry.registry.clone()))
        }
    });
    if let Some((reqs, registry)) = req_info {
        world.commands().queue(move |w: &mut World| {
            insert_required_components_recursive(w, ctx.entity, &reqs, &registry);
        });
    }
    invoke_dynamic_hook(&world, ctx, |e| e.on_add);
}

fn dynamic_rel_on_insert(world: &mut DeferredWorld<'_>, ctx: HookContext) {
    let Some(rel_binding) = world
        .get_resource::<DynamicComponentHookMap>()
        .and_then(|map| map.rel_to_target.get(&ctx.component_id).copied())
    else {
        return;
    };
    let tgt_cid = rel_binding.target_cid;
    let target_ffi = {
        let Ok(ptr) = world.entity(ctx.entity).get_by_id(ctx.component_id) else {
            return;
        };
        // SAFETY: Dynamic relationship source components are validated as `(size = 8, align = 8)`
        // storing an `FfiEntity` target handle at offset 0.
        let bits = unsafe { core::ptr::read_unaligned(ptr.as_ptr().cast::<u64>()) };
        FfiEntity::from_bits(bits)
    };
    let valid_target = (!target_ffi.is_null())
        .then(|| Entity::try_from_bits(target_ffi.to_bits()))
        .flatten()
        .filter(|&t| {
            t != Entity::PLACEHOLDER
                && (rel_binding.allow_self_referential || t != ctx.entity)
                && world.entities().contains(t)
        });
    let Some(target_entity) = valid_target else {
        let source_entity = ctx.entity;
        let rel_cid = ctx.component_id;
        world.commands().queue(move |w: &mut World| {
            if let Ok(mut e) = w.get_entity_mut(source_entity) {
                e.remove_by_id(rel_cid);
            }
        });
        return;
    };

    let source_ffi = FfiEntity::from_bits(ctx.entity.to_bits());
    if let Some(mut map) = world.get_resource_mut::<DynamicComponentHookMap>() {
        let vec = map.rel_targets.entry((tgt_cid, target_entity)).or_default();
        if !vec.contains(&source_ffi) {
            vec.push(source_ffi);
        }
    }
    if world
        .get_entity(target_entity)
        .is_ok_and(|ent| !ent.contains_id(tgt_cid))
    {
        world.commands().queue(move |w: &mut World| {
            let should_insert = w
                .get_resource::<DynamicComponentHookMap>()
                .is_some_and(|m| {
                    !m.relationship_targets_for(tgt_cid, target_entity)
                        .is_empty()
                });
            if should_insert
                && let Ok(mut target_ent) = w.get_entity_mut(target_entity)
                && !target_ent.contains_id(tgt_cid)
            {
                // SAFETY: `tgt_cid` was registered with zero-sized `(0, 1)` layout.
                let owning = unsafe { OwningPtr::new(core::ptr::NonNull::<u8>::dangling()) };
                // SAFETY: `tgt_cid` is a valid zero-sized component in `w`.
                unsafe {
                    target_ent.insert_by_id(tgt_cid, owning);
                }
            }
        });
    }
}

fn dynamic_rel_on_discard(world: &mut DeferredWorld<'_>, ctx: HookContext) {
    let Some(tgt_cid) = world
        .get_resource::<DynamicComponentHookMap>()
        .and_then(|map| {
            map.rel_to_target
                .get(&ctx.component_id)
                .map(|b| b.target_cid)
        })
    else {
        return;
    };
    let old_target_ffi = {
        let Ok(ptr) = world.entity(ctx.entity).get_by_id(ctx.component_id) else {
            return;
        };
        // SAFETY: Dynamic relationship source components are validated as `(size = 8, align = 8)`
        // storing an `FfiEntity` target handle at offset 0.
        let bits = unsafe { core::ptr::read_unaligned(ptr.as_ptr().cast::<u64>()) };
        FfiEntity::from_bits(bits)
    };
    let Some(old_target) = (!old_target_ffi.is_null())
        .then(|| Entity::try_from_bits(old_target_ffi.to_bits()))
        .flatten()
        .filter(|&t| t != Entity::PLACEHOLDER)
    else {
        return;
    };

    let source_ffi = FfiEntity::from_bits(ctx.entity.to_bits());
    let became_empty = if let Some(mut map) = world.get_resource_mut::<DynamicComponentHookMap>() {
        if let Some(vec) = map.rel_targets.get_mut(&(tgt_cid, old_target)) {
            vec.retain(|&e| e != source_ffi);
            if vec.is_empty() {
                map.rel_targets.remove(&(tgt_cid, old_target));
                true
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };
    if became_empty && world.entities().contains(old_target) {
        world.commands().queue(move |w: &mut World| {
            let still_empty = w
                .get_resource::<DynamicComponentHookMap>()
                .is_some_and(|m| m.relationship_targets_for(tgt_cid, old_target).is_empty());
            if still_empty
                && let Ok(mut target_ent) = w.get_entity_mut(old_target)
                && target_ent.contains_id(tgt_cid)
            {
                target_ent.remove_by_id(tgt_cid);
            }
        });
    }
}

fn dynamic_rel_target_on_despawn(world: &mut DeferredWorld<'_>, ctx: HookContext) {
    let (rel_cid, sources) = {
        let Some(mut map) = world.get_resource_mut::<DynamicComponentHookMap>() else {
            return;
        };
        let Some(rel_cid) = map.target_to_rel.get(&ctx.component_id).copied() else {
            return;
        };
        let sources = map
            .rel_targets
            .remove(&(ctx.component_id, ctx.entity))
            .unwrap_or_default();
        (rel_cid, sources)
    };
    if sources.is_empty() {
        return;
    }
    let despawning_entity = ctx.entity;
    let expected_bits = despawning_entity.to_bits();
    world.commands().queue(move |w: &mut World| {
        for src_ffi in sources {
            let Some(src_ent) = (!src_ffi.is_null())
                .then(|| Entity::try_from_bits(src_ffi.to_bits()))
                .flatten()
                .filter(|&e| e != Entity::PLACEHOLDER && e != despawning_entity)
            else {
                continue;
            };
            let matches = w
                .get_entity(src_ent)
                .ok()
                .and_then(|ent| ent.get_by_id(rel_cid).ok())
                .is_some_and(|ptr| {
                    // SAFETY: `rel_cid` stores an 8-byte `FfiEntity` target handle at offset 0.
                    unsafe {
                        core::ptr::read_unaligned(ptr.as_ptr().cast::<u64>()) == expected_bits
                    }
                });
            if matches && let Ok(mut ent_mut) = w.get_entity_mut(src_ent) {
                ent_mut.remove_by_id(rel_cid);
            }
        }
    });
}

fn dynamic_on_insert_hook(mut world: DeferredWorld<'_>, ctx: HookContext) {
    // Increment unconditionally regardless of `is_active` because `ComponentDescriptor` permanently
    // binds `drop_trampoline::<SLOT>` to `ctx.component_id`, which always decrements `live_values`
    // on despawn / `World::drop`.
    if let Some(slot) = world
        .get_resource::<DynamicComponentHookMap>()
        .and_then(|map| map.drop_slot_by_id.get(&ctx.component_id).copied())
    {
        increment_drop_slot_live_values(slot);
    }
    dynamic_rel_on_insert(&mut world, ctx);
    invoke_dynamic_hook(&world, ctx, |e| e.on_insert);
}

fn dynamic_on_discard_hook(mut world: DeferredWorld<'_>, ctx: HookContext) {
    dynamic_rel_on_discard(&mut world, ctx);
    invoke_dynamic_hook(&world, ctx, |e| e.on_discard);
}

fn dynamic_on_remove_hook(world: DeferredWorld<'_>, ctx: HookContext) {
    invoke_dynamic_hook(&world, ctx, |e| e.on_remove);
}

fn dynamic_on_despawn_hook(mut world: DeferredWorld<'_>, ctx: HookContext) {
    dynamic_rel_target_on_despawn(&mut world, ctx);
    invoke_dynamic_hook(&world, ctx, |e| e.on_despawn);
}

// ============================================================================
// Per-World Disarmed-on-Unload Custom Component Clone Runner (`clone_fn`)
// ============================================================================

fn dynamic_clone_runner(_source: &SourceComponent, ctx: &mut ComponentCloneCtx) {
    let comp_id = ctx.component_id();
    let source_entity = ctx.source();
    let target_entity = ctx.target();
    let expected_layout = ctx.component_info().layout();

    ctx.queue_deferred(move |world: &mut World, _mapper| {
        let active_entry = world
            .get_resource::<DynamicComponentHookMap>()
            .and_then(|map| map.clone_by_id.get(&comp_id))
            .and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|e| e.is_active.load(Ordering::Acquire))
            })
            .filter(|entry| entry.layout == expected_layout)
            .map(|entry| (entry.clone_fn, entry.user_data));

        let Some((GuestFnPtr(clone_fn), user_data)) = active_entry else {
            return;
        };

        if world.get_entity(target_entity).is_err() {
            return;
        }

        let size = expected_layout.size();
        if size == 0 {
            if !world
                .get_entity(source_entity)
                .is_ok_and(|ent| ent.contains_id(comp_id))
            {
                return;
            }
            // SAFETY: `is_active` was verified true (`Ordering::Acquire`) and `size == 0`, so
            // `clone_fn` receives valid non-null dangling ZST pointers without reading/writing.
            let status = unsafe {
                clone_fn(
                    core::ptr::NonNull::<u8>::dangling()
                        .as_ptr()
                        .cast::<c_void>(),
                    core::ptr::NonNull::<u8>::dangling()
                        .as_ptr()
                        .cast::<c_void>(),
                    user_data.as_ptr(),
                )
            };
            if status.is_ok()
                && let Ok(mut target_ent) = world.get_entity_mut(target_entity)
            {
                // SAFETY: `size == 0`; `NonNull::dangling()` is valid for ZST `OwningPtr`.
                let owning = unsafe { OwningPtr::new(core::ptr::NonNull::<u8>::dangling()) };
                // SAFETY: `comp_id` was registered in `world` with zero-sized `expected_layout`.
                unsafe {
                    target_ent.insert_by_id(comp_id, owning);
                }
            }
            return;
        }

        let temp = AlignedBuf::from_layout_zeroed(expected_layout);
        let status = {
            let Ok(source_ent) = world.get_entity(source_entity) else {
                return;
            };
            let Ok(src_ptr) = source_ent.get_by_id(comp_id) else {
                return;
            };
            // SAFETY: `src_ptr` points to `size` initialized source bytes; `temp` points to
            // `size` writable `expected_layout.align()`-aligned bytes; `is_active` is true.
            unsafe {
                clone_fn(
                    src_ptr.as_ptr().cast::<c_void>(),
                    temp.as_mut_c_void(),
                    user_data.as_ptr(),
                )
            }
        };

        if status.is_ok()
            && let Ok(mut target_ent) = world.get_entity_mut(target_entity)
        {
            // SAFETY: `temp.non_null()` points to `size` initialized bytes written by `clone_fn`;
            // `insert_by_id` moves out of `owning` without freeing `temp`'s scratch buffer.
            let owning = unsafe { OwningPtr::new(temp.non_null()) };
            // SAFETY: `comp_id` was registered in `world` with `expected_layout`.
            unsafe {
                target_ent.insert_by_id(comp_id, owning);
            }
        }
    });
}

/// Concrete `#[repr(C)]` initialization context backing opaque [`BamLoadCtx`].
///
/// Valid strictly for the duration of `bamabi_init`; poisoned automatically on `finish_init`
/// and `Drop` without any thread-local global state.
#[repr(C)]
pub struct LoadContext {
    /// Leading handle validation header (`LOAD_CTX_MAGIC`) at byte offset `0`.
    pub header: HandleHeader,
    /// Embedded world-scoped `HostContext` returned by `host_load_host_ctx`.
    pub host_ctx: HostContext,
    /// Mutable Copy-on-Write [`ModRegistry`] updated as dynamic components register.
    pub registry: ModRegistry,
    /// Accumulated systems registered during `bamabi_init`.
    pub registered_systems: Vec<GuestSystem>,
    /// Accumulated dynamic observers registered during `bamabi_init`.
    pub registered_observers: Vec<GuestObserverBinding>,
    /// Handles of dynamic components registered during this init session.
    pub registered_components: Vec<BamComponent>,
    /// Shared liveness flag for this module load session.
    pub is_active: Arc<AtomicBool>,
    /// Pointer to the exclusively borrowed Bevy [`World`] (or null in standalone unit tests).
    pub world_ptr: *mut World,
    /// Fallback standalone `World` used only when `world_ptr` is null in isolated FFI unit tests.
    standalone_world: Option<Box<World>>,
    /// Assigned [`ModId`] for this load session.
    pub mod_id: Option<ModId>,
    /// Optional manifest module name used to prefix unqualified guest component names.
    pub manifest_name: Option<Arc<str>>,
    /// Reload generation assigned to the mod currently executing `bamabi_init`.
    pub mod_generation: u64,
    known_mod_generations: HashMap<Arc<str>, u64>,
    pre_snapshot: Arc<RegistrySnapshot>,
    staged_drop_bindings: Arc<Mutex<Vec<StagedDropBinding>>>,
    pub(crate) staged_migrations: Option<Arc<Mutex<Vec<StagedColumnMigrationUndo>>>>,
    fresh_dynamic_comp_ids: Vec<ComponentId>,
    fresh_rel_bindings: Vec<(ComponentId, ComponentId)>,
    system_filter: Option<HashSet<Arc<str>>>,
    in_init: bool,
}

const _: () = assert!(core::mem::offset_of!(LoadContext, header) == 0);

impl LoadContext {
    /// Creates an active [`LoadContext`] bound to `registry`, `is_active`, `world_ptr`, and `mod_id`.
    pub fn with_world_and_mod_id(
        registry: ModRegistry,
        is_active: Arc<AtomicBool>,
        world_ptr: *mut World,
        mod_id: Option<ModId>,
    ) -> Self {
        Self::with_manifest_name(registry, is_active, world_ptr, mod_id, None)
    }

    /// Creates an active [`LoadContext`] bound to `registry`, `is_active`, `world_ptr`, `mod_id`,
    /// and canonical `manifest_name` (`"<manifest.name>::<Name>"` prefixing for unqualified components).
    pub fn with_manifest_name(
        registry: ModRegistry,
        is_active: Arc<AtomicBool>,
        world_ptr: *mut World,
        mod_id: Option<ModId>,
        manifest_name: Option<Arc<str>>,
    ) -> Self {
        let pre_snapshot = registry.snapshot_arc();
        let host_ctx =
            HostContext::new(Arc::clone(&pre_snapshot)).with_namespace(manifest_name.clone());
        let mut ctx = Self {
            header: HandleHeader::new(LOAD_CTX_MAGIC),
            host_ctx,
            registry,
            registered_systems: Vec::new(),
            registered_observers: Vec::new(),
            registered_components: Vec::new(),
            is_active,
            world_ptr,
            standalone_world: None,
            mod_id,
            manifest_name,
            mod_generation: 0,
            known_mod_generations: HashMap::default(),
            pre_snapshot,
            staged_drop_bindings: Arc::new(Mutex::new(Vec::new())),
            staged_migrations: None,
            fresh_dynamic_comp_ids: Vec::new(),
            fresh_rel_bindings: Vec::new(),
            system_filter: None,
            in_init: true,
        };
        if !ctx.world_ptr.is_null() {
            let world_tables_id = ensure_symbol_tables_in_world(ctx.world_mut());
            ctx.registry.attach_world_tables_id(world_tables_id);
            if ctx.mod_id.is_some()
                || ctx
                    .registry
                    .get_by_name(bamabi_sys::BAM_CHILD_OF_NAME)
                    .is_some()
            {
                let children_id = ctx
                    .world_mut()
                    .register_component::<bevy_ecs::hierarchy::Children>();
                if ctx
                    .registry
                    .get_by_name(bamabi_sys::BAM_CHILDREN_NAME)
                    .is_none()
                {
                    let _ = ctx.registry.register_children_hierarchy(children_id);
                }
            }
            let updated_reg = ctx.registry.clone();
            if let Some(mut world_reg) = ctx.world_mut().get_resource_mut::<ModRegistry>() {
                *world_reg = updated_reg;
            }
            let new_snap = ctx.registry.snapshot_arc();
            ctx.pre_snapshot = Arc::clone(&new_snap);
            ctx.host_ctx.set_snapshot(new_snap);
        }
        ctx
    }

    /// Shares the transaction's [`StagedDropBinding`] log so `LoadTransaction::drop`
    /// automatically rolls back every trampoline mutation recorded during `bamabi_init`.
    pub(crate) fn with_staged_drop_bindings(
        mut self,
        staged: Arc<Mutex<Vec<StagedDropBinding>>>,
    ) -> Self {
        self.staged_drop_bindings = staged;
        self
    }

    /// Shares the transaction's [`StagedColumnMigrationUndo`] log so `LoadTransaction::drop` and
    /// `rollback_unscheduled_mod_in_world` can revert in-place `migrate_component_column` swaps
    /// if initialization or cascade staging subsequently fails.
    pub(crate) fn with_staged_migrations(
        mut self,
        staged: Arc<Mutex<Vec<StagedColumnMigrationUndo>>>,
    ) -> Self {
        self.staged_migrations = Some(staged);
        self
    }

    /// Sets the reload generation assigned to the mod currently executing `bamabi_init`.
    #[inline]
    pub(crate) fn with_mod_generation(mut self, generation: u64) -> Self {
        self.mod_generation = generation;
        self
    }

    /// Attaches a snapshot of all known mod generations accessible while `ModHost` is temporarily
    /// extracted from `World` via `resource_scope` during `bamabi_init`.
    #[inline]
    pub(crate) fn with_known_mod_generations(
        mut self,
        generations: HashMap<Arc<str>, u64>,
    ) -> Self {
        self.known_mod_generations = generations;
        self
    }

    /// Resolves the active reload generation for `mod_name` during `bamabi_init`, consulting this
    /// `LoadContext`'s active generation and generation snapshot before falling back to `ModHost`.
    #[inline]
    pub(crate) fn mod_generation_for(&self, world: Option<&World>, mod_name: &str) -> u64 {
        if self.manifest_name.as_deref() == Some(mod_name) {
            return self.mod_generation;
        }
        if let Some(&gen_val) = self.known_mod_generations.get(mod_name) {
            return gen_val;
        }
        world
            .and_then(|w| w.get_resource::<ModHost>())
            .map(|h| h.mod_generation(mod_name))
            .unwrap_or(0)
    }

    /// Creates an active standalone [`LoadContext`] for unit testing without an external `World`.
    pub fn new(registry: ModRegistry) -> Self {
        Self::with_world_and_mod_id(
            registry,
            Arc::new(AtomicBool::new(true)),
            core::ptr::null_mut(),
            None,
        )
    }

    /// Ends the active initialization window, poisoning both `LoadContext` and its embedded
    /// `HostContext` so any stashed FFI pointers fail validation immediately.
    #[inline]
    pub fn finish_init(&mut self) {
        self.in_init = false;
        self.header.poison();
        self.host_ctx.header.poison();
    }

    /// Rolls back all drop trampoline bindings and registry mutations recorded in this `LoadContext`,
    /// returning freshly allocated trampoline slots to `FREE_LIST` and
    /// restoring the initial `RegistrySnapshot`.
    pub fn rollback(&mut self) {
        self.finish_init();
        let bindings = core::mem::take(&mut *lock_unpoison(&self.staged_drop_bindings));
        let migrations = self
            .staged_migrations
            .take()
            .map(|log| core::mem::take(&mut *lock_unpoison(&log)))
            .unwrap_or_default();
        let fresh_comp_ids = core::mem::take(&mut self.fresh_dynamic_comp_ids);
        let fresh_rels = core::mem::take(&mut self.fresh_rel_bindings);
        let active_guard = Arc::clone(&self.is_active);
        if !migrations.is_empty() && !self.world_ptr.is_null() {
            rollback_staged_column_migrations(self.world_mut(), migrations);
        }
        if let Some(mut drop_guard) = self.world_mut().get_resource_mut::<WorldDropGuard>() {
            drop_guard.rollback_staged_bindings(&bindings);
        }
        if let Some(mut hook_map) = self
            .world_mut()
            .get_resource_mut::<DynamicComponentHookMap>()
        {
            hook_map.rollback_for_guard(&active_guard, &fresh_comp_ids, &fresh_rels);
        }
        prune_drop_trampolines_for_guard(&self.is_active);
        let pre = Arc::clone(&self.pre_snapshot);
        self.registry.restore_snapshot(Arc::clone(&pre));
        self.host_ctx.set_snapshot(Arc::clone(&pre));
        if !self.world_ptr.is_null()
            && let Some(mut world_reg) = self.world_mut().get_resource_mut::<ModRegistry>()
        {
            world_reg.restore_snapshot(pre);
        }
        self.registered_components.clear();
        self.registered_systems.clear();
    }

    /// Casts `&mut self` to an opaque `*mut BamLoadCtx` FFI pointer.
    #[inline]
    pub fn as_ffi_ptr(&mut self) -> *mut BamLoadCtx {
        (self as *mut Self).cast::<BamLoadCtx>()
    }

    pub(crate) fn world_mut(&mut self) -> &mut World {
        if !self.world_ptr.is_null() {
            // SAFETY: `self.world_ptr` comes from an exclusively borrowed `&mut World` held for the
            // duration of `bamabi_init` with no aliasing references.
            unsafe { &mut *self.world_ptr }
        } else {
            self.standalone_world
                .get_or_insert_with(|| Box::new(World::new()))
        }
    }

    pub(crate) fn registry_and_world_mut(&mut self) -> (&mut ModRegistry, &mut World) {
        let world = if !self.world_ptr.is_null() {
            // SAFETY: `self.world_ptr` comes from an exclusively borrowed `&mut World` held for the
            // duration of `bamabi_init` with no aliasing references.
            unsafe { &mut *self.world_ptr }
        } else {
            self.standalone_world
                .get_or_insert_with(|| Box::new(World::new()))
        };
        (&mut self.registry, world)
    }

    /// Configures an optional system allowlist checked during system registration.
    pub fn with_system_filter(
        mut self,
        allowed: impl IntoIterator<Item = impl Into<Arc<str>>>,
    ) -> Self {
        self.system_filter = Some(allowed.into_iter().map(Into::into).collect());
        self
    }

    /// Returns `true` if `name` is permitted by the configured system registration filter.
    #[inline]
    pub fn allows_system(&self, name: &str) -> bool {
        self.system_filter
            .as_ref()
            .is_none_or(|allowed| allowed.contains(name))
    }

    pub(crate) fn record_fresh_dynamic_comp_id(&mut self, comp_id: ComponentId) {
        self.fresh_dynamic_comp_ids.push(comp_id);
    }
}

impl Drop for LoadContext {
    #[inline]
    fn drop(&mut self) {
        self.in_init = false;
        self.header.poison();
    }
}

/// Validates that `load_ctx` points to an active [`LoadContext`] currently inside `bamabi_init`.
///
/// # Safety
/// - If `load_ctx` is non-null, it must point to a readable, aligned [`LoadContext`] (or
///   sufficiently sized valid handle allocation).
#[inline]
pub(crate) unsafe fn validate_active_load_ctx<'a>(
    load_ctx: *mut BamLoadCtx,
) -> Result<&'a mut LoadContext, FfiResult> {
    // SAFETY: Unconditionally validates non-null, alignment, and `LOAD_CTX_MAGIC` + non-zero generation.
    let ctx = unsafe { validate_load_ctx_mut(load_ctx)? };
    if !ctx.in_init {
        return Err(FfiResult::INVALID_STATE);
    }
    Ok(ctx)
}

/// Obtains the world-scoped [`BamHostCtx`] pointer from an active [`BamLoadCtx`].
///
/// # Safety
/// `load_ctx` must point to an active [`LoadContext`] cast to [`BamLoadCtx`], or null.
pub unsafe extern "C" fn host_load_host_ctx(load_ctx: *const BamLoadCtx) -> *const BamHostCtx {
    // SAFETY: Unconditionally validates `load_ctx` against `LOAD_CTX_MAGIC`.
    let Ok(ctx) = (unsafe { validate_load_ctx_ref(load_ctx) }) else {
        return core::ptr::null();
    };
    if !ctx.in_init {
        return core::ptr::null();
    }
    ctx.host_ctx.as_ffi_ptr()
}

/// Resolves an existing component or resource by canonical `name`, `uuid`, or both.
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - If non-null, `name` must point to a NUL-terminated C string.
/// - If non-null, `uuid` must point to a readable [`ComponentUuid`].
/// - `out_component` must point to writable [`BamComponent`] storage, or null.
pub unsafe extern "C" fn host_resolve_component(
    ctx: *const BamHostCtx,
    name: *const c_char,
    uuid: *const ComponentUuid,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_component.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = ffi_try!(unsafe { validate_host_ctx(ctx) });

    let parsed_name = if !name.is_null() {
        // SAFETY: Caller guarantees `name` points to a NUL-terminated C string.
        match unsafe { validate_non_empty_cstr(name) } {
            Ok(s) => Some(s),
            Err(err) => {
                host_ctx.set_last_error("resolve_component `name` is empty or invalid UTF-8");
                return err;
            }
        }
    } else {
        None
    };

    let parsed_uuid = if !uuid.is_null() {
        // SAFETY: Caller guarantees `uuid` points to a readable `ComponentUuid`.
        Some(unsafe { core::ptr::read_unaligned(uuid) })
    } else {
        None
    };

    let snap = host_ctx.snapshot();
    let resolve_by_name = |raw_name: &str| {
        if let Some(found) = snap.get_by_name(raw_name) {
            return Some(found);
        }
        if !raw_name.contains("::")
            && let Some(ns) = host_ctx.default_namespace()
        {
            let qualified = format!("{ns}::{raw_name}");
            return snap.get_by_name(&qualified);
        }
        None
    };

    let entry = match (parsed_name, parsed_uuid) {
        (Some(n), Some(u)) => match resolve_by_name(n) {
            Some(by_name) => {
                if by_name.uuid != u {
                    host_ctx.set_last_error(format!(
                        "resolve_component UUID mismatch for '{n}': expected {:?}, got {u:?}",
                        by_name.uuid
                    ));
                    return FfiResult::TYPE_MISMATCH;
                }
                by_name
            }
            None => {
                if let Some(by_uuid) = snap.get_by_uuid(&u) {
                    host_ctx.set_last_error(format!(
                        "resolve_component UUID {u:?} is bound to '{}', not '{n}'",
                        by_uuid.name
                    ));
                    return FfiResult::TYPE_MISMATCH;
                }
                host_ctx.set_last_error(format!("component or resource '{n}' not found"));
                return FfiResult::NOT_FOUND;
            }
        },
        (Some(n), None) => {
            let Some(by_name) = resolve_by_name(n) else {
                host_ctx.set_last_error(format!("component or resource '{n}' not found"));
                return FfiResult::NOT_FOUND;
            };
            by_name
        }
        (None, Some(u)) => {
            let Some(by_uuid) = snap.get_by_uuid(&u) else {
                host_ctx.set_last_error(format!("component or resource with UUID {u:?} not found"));
                return FfiResult::NOT_FOUND;
            };
            by_uuid
        }
        (None, None) => {
            host_ctx.set_last_error("resolve_component requires at least one of `name` or `uuid`");
            return FfiResult::INVALID_ARGUMENT;
        }
    };

    // SAFETY: `out_component` was checked non-null above and caller guarantees writable `BamComponent`.
    unsafe {
        *out_component = entry.handle;
    }
    FfiResult::OK
}

const BAM_REL_BIND_PREFIX: &str = "__bam_rel_bind::";
const BAM_REL_BIND_SELF_PREFIX: &str = "__bam_rel_bind_self::";

/// Binds a dynamic `(Relationship, RelationshipTarget)` pair when guest code registers the
/// synthetic `"__bam_rel_bind::<rel_handle>::<target_handle>[::self]"` or
/// `"__bam_rel_bind_self::<rel_handle>::<target_handle>"` marker descriptor.
///
/// # Safety
/// `out_component` must be a non-null, aligned, writable `*mut BamComponent`.
unsafe fn bind_dynamic_relationship_descriptor(
    ctx: &mut LoadContext,
    pair_spec: &str,
    default_allow_self: bool,
    desc: &ComponentDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if desc.size != 0
        || desc.align != 1
        || desc.drop_fn.is_some()
        || desc.clone_fn.is_some()
        || desc.has_hooks()
        || desc.required_component_count != 0
    {
        ctx.host_ctx
            .set_last_error("__bam_rel_bind marker must be a zero-sized hookless descriptor");
        return FfiResult::INVALID_ARGUMENT;
    }
    let (pair_body, allow_self_referential) =
        if let Some(stripped) = pair_spec.strip_suffix("::self") {
            (stripped, true)
        } else {
            (pair_spec, default_allow_self)
        };
    let Some((rel_str, tgt_str)) = pair_body.split_once("::") else {
        ctx.host_ctx
            .set_last_error("malformed __bam_rel_bind relationship descriptor");
        return FfiResult::INVALID_ARGUMENT;
    };
    let (Ok(rel_raw), Ok(tgt_raw)) = (rel_str.parse::<u32>(), tgt_str.parse::<u32>()) else {
        ctx.host_ctx
            .set_last_error("malformed __bam_rel_bind component handle integer");
        return FfiResult::INVALID_ARGUMENT;
    };
    if rel_raw == 0 || tgt_raw == 0 {
        return FfiResult::INVALID_COMPONENT;
    }
    let rel_handle = BamComponent::from_raw(rel_raw);
    let tgt_handle = BamComponent::from_raw(tgt_raw);

    let Some(rel_meta) = ctx.registry.get_by_handle(rel_handle).cloned() else {
        ctx.host_ctx.set_last_error(format!(
            "relationship source handle {} is not registered",
            rel_handle.0
        ));
        return FfiResult::INVALID_COMPONENT;
    };
    let Some(tgt_meta) = ctx.registry.get_by_handle(tgt_handle).cloned() else {
        ctx.host_ctx.set_last_error(format!(
            "relationship target handle {} is not registered",
            tgt_handle.0
        ));
        return FfiResult::INVALID_COMPONENT;
    };
    if !rel_meta.is_dynamic
        || !tgt_meta.is_dynamic
        || rel_meta.is_resource()
        || rel_meta.is_message()
        || rel_meta.is_event()
        || tgt_meta.is_resource()
        || tgt_meta.is_message()
        || tgt_meta.is_event()
    {
        ctx.host_ctx.set_last_error(
            "dynamic relationship pairs require dynamic non-resource/non-message/non-event components",
        );
        return FfiResult::PERMISSION_DENIED;
    }
    if rel_meta.size != core::mem::size_of::<FfiEntity>()
        || rel_meta.align != core::mem::align_of::<FfiEntity>()
        || rel_meta.drop_fn.is_some()
    {
        ctx.host_ctx.set_last_error(format!(
            "relationship source '{}' must have 8-byte FfiEntity layout without drop_fn",
            rel_meta.name
        ));
        return FfiResult::TYPE_MISMATCH;
    }
    if tgt_meta.size != 0 || tgt_meta.drop_fn.is_some() {
        ctx.host_ctx.set_last_error(format!(
            "relationship target '{}' must be a zero-sized marker component without drop_fn",
            tgt_meta.name
        ));
        return FfiResult::TYPE_MISMATCH;
    }
    let rel_cid = rel_meta.component_id;
    let tgt_cid = tgt_meta.component_id;
    if rel_cid == tgt_cid {
        ctx.host_ctx
            .set_last_error("relationship source and target components must be distinct");
        return FfiResult::INVALID_ARGUMENT;
    }

    let world = ctx.world_mut();
    ensure_dynamic_hook_map_in_world(world);
    {
        let map = world.resource::<DynamicComponentHookMap>();
        if let Some(&existing) = map.rel_to_target.get(&rel_cid) {
            if existing.target_cid == tgt_cid
                && existing.allow_self_referential == allow_self_referential
            {
                // SAFETY: Caller guarantees `out_component` is non-null, aligned, and writable.
                unsafe {
                    *out_component = tgt_handle;
                }
                return FfiResult::OK;
            }
            ctx.host_ctx.set_last_error(format!(
                "relationship source '{}' is already bound with incompatible target/self-referential policy",
                rel_meta.name
            ));
            return FfiResult::TYPE_MISMATCH;
        }
        if map
            .target_to_rel
            .get(&tgt_cid)
            .is_some_and(|&existing_rel| existing_rel != rel_cid)
        {
            ctx.host_ctx.set_last_error(format!(
                "relationship target '{}' is already bound to a different source component",
                tgt_meta.name
            ));
            return FfiResult::TYPE_MISMATCH;
        }
    }

    if world
        .archetypes()
        .iter()
        .any(|a| a.contains(rel_cid) || a.contains(tgt_cid))
    {
        ctx.host_ctx.set_last_error(
            "cannot bind relationship hooks after source or target components exist in archetypes",
        );
        return FfiResult::INVALID_STATE;
    }

    if let Some(hooks) = world.register_component_hooks_by_id(rel_cid) {
        hooks.try_on_insert(dynamic_on_insert_hook);
        hooks.try_on_discard(dynamic_on_discard_hook);
    }
    if let Some(hooks) = world.register_component_hooks_by_id(tgt_cid) {
        hooks.try_on_despawn(dynamic_on_despawn_hook);
    }

    world
        .resource_mut::<DynamicComponentHookMap>()
        .bind_relationship_pair(rel_cid, tgt_cid, allow_self_referential);
    ctx.fresh_rel_bindings.push((rel_cid, tgt_cid));

    // SAFETY: Caller guarantees `out_component` is non-null, aligned, and writable.
    unsafe {
        *out_component = tgt_handle;
    }
    FfiResult::OK
}

/// Registers a dynamic component described by `desc_ptr` inside `load_ctx` and writes its 1-based
/// [`BamComponent`] handle into `*out_component`.
///
/// # Safety
/// - `load_ctx` must point to the active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`ComponentDesc`], or null.
/// - `out_component` must point to writable [`BamComponent`] storage, or null.
pub unsafe extern "C" fn host_register_component(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const ComponentDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_component.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = match unsafe { validate_active_load_ctx(load_ctx) } {
        Ok(c) => c,
        Err(err) => return err,
    };

    // SAFETY: `read_descriptor` checks null and `struct_size >= ComponentDesc::MIN_V0_2_SIZE`.
    let desc = match unsafe { read_descriptor::<ComponentDesc>(desc_ptr.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid ComponentDesc header ({err})"));
            return err;
        }
    };

    let val_status = desc.validate();
    if val_status.is_err() {
        ctx.host_ctx
            .set_last_error(format!("ComponentDesc validation failed ({val_status})"));
        return val_status;
    }

    let Some(layout) = desc.layout() else {
        ctx.host_ctx
            .set_last_error("ComponentDesc size/align violates C struct layout invariants");
        return FfiResult::INVALID_ARGUMENT;
    };

    // SAFETY: `desc.validate()` verified `!desc.name.is_null()`.
    let Ok(name_str) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
        ctx.host_ctx
            .set_last_error("ComponentDesc `name` is not valid UTF-8");
        return FfiResult::INVALID_UTF8;
    };
    if name_str.is_empty() {
        ctx.host_ctx
            .set_last_error("ComponentDesc `name` must not be empty");
        return FfiResult::INVALID_ARGUMENT;
    }

    // Prefix unqualified dynamic component names with `"<manifest.name>::<Name>"` unless
    // an unqualified entry of that exact name was already registered on the host.
    let canonical_name: std::borrow::Cow<'_, str> =
        if !name_str.contains("::") && ctx.registry.get_by_name(name_str).is_none() {
            if let Some(ref ns) = ctx.manifest_name {
                std::borrow::Cow::Owned(format!("{ns}::{name_str}"))
            } else {
                std::borrow::Cow::Borrowed(name_str)
            }
        } else {
            std::borrow::Cow::Borrowed(name_str)
        };
    let name_ref = canonical_name.as_ref();

    if let Some(pair_spec) = name_ref.strip_prefix(BAM_REL_BIND_SELF_PREFIX) {
        // SAFETY: `out_component` was verified non-null above and caller guarantees writable `BamComponent`.
        return unsafe {
            bind_dynamic_relationship_descriptor(ctx, pair_spec, true, &desc, out_component)
        };
    }
    if let Some(pair_spec) = name_ref.strip_prefix(BAM_REL_BIND_PREFIX) {
        // SAFETY: `out_component` was verified non-null above and caller guarantees writable `BamComponent`.
        return unsafe {
            bind_dynamic_relationship_descriptor(ctx, pair_spec, false, &desc, out_component)
        };
    }

    // Validate `required_components[0..required_component_count]` handles and default constructors.
    let mut required_handles = Vec::with_capacity(desc.required_component_count);
    let mut required_inits: Vec<ResolvedRequiredInit> =
        Vec::with_capacity(desc.required_component_count);
    if desc.required_component_count > 0 {
        // SAFETY: `desc.validate()` verified `!desc.required_components.is_null()` when count > 0.
        let req_slice = unsafe {
            core::slice::from_raw_parts(desc.required_components, desc.required_component_count)
        };
        for &req_handle in req_slice {
            let Some(req_meta) = ctx.registry.get_by_handle(req_handle) else {
                ctx.host_ctx.set_last_error(format!(
                    "ComponentDesc required component handle {} is not registered",
                    req_handle.0
                ));
                return FfiResult::INVALID_COMPONENT;
            };
            if req_meta.is_resource() || req_meta.is_message() || req_meta.is_event() {
                ctx.host_ctx.set_last_error(format!(
                    "ComponentDesc required handle '{}' must be a Component, not a Resource/Message/Event",
                    req_meta.name
                ));
                return FfiResult::INVALID_COMPONENT;
            }
            if req_meta.uuid == desc.uuid || req_meta.name.as_ref() == name_ref {
                ctx.host_ctx.set_last_error(format!(
                    "ComponentDesc '{}' cannot require itself",
                    name_ref
                ));
                return FfiResult::INVALID_ARGUMENT;
            }
            let has_default_ctor = req_meta.insert_default_fn.is_some()
                || (req_meta.is_abi_pod
                    && req_meta.drop_fn.is_none()
                    && req_meta.drop_slot.is_none()
                    && (req_meta.is_dynamic || req_meta.insert_raw_fn.is_some()));
            if !has_default_ctor {
                ctx.host_ctx.set_last_error(format!(
                    "ComponentDesc required component '{}' lacks a default or POD zero-init constructor",
                    req_meta.name
                ));
                return FfiResult::PERMISSION_DENIED;
            }
            if !required_handles.contains(&req_handle) {
                required_handles.push(req_handle);
                required_inits.push(ResolvedRequiredInit {
                    handle: req_handle,
                    strategy: RequiredInitStrategy::Default,
                });
            }
        }
    }

    // Validate ABI `0.3` `schema_fingerprint` / `fields` and `required_init_descs` upfront before
    // allocating a Bevy `ComponentDescriptor` or trampoline slot.
    // SAFETY: `desc.validate()` verified `!desc.fields.is_null()` when `desc.field_count > 0`.
    let (schema_fingerprint, extracted_fields, owned_field_strings) =
        match unsafe { extract_owned_dynamic_fields(&desc) } {
            Ok(v) => v,
            Err((err, msg)) => {
                ctx.host_ctx.set_last_error(msg);
                return err;
            }
        };

    if desc.covers_slot(
        core::mem::offset_of!(ComponentDesc, required_init_desc_count),
        core::mem::size_of::<usize>(),
    ) && desc.required_init_desc_count > 0
    {
        if desc.required_init_descs.is_null() {
            ctx.host_ctx
                .set_last_error("ComponentDesc `required_init_descs` is null with non-zero count");
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `desc.required_init_descs` is non-null and caller guarantees `desc.required_init_desc_count`
        // readable `BamRequiredComponent` entries.
        let req_init_slice = unsafe {
            core::slice::from_raw_parts(desc.required_init_descs, desc.required_init_desc_count)
        };
        for (idx, req_init) in req_init_slice.iter().enumerate() {
            if (req_init.struct_size as usize) < BamRequiredComponent::MIN_V0_3_SIZE {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] struct_size ({}) < MIN_V0_3_SIZE ({})",
                    req_init.struct_size,
                    BamRequiredComponent::MIN_V0_3_SIZE
                ));
                return FfiResult::INVALID_VERSION;
            }
            if (req_init.flags & !BAM_REQUIRED_KNOWN_FLAGS) != 0 {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] has unsupported flags 0x{:08X}",
                    req_init.flags
                ));
                return FfiResult::UNSUPPORTED;
            }
            if req_init.reserved0 != 0 {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] non-zero `reserved0` ({})",
                    req_init.reserved0
                ));
                return FfiResult::INVALID_ARGUMENT;
            }
            let has_custom_bytes = (req_init.flags & BAM_REQUIRED_FLAG_CUSTOM_BYTES) != 0;
            let has_init_fn = (req_init.flags & BAM_REQUIRED_FLAG_INIT_FN) != 0;
            if has_custom_bytes && has_init_fn {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] cannot combine CUSTOM_BYTES and INIT_FN"
                ));
                return FfiResult::INVALID_ARGUMENT;
            }
            if has_custom_bytes && req_init.init_size > 0 && req_init.init_bytes.is_null() {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] `init_bytes` is null with non-zero `init_size`"
                ));
                return FfiResult::NULL_POINTER;
            }
            if has_init_fn && req_init.init_fn.is_none() {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] `init_fn` is null with INIT_FN flag set"
                ));
                return FfiResult::NULL_POINTER;
            }

            let resolve_by_uuid = (req_init.flags & BAM_REQUIRED_FLAG_RESOLVE_BY_UUID) != 0;
            let req_meta = if resolve_by_uuid {
                if req_init.uuid == ComponentUuid::ZERO {
                    ctx.host_ctx.set_last_error(format!(
                        "BamRequiredComponent[{idx}] RESOLVE_BY_UUID set with zero UUID"
                    ));
                    return FfiResult::INVALID_ARGUMENT;
                }
                let Some(meta) = ctx.registry.get_by_uuid(&req_init.uuid) else {
                    ctx.host_ctx.set_last_error(format!(
                        "BamRequiredComponent[{idx}] required UUID {:?} is not registered",
                        req_init.uuid
                    ));
                    return FfiResult::NOT_FOUND;
                };
                meta
            } else {
                let Some(meta) = ctx.registry.get_by_handle(req_init.component) else {
                    ctx.host_ctx.set_last_error(format!(
                        "BamRequiredComponent[{idx}] required component handle {} is not registered",
                        req_init.component.0
                    ));
                    return FfiResult::INVALID_COMPONENT;
                };
                meta
            };

            if req_meta.is_resource() || req_meta.is_message() || req_meta.is_event() {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] target '{}' must be a Component, not a Resource/Message/Event",
                    req_meta.name
                ));
                return FfiResult::INVALID_COMPONENT;
            }
            if req_meta.uuid == desc.uuid || req_meta.name.as_ref() == name_ref {
                ctx.host_ctx.set_last_error(format!(
                    "ComponentDesc '{}' cannot require itself via required_init_descs[{idx}]",
                    name_ref
                ));
                return FfiResult::INVALID_ARGUMENT;
            }

            let strategy = if has_custom_bytes {
                if req_init.init_size != req_meta.size {
                    ctx.host_ctx.set_last_error(format!(
                        "BamRequiredComponent[{idx}] init_size ({}) != target '{}' size ({})",
                        req_init.init_size, req_meta.name, req_meta.size
                    ));
                    return FfiResult::TYPE_MISMATCH;
                }
                let can_raw_pod = req_meta.is_abi_pod
                    && req_meta.drop_fn.is_none()
                    && req_meta.drop_slot.is_none()
                    && (req_meta.is_dynamic || req_meta.insert_raw_fn.is_some());
                if !can_raw_pod {
                    ctx.host_ctx.set_last_error(format!(
                        "BamRequiredComponent[{idx}] CUSTOM_BYTES requires POD raw-insertable target '{}'",
                        req_meta.name
                    ));
                    return FfiResult::PERMISSION_DENIED;
                }
                let owned_bytes: Arc<[u8]> = if req_init.init_size == 0 {
                    Arc::from([])
                } else {
                    // SAFETY: `init_bytes` was verified non-null above and caller guarantees
                    // `init_size` readable initialized POD bytes for this registration call.
                    let slice = unsafe {
                        core::slice::from_raw_parts(req_init.init_bytes, req_init.init_size)
                    };
                    Arc::from(slice)
                };
                RequiredInitStrategy::CustomBytes(owned_bytes)
            } else if has_init_fn {
                if req_init.init_size != req_meta.size {
                    ctx.host_ctx.set_last_error(format!(
                        "BamRequiredComponent[{idx}] init_size ({}) != target '{}' size ({})",
                        req_init.init_size, req_meta.name, req_meta.size
                    ));
                    return FfiResult::TYPE_MISMATCH;
                }
                if !req_meta.is_dynamic && req_meta.insert_raw_fn.is_none() {
                    ctx.host_ctx.set_last_error(format!(
                        "BamRequiredComponent[{idx}] INIT_FN cannot write raw bytes into opaque host component '{}'",
                        req_meta.name
                    ));
                    return FfiResult::PERMISSION_DENIED;
                }
                let Some(cb) = req_init.init_fn else {
                    ctx.host_ctx.set_last_error(format!(
                        "BamRequiredComponent[{idx}] `init_fn` is null with INIT_FN flag set"
                    ));
                    return FfiResult::NULL_POINTER;
                };
                RequiredInitStrategy::InitFn {
                    init_fn: GuestFnPtr(cb),
                    user_data: GuestUserData::new(req_init.user_data),
                    is_active: Arc::clone(&ctx.is_active),
                }
            } else {
                if req_init.init_size != 0 && req_init.init_size != req_meta.size {
                    ctx.host_ctx.set_last_error(format!(
                        "BamRequiredComponent[{idx}] init_size ({}) != target '{}' size ({})",
                        req_init.init_size, req_meta.name, req_meta.size
                    ));
                    return FfiResult::TYPE_MISMATCH;
                }
                let has_default_ctor = req_meta.insert_default_fn.is_some()
                    || (req_meta.is_abi_pod
                        && req_meta.drop_fn.is_none()
                        && req_meta.drop_slot.is_none()
                        && (req_meta.is_dynamic || req_meta.insert_raw_fn.is_some()));
                if !has_default_ctor {
                    ctx.host_ctx.set_last_error(format!(
                        "BamRequiredComponent[{idx}] target '{}' lacks a default or POD zero-init constructor",
                        req_meta.name
                    ));
                    return FfiResult::PERMISSION_DENIED;
                }
                RequiredInitStrategy::Default
            };

            let target_handle = req_meta.handle;
            if let Some(existing_init) = required_inits
                .iter_mut()
                .find(|entry| entry.handle == target_handle)
            {
                existing_init.strategy = strategy;
            } else {
                required_handles.push(target_handle);
                required_inits.push(ResolvedRequiredInit {
                    handle: target_handle,
                    strategy,
                });
            }
        }
    }
    let required_boxed: Box<[BamComponent]> = required_handles.into_boxed_slice();
    let required_inits_arc: Arc<[ResolvedRequiredInit]> = Arc::from(required_inits);

    let storage_flags = component_flags_to_storage_flags(desc.flags);
    let existing_entry = ctx.registry.get_by_name(name_ref).cloned();
    let existing_comp_id = existing_entry.as_ref().map(|e| e.component_id);

    if let Err(collision) = ctx.registry.check_registration_collision(
        name_ref,
        desc.uuid,
        desc.size,
        desc.align,
        storage_flags,
        existing_comp_id,
    ) {
        ctx.host_ctx.set_last_error(collision.to_string());
        return FfiResult::TYPE_MISMATCH;
    }
    if let Some(ref existing) = existing_entry
        && existing.is_dynamic
        && existing.schema_fingerprint != 0
        && schema_fingerprint != 0
        && existing.schema_fingerprint != schema_fingerprint
    {
        ctx.host_ctx.set_last_error(format!(
            "ComponentDesc '{name_ref}' schema_fingerprint mismatch (existing=0x{:016x}, new=0x{:016x}); call migrate_component_column to evolve layout",
            existing.schema_fingerprint, schema_fingerprint
        ));
        return FfiResult::TYPE_MISMATCH;
    }
    if let Err(drop_mismatch) = ctx
        .registry
        .check_dynamic_drop_compatibility(name_ref, desc.drop_fn.is_some())
    {
        ctx.host_ctx.set_last_error(drop_mismatch.to_string());
        return FfiResult::TYPE_MISMATCH;
    }
    let mut desc_for_registry = desc;
    desc_for_registry.required_component_count = required_boxed.len();
    if let Err(hook_mismatch) = ctx
        .registry
        .check_dynamic_extended_hook_compatibility(name_ref, &desc_for_registry)
    {
        ctx.host_ctx.set_last_error(hook_mismatch.to_string());
        return FfiResult::TYPE_MISMATCH;
    }

    let is_active = Arc::clone(&ctx.is_active);
    let owner = ctx.mod_id;
    let staged_log = Arc::clone(&ctx.staged_drop_bindings);
    let has_any_dynamic_hook = desc.has_hooks() || !required_inits_arc.is_empty();

    let (comp_id, drop_slot, was_fresh_id): (ComponentId, Option<usize>, bool) = {
        let world = ctx.world_mut();
        world.init_resource::<WorldDropGuard>();

        let (slot_opt, trampoline_opt) = if let Some(drop_fn) = desc.drop_fn {
            let bound = world
                .get_resource_mut::<WorldDropGuard>()
                .and_then(|mut g| g.bind_or_update(desc.uuid, drop_fn, Arc::clone(&is_active)));
            let Some((slot, tramp, staged_binding)) = bound else {
                ctx.host_ctx.set_last_error(format!(
                    "exhausted MAX_DROP_TRAMPOLINES ({MAX_DROP_TRAMPOLINES}) foreign destructor slots"
                ));
                return FfiResult::CAPACITY_EXCEEDED;
            };
            lock_unpoison(&staged_log).push(staged_binding);
            (Some(slot), Some(tramp))
        } else {
            (None, None)
        };

        // Reuse an existing dynamic ComponentId ONLY when its permanently bound drop slot matches
        // `slot_opt`. If the old drop slot was recycled after unload and a different slot is now
        // bound, allocate a fresh `ComponentDescriptor` wired to the new `drop_trampoline::<SLOT>`.
        let reusable_comp_id = existing_entry.as_ref().and_then(|entry| {
            if !entry.is_dynamic || entry.drop_slot == slot_opt {
                Some(entry.component_id)
            } else {
                None
            }
        });

        if let Some(existing_id) = reusable_comp_id {
            (existing_id, slot_opt, false)
        } else {
            let storage_type = if desc.is_sparse_set() {
                StorageType::SparseSet
            } else {
                StorageType::Table
            };
            let clone_behavior = if desc.clone_fn.is_some() {
                ComponentCloneBehavior::Custom(dynamic_clone_runner)
            } else if desc.drop_fn.is_some() {
                ComponentCloneBehavior::Ignore
            } else {
                ComponentCloneBehavior::Default
            };
            // SAFETY: `layout` was verified by `desc.layout()`; `trampoline_opt` follows Bevy's
            // `OwningPtr` drop contract; `storage_type`, `is_summary_tick`, and mutability match `desc.flags`.
            let bevy_desc = unsafe {
                ComponentDescriptor::new_with_layout(
                    name_ref.to_string(),
                    storage_type,
                    layout,
                    trampoline_opt,
                    !desc.is_immutable(),
                    desc.is_summary_tick(),
                    clone_behavior,
                    None,
                )
            };
            let new_id = world.register_component_with_descriptor(bevy_desc);
            if (slot_opt.is_some() || has_any_dynamic_hook)
                && let Some(hooks) = world.register_component_hooks_by_id(new_id)
            {
                if desc.on_add.is_some() || !required_inits_arc.is_empty() {
                    hooks.on_add(dynamic_on_add_hook);
                }
                if desc.on_insert.is_some() {
                    hooks.on_insert(dynamic_on_insert_hook);
                } else if let Some(slot) = slot_opt {
                    hooks.on_insert(on_insert_hook_for_slot(slot));
                }
                if desc.on_discard.is_some() {
                    hooks.on_discard(dynamic_on_discard_hook);
                }
                if desc.on_remove.is_some() {
                    hooks.on_remove(dynamic_on_remove_hook);
                }
                if desc.on_despawn.is_some() {
                    hooks.on_despawn(dynamic_on_despawn_hook);
                }
            }
            (new_id, slot_opt, true)
        }
    };
    if was_fresh_id {
        ctx.fresh_dynamic_comp_ids.push(comp_id);
    }

    let handle = match ctx
        .registry
        .register_dynamic_component(DynamicComponentReg {
            comp_id,
            desc: &desc_for_registry,
            name: name_ref,
            owner,
            drop_slot,
            required_components: required_boxed,
            schema_fingerprint,
            fields: extracted_fields,
            owned_field_strings,
        }) {
        Ok(h) => h,
        Err(err) => {
            ctx.host_ctx.set_last_error(err.to_string());
            return FfiResult::TYPE_MISMATCH;
        }
    };

    if drop_slot.is_some() || has_any_dynamic_hook || desc.clone_fn.is_some() {
        let mut hook_binding = has_any_dynamic_hook.then(|| DynamicHookBinding {
            on_add: desc.on_add.map(GuestFnPtr),
            on_insert: desc.on_insert.map(GuestFnPtr),
            on_discard: desc.on_discard.map(GuestFnPtr),
            on_remove: desc.on_remove.map(GuestFnPtr),
            on_despawn: desc.on_despawn.map(GuestFnPtr),
            user_data: GuestUserData::new(desc.hook_user_data),
            is_active: Arc::clone(&ctx.is_active),
            required_inits: required_inits_arc,
            registry: ctx.registry.clone(),
        });
        let clone_entry = desc.clone_fn.map(|clone_fn| CloneBindingEntry {
            clone_fn: GuestFnPtr(clone_fn),
            user_data: GuestUserData::new(desc.hook_user_data),
            layout,
            is_active: Arc::clone(&ctx.is_active),
        });
        let world = ctx.world_mut();
        let mut map = world.get_resource_or_insert_with::<DynamicComponentHookMap>(
            DynamicComponentHookMap::default,
        );
        if let Some(slot) = drop_slot {
            map.set_drop_slot(comp_id, slot);
        }
        if let Some(binding) = hook_binding.take() {
            map.insert_or_update(comp_id, binding);
        }
        if let Some(entry) = clone_entry {
            map.insert_or_update_clone(comp_id, entry);
        }
    }

    ctx.host_ctx.set_snapshot(ctx.registry.snapshot_arc());
    if !ctx.world_ptr.is_null() {
        let updated_reg = ctx.registry.clone();
        ctx.world_mut().insert_resource(updated_reg);
    }
    if !ctx.registered_components.contains(&handle) {
        ctx.registered_components.push(handle);
    }

    // SAFETY: `out_component` was checked non-null above and caller guarantees writable `BamComponent`.
    unsafe {
        *out_component = handle;
    }
    FfiResult::OK
}

/// Parses an array of `count` NUL-terminated UTF-8 C strings and validates ordering target syntax.
///
/// # Safety
/// If `count > 0` and `ptr` is non-null, `ptr` must point to `count` readable `*const c_char` slots.
unsafe fn parse_ordering_targets(
    ptr: *const *const c_char,
    count: usize,
) -> Result<Vec<String>, (FfiResult, String)> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err((
            FfiResult::NULL_POINTER,
            "ordering array pointer is null with non-zero count".to_string(),
        ));
    }
    // SAFETY: Caller guarantees `count` readable `*const c_char` elements.
    let slice = unsafe { core::slice::from_raw_parts(ptr, count) };
    let mut out = Vec::with_capacity(count);
    for (idx, &entry) in slice.iter().enumerate() {
        if entry.is_null() {
            return Err((
                FfiResult::NULL_POINTER,
                format!("ordering target at index {idx} is null"),
            ));
        }
        // SAFETY: `entry` is non-null and points to a NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(entry) }).to_str() else {
            return Err((
                FfiResult::INVALID_UTF8,
                format!("ordering target at index {idx} is not valid UTF-8"),
            ));
        };
        if ModNamedSystemSet::parse_target(s, "mod").is_none() {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                format!("invalid ordering target syntax '{s}' at index {idx}"),
            ));
        }
        out.push(s.to_string());
    }
    Ok(out)
}

/// Parses and validates an array of `query_count` extensible [`BamQueryDesc`] records against
/// `snapshot`, returning owned [`BamQuerySpecOwned`] clauses.
///
/// # Safety
/// If `query_count > 0` and `queries` is non-null, `queries` must point to `query_count`
/// contiguous readable [`BamQueryDesc`] elements (each sized `first_size >= MIN_V0_2_SIZE`).
unsafe fn parse_and_validate_query_descs(
    queries: *const BamQueryDesc,
    query_count: usize,
    snapshot: &RegistrySnapshot,
) -> Result<Vec<BamQuerySpecOwned>, (FfiResult, String)> {
    if query_count == 0 {
        return Ok(Vec::new());
    }
    if query_count > MAX_QUERIES_PER_SYSTEM {
        return Err((
            FfiResult::CAPACITY_EXCEEDED,
            format!(
                "query_count ({query_count}) exceeds MAX_QUERIES_PER_SYSTEM ({MAX_QUERIES_PER_SYSTEM})"
            ),
        ));
    }
    if queries.is_null() {
        return Err((
            FfiResult::NULL_POINTER,
            "`queries` pointer is null with non-zero `query_count`".to_string(),
        ));
    }

    let mut query_specs = Vec::with_capacity(query_count);
    let base_ptr = queries.cast::<u8>();
    // Determine element stride from the first `BamQueryDesc::struct_size` so forward-extended
    // query arrays step accurately.
    // SAFETY: Verified `!queries.is_null()` and `query_count > 0` above.
    let first_size = unsafe { core::ptr::read_unaligned(base_ptr.cast::<u32>()) } as usize;
    if first_size < BamQueryDesc::MIN_V0_2_SIZE {
        return Err((
            FfiResult::INVALID_VERSION,
            format!("BamQueryDesc[0] struct_size ({first_size}) < MIN_V0_2_SIZE"),
        ));
    }

    for q_idx in 0..query_count {
        let Some(byte_offset) = q_idx.checked_mul(first_size) else {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                format!("BamQueryDesc[{q_idx}] byte offset overflow"),
            ));
        };
        // SAFETY: Caller guarantees `queries` holds `query_count` contiguous elements.
        let q_ptr = unsafe { base_ptr.add(byte_offset).cast::<c_void>() };
        // SAFETY: Reading extensible `BamQueryDesc` element `q_idx`.
        let q_desc = unsafe { read_descriptor::<BamQueryDesc>(q_ptr) }
            .map_err(|err| (err, format!("invalid BamQueryDesc[{q_idx}] ({err})")))?;
        let q_val = q_desc.validate();
        if q_val.is_err() {
            return Err((
                q_val,
                format!("BamQueryDesc[{q_idx}] validation failed ({q_val})"),
            ));
        }

        if (q_desc.flags & BAM_QUERY_FLAG_PARAM_SET) != 0 && q_desc.param_set_group == 0 {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                format!(
                    "BamQueryDesc[{q_idx}] sets BAM_QUERY_FLAG_PARAM_SET with param_set_group == 0"
                ),
            ));
        }
        let param_set_group =
            if (q_desc.flags & BAM_QUERY_FLAG_PARAM_SET) != 0 || q_desc.param_set_group != 0 {
                q_desc.param_set_group
            } else {
                0
            };

        let mut terms = Vec::with_capacity(q_desc.term_count);
        for t_idx in 0..q_desc.term_count {
            // SAFETY: `q_desc.validate()` verified `!q_desc.terms.is_null()` when `term_count > 0`.
            let term: BamTermDesc = unsafe { core::ptr::read_unaligned(q_desc.terms.add(t_idx)) };
            terms.push(term);
        }
        query_specs.push(BamQuerySpecOwned {
            flags: q_desc.flags,
            param_set_group,
            terms,
        });
    }

    validate_system_queries_against_snapshot(snapshot, &query_specs)?;
    Ok(query_specs)
}

/// Parses and validates a read-only `run_if_query` descriptor against `snapshot`.
///
/// # Safety
/// - `query_ptr` must point to a readable [`BamQueryDesc`] whose `terms` array (if
///   `term_count > 0`) contains `term_count` readable [`BamTermDesc`] records.
pub(crate) unsafe fn parse_and_validate_run_if_query(
    query_ptr: *const BamQueryDesc,
    snapshot: &RegistrySnapshot,
) -> Result<BamQuerySpecOwned, (FfiResult, String)> {
    // SAFETY: Caller guarantees `query_ptr` points to a readable `BamQueryDesc`.
    let q_desc = unsafe { read_descriptor::<BamQueryDesc>(query_ptr.cast::<c_void>()) }
        .map_err(|err| (err, format!("invalid `run_if_query` ({err})")))?;
    let q_val = q_desc.validate();
    if q_val.is_err() {
        return Err((q_val, format!("`run_if_query` validation failed ({q_val})")));
    }
    if (q_desc.flags & BAM_QUERY_FLAG_PARAM_SET) != 0 || q_desc.param_set_group != 0 {
        return Err((
            FfiResult::INVALID_ARGUMENT,
            "`run_if_query` cannot belong to a param_set_group".to_string(),
        ));
    }
    let mut terms = Vec::with_capacity(q_desc.term_count);
    for t_idx in 0..q_desc.term_count {
        // SAFETY: `q_desc.validate()` verified `!q_desc.terms.is_null()` when `term_count > 0`.
        let term: BamTermDesc = unsafe { core::ptr::read_unaligned(q_desc.terms.add(t_idx)) };
        if term.mode.is_mutable() {
            return Err((
                FfiResult::PERMISSION_DENIED,
                format!(
                    "`run_if_query` term {t_idx} must be read-only (got {:?})",
                    term.mode
                ),
            ));
        }
        terms.push(term);
    }
    let spec = BamQuerySpecOwned {
        flags: q_desc.flags,
        param_set_group: 0,
        terms,
    };
    validate_system_queries_against_snapshot(snapshot, core::slice::from_ref(&spec))?;
    Ok(spec)
}

/// Registers a unified [`BamSystemDesc`] inside `load_ctx`, returning its 1-based [`BamSystem`] handle.
///
/// # Safety
/// - `load_ctx` must point to the active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`BamSystemDesc`], or null.
pub(crate) unsafe fn register_system_inner(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const BamSystemDesc,
) -> Result<BamSystem, FfiResult> {
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = unsafe { validate_active_load_ctx(load_ctx) }?;

    // SAFETY: `read_descriptor` checks null and `struct_size >= BamSystemDesc::MIN_V0_2_SIZE`.
    let desc = match unsafe { read_descriptor::<BamSystemDesc>(desc_ptr.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid BamSystemDesc header ({err})"));
            return Err(err);
        }
    };

    let val_status = desc.validate();
    if val_status.is_err() {
        ctx.host_ctx
            .set_last_error(format!("BamSystemDesc validation failed ({val_status})"));
        return Err(val_status);
    }

    let Some(callback) = desc.callback else {
        ctx.host_ctx
            .set_last_error("BamSystemDesc `callback` is null");
        return Err(FfiResult::NULL_POINTER);
    };

    // SAFETY: `desc.validate()` verified `!desc.name.is_null()`.
    let Ok(sys_name) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
        ctx.host_ctx
            .set_last_error("BamSystemDesc `name` is not valid UTF-8");
        return Err(FfiResult::INVALID_UTF8);
    };
    if sys_name.is_empty() {
        ctx.host_ctx
            .set_last_error("BamSystemDesc `name` must not be empty");
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    if !ctx.allows_system(sys_name) {
        ctx.host_ctx.set_last_error(format!(
            "system '{sys_name}' is not permitted by LoadContext system filter"
        ));
        return Err(FfiResult::PERMISSION_DENIED);
    }

    // Parse `queries[0..desc.query_count]`.
    // SAFETY: `desc.validate()` verified `!desc.queries.is_null()` when `desc.query_count > 0`.
    let query_specs = match unsafe {
        parse_and_validate_query_descs(desc.queries, desc.query_count, ctx.registry.snapshot())
    } {
        Ok(specs) => specs,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return Err(status);
        }
    };

    if desc.run_if_ctx.is_none() && !desc.run_if_query.is_null() {
        ctx.host_ctx
            .set_last_error("BamSystemDesc `run_if_query` requires non-null `run_if_ctx`");
        return Err(FfiResult::INVALID_ARGUMENT);
    }

    let run_if_ctx = if let Some(run_if_ctx_fn) = desc.run_if_ctx {
        let cond_spec = if !desc.run_if_query.is_null() {
            // SAFETY: `desc.run_if_query` is non-null and caller guarantees readable `BamQueryDesc`.
            match unsafe {
                parse_and_validate_run_if_query(desc.run_if_query, ctx.registry.snapshot())
            } {
                Ok(spec) => spec,
                Err((err, msg)) => {
                    ctx.host_ctx.set_last_error(msg);
                    return Err(err);
                }
            }
        } else {
            BamQuerySpecOwned::default()
        };
        Some((run_if_ctx_fn, cond_spec, desc.run_if_ctx_user_data))
    } else {
        None
    };

    // SAFETY: `desc.validate()` verified `after` and `before` non-null when their counts > 0.
    let after_systems = match unsafe { parse_ordering_targets(desc.after, desc.after_count) } {
        Ok(v) => v,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return Err(status);
        }
    };
    // SAFETY: Same guarantee for `desc.before[0..desc.before_count]`.
    let before_systems = match unsafe { parse_ordering_targets(desc.before, desc.before_count) } {
        Ok(v) => v,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return Err(status);
        }
    };
    // SAFETY: `desc.validate()` verified `in_sets` non-null when `in_set_count > 0`.
    let in_sets = match unsafe { parse_ordering_targets(desc.in_sets, desc.in_set_count) } {
        Ok(v) => v,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return Err(status);
        }
    };
    // SAFETY: `desc.validate()` verified `after_weak` is non-null when `after_weak_count > 0`.
    let after_weak = match unsafe { parse_ordering_targets(desc.after_weak, desc.after_weak_count) }
    {
        Ok(v) => v,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return Err(status);
        }
    };
    // SAFETY: `desc.validate()` verified `before_weak` is non-null when `before_weak_count > 0`.
    let before_weak =
        match unsafe { parse_ordering_targets(desc.before_weak, desc.before_weak_count) } {
            Ok(v) => v,
            Err((status, msg)) => {
                ctx.host_ctx.set_last_error(msg);
                return Err(status);
            }
        };
    // SAFETY: `desc.validate()` verified `ambiguous_with` is non-null when `ambiguous_with_count > 0`.
    let ambiguous_with =
        match unsafe { parse_ordering_targets(desc.ambiguous_with, desc.ambiguous_with_count) } {
            Ok(v) => v,
            Err((status, msg)) => {
                ctx.host_ctx.set_last_error(msg);
                return Err(status);
            }
        };

    let run_if = desc.run_if.map(|cb| (cb, desc.run_if_user_data));

    let mut sys = GuestSystem::new(
        sys_name,
        query_specs,
        callback,
        desc.user_data,
        ctx.registry.clone(),
    )
    .with_schedule(desc.schedule)
    .with_main_thread(desc.is_main_thread())
    .with_ordering(after_systems, before_systems)
    .with_weak_ordering(after_weak, before_weak)
    .with_in_sets(in_sets)
    .with_ambiguous_with(ambiguous_with)
    .with_ambiguous_with_all(desc.is_ambiguous_with_all())
    .with_run_if(run_if)
    .with_run_if_ctx(run_if_ctx)
    .with_is_active(Arc::clone(&ctx.is_active));

    if let Some(id) = ctx.mod_id {
        sys.set_mod_id(id);
    }

    let next_raw = u32::try_from(ctx.registered_systems.len() + 1)
        .map_err(|_| FfiResult::CAPACITY_EXCEEDED)?;
    ctx.registered_systems.push(sys);
    Ok(BamSystem::from_raw(next_raw))
}

/// Registers a unified [`BamSystemDesc`] inside `load_ctx`.
///
/// # Safety
/// - `load_ctx` must point to the active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`BamSystemDesc`], or null.
pub unsafe extern "C" fn host_register_system(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const BamSystemDesc,
) -> FfiResult {
    // SAFETY: Caller upholds `load_ctx` and `desc_ptr` FFI contract.
    match unsafe { register_system_inner(load_ctx, desc_ptr) } {
        Ok(_) => FfiResult::OK,
        Err(err) => err,
    }
}

/// Resolves a validated [`BamObserverDesc`] into a [`GuestObserverBinding`].
///
/// # Safety
/// - If `desc.name` is non-null, it must point to a valid NUL-terminated C string.
/// - If `desc.watched_component_count > 0`, `desc.watched_components` must point to
///   `desc.watched_component_count` readable [`BamComponent`] handles.
pub(crate) unsafe fn build_observer_binding_from_desc(
    desc: &BamObserverDesc,
    snapshot: &RegistrySnapshot,
    registry: ModRegistry,
    is_active: Arc<AtomicBool>,
    mod_id: Option<ModId>,
) -> Result<GuestObserverBinding, (FfiResult, String)> {
    let obs_name: Arc<str> = if desc.name.is_null() {
        Arc::from("<unnamed_observer>")
    } else {
        // SAFETY: Caller guarantees `desc.name` points to a NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
            return Err((
                FfiResult::INVALID_UTF8,
                "BamObserverDesc `name` is not valid UTF-8".to_string(),
            ));
        };
        if s.is_empty() {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                "BamObserverDesc `name` must not be empty".to_string(),
            ));
        }
        Arc::from(s)
    };

    if desc.callback.is_none()
        && desc.mut_callback.is_none()
        && desc.ctx_callback.is_none()
        && desc.ctx_mut_callback.is_none()
    {
        return Err((
            FfiResult::NULL_POINTER,
            "BamObserverDesc requires `callback`, `mut_callback`, `ctx_callback`, or `ctx_mut_callback`"
                .to_string(),
        ));
    }
    if desc.query_count > MAX_QUERIES_PER_SYSTEM {
        return Err((
            FfiResult::CAPACITY_EXCEEDED,
            format!(
                "BamObserverDesc `query_count` ({}) exceeds MAX_QUERIES_PER_SYSTEM ({MAX_QUERIES_PER_SYSTEM})",
                desc.query_count
            ),
        ));
    }
    if desc.query_count > 0 && desc.queries.is_null() {
        return Err((
            FfiResult::NULL_POINTER,
            "BamObserverDesc `queries` is null with non-zero `query_count`".to_string(),
        ));
    }
    if desc.query_count > 0 && desc.ctx_callback.is_none() && desc.ctx_mut_callback.is_none() {
        return Err((
            FfiResult::INVALID_ARGUMENT,
            "BamObserverDesc `queries` requires non-null `ctx_callback` or `ctx_mut_callback`"
                .to_string(),
        ));
    }
    // SAFETY: Verified non-null `desc.queries` when `desc.query_count > 0`.
    let query_specs =
        unsafe { parse_and_validate_query_descs(desc.queries, desc.query_count, snapshot) }?;

    let target_entity = decode_optional_entity(desc.entity()).map_err(|status| {
        (
            status,
            "BamObserverDesc target entity is invalid or Entity::PLACEHOLDER".to_string(),
        )
    })?;

    let propagate_relationship = if desc.propagate_relationship.is_valid() {
        let Some(rel_meta) = snapshot.get_by_handle(desc.propagate_relationship) else {
            return Err((
                FfiResult::INVALID_COMPONENT,
                "BamObserverDesc `propagate_relationship` handle is not registered".to_string(),
            ));
        };
        if rel_meta.is_resource() || rel_meta.is_message() {
            return Err((
                FfiResult::INVALID_COMPONENT,
                "BamObserverDesc `propagate_relationship` cannot be a Resource or Message"
                    .to_string(),
            ));
        }
        Some(rel_meta.component_id)
    } else {
        None
    };

    let mut watched_components = Vec::with_capacity(desc.watched_component_count + 1);
    if desc.watched_component_count > 0 {
        if desc.watched_components.is_null() {
            return Err((
                FfiResult::NULL_POINTER,
                "BamObserverDesc `watched_components` is null with non-zero count".to_string(),
            ));
        }
        // SAFETY: Caller guarantees `watched_components` has `watched_component_count` readable items.
        let handles = unsafe {
            core::slice::from_raw_parts(desc.watched_components, desc.watched_component_count)
        };
        for &handle in handles {
            let Some(comp_meta) = snapshot.get_by_handle(handle) else {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    format!(
                        "BamObserverDesc watched component handle {} is not registered",
                        handle.0
                    ),
                ));
            };
            if comp_meta.is_resource() || comp_meta.is_message() {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    format!(
                        "BamObserverDesc watched handle '{}' cannot be a Resource or Message",
                        comp_meta.name
                    ),
                ));
            }
            if !watched_components.contains(&comp_meta.component_id) {
                watched_components.push(comp_meta.component_id);
            }
        }
    }

    let (event_id, event_key, event_size, synthesizes_ffi_entity_payload, extract_target_fn) =
        if desc.event_kind.is_custom() {
            let Some(meta) = snapshot.get_by_handle(desc.event) else {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    "BamObserverDesc `event` handle is not registered".to_string(),
                ));
            };
            if meta.is_resource() || meta.is_message() {
                return Err((
                FfiResult::INVALID_COMPONENT,
                "BamObserverDesc `event` must refer to an Event, EntityEvent, or component, not a Resource or Message"
                    .to_string(),
            ));
            }
            let key = meta.event_key.unwrap_or_else(|| {
                // SAFETY: `meta.component_id` is valid in `world` and matches `meta`'s registered layout.
                unsafe { EventKey::new(meta.component_id) }
            });
            (
                Some(meta.component_id),
                key,
                meta.size,
                meta.synthesizes_ffi_entity_payload,
                meta.extract_event_target_fn,
            )
        } else if desc.event_kind.is_component_lifecycle() {
            if desc.event.is_valid() {
                let Some(comp_meta) = snapshot.get_by_handle(desc.event) else {
                    return Err((
                        FfiResult::INVALID_COMPONENT,
                        "BamObserverDesc lifecycle `event` component handle is not registered"
                            .to_string(),
                    ));
                };
                if comp_meta.is_resource() || comp_meta.is_message() {
                    return Err((
                    FfiResult::INVALID_COMPONENT,
                    "BamObserverDesc lifecycle `event` component cannot be a Resource or Message"
                        .to_string(),
                ));
                }
                if !watched_components.contains(&comp_meta.component_id) {
                    watched_components.push(comp_meta.component_id);
                }
            }
            if watched_components.is_empty() {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    "BamObserverDesc lifecycle observer requires at least one watched component"
                        .to_string(),
                ));
            }
            let key = match desc.event_kind {
                BamEventKind::ADD => bevy_ecs::lifecycle::ADD,
                BamEventKind::INSERT => bevy_ecs::lifecycle::INSERT,
                BamEventKind::DISCARD => bevy_ecs::lifecycle::DISCARD,
                BamEventKind::REMOVE => bevy_ecs::lifecycle::REMOVE,
                BamEventKind::DESPAWN => bevy_ecs::lifecycle::DESPAWN,
                other => {
                    return Err((
                        FfiResult::INVALID_ARGUMENT,
                        format!("unsupported lifecycle BamEventKind({})", other.0),
                    ));
                }
            };
            (None, key, core::mem::size_of::<FfiEntity>(), true, None)
        } else if desc.event_kind.is_ready() {
            let meta = if desc.event.is_valid() {
                let Some(m) = snapshot.get_by_handle(desc.event) else {
                    return Err((
                        FfiResult::INVALID_COMPONENT,
                        "BamObserverDesc READY `event` handle is not registered".to_string(),
                    ));
                };
                m
            } else if let Some(m) = snapshot
                .get_by_name("bevy_scene::Ready")
                .or_else(|| snapshot.get_by_name("Ready"))
            {
                m
            } else {
                return Err((
                    FfiResult::NOT_FOUND,
                    "BamObserverDesc READY event ('bevy_scene::Ready' / 'Ready') is not registered"
                        .to_string(),
                ));
            };
            if meta.is_resource() || meta.is_message() {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    "BamObserverDesc READY `event` cannot be a Resource or Message".to_string(),
                ));
            }
            let key = meta.event_key.unwrap_or_else(|| {
                // SAFETY: `meta.component_id` is valid in `world` and matches `meta`'s registered layout.
                unsafe { EventKey::new(meta.component_id) }
            });
            (
                Some(meta.component_id),
                key,
                meta.size,
                true,
                meta.extract_event_target_fn,
            )
        } else {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                format!("unsupported BamEventKind({})", desc.event_kind.0),
            ));
        };

    Ok(GuestObserverBinding {
        name: obs_name,
        event: desc.event,
        event_kind: desc.event_kind,
        event_id,
        event_key,
        event_size,
        synthesizes_ffi_entity_payload,
        target_entity,
        watched_components: watched_components.into_boxed_slice(),
        extract_target_fn,
        callback: desc.callback.map(GuestFnPtr),
        mut_callback: desc.mut_callback.map(GuestFnPtr),
        ctx_callback: desc.ctx_callback.map(GuestFnPtr),
        ctx_mut_callback: desc.ctx_mut_callback.map(GuestFnPtr),
        query_specs,
        is_propagating: desc.is_propagating(),
        propagate_relationship,
        user_data: GuestUserData::new(desc.user_data),
        is_active,
        registry,
        mod_id,
    })
}

/// Registers a dynamic Bevy ECS [`Observer`](bevy_ecs::observer::Observer) from a [`BamObserverDesc`]
/// during `bamabi_init`.
///
/// # Safety
/// - `load_ctx` must point to an active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc` must point to a valid aligned [`BamObserverDesc`] whose `name` (if non-null) is NUL-terminated UTF-8.
pub unsafe extern "C" fn host_register_observer(
    load_ctx: *mut BamLoadCtx,
    desc: *const BamObserverDesc,
) -> FfiResult {
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = ffi_try!(unsafe { validate_active_load_ctx(load_ctx) });
    ctx.host_ctx.clear_last_error();

    if desc.is_null() {
        ctx.host_ctx
            .set_last_error("host_register_observer received null BamObserverDesc pointer");
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: `desc` is non-null; `read_descriptor` checks alignment and `struct_size >= MIN_V0_2_SIZE`.
    let desc = match unsafe { read_descriptor::<BamObserverDesc>(desc.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid BamObserverDesc descriptor ({err})"));
            return err;
        }
    };

    let validation = desc.validate();
    if validation.is_err() {
        ctx.host_ctx
            .set_last_error(format!("BamObserverDesc validation failed ({validation})"));
        return validation;
    }

    // SAFETY: `desc` passed validation; `name` and `watched_components` satisfy caller contract.
    let binding = match unsafe {
        build_observer_binding_from_desc(
            &desc,
            ctx.registry.snapshot(),
            ctx.registry.clone(),
            Arc::clone(&ctx.is_active),
            ctx.mod_id,
        )
    } {
        Ok(b) => b,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return status;
        }
    };

    ctx.registered_observers.push(binding);
    FfiResult::OK
}

struct ParsedNonComponentDesc {
    desc: ComponentDesc,
    layout: Layout,
    canonical_name: String,
    schema_fingerprint: u64,
    fields: Arc<[BamFieldDesc]>,
    owned_field_strings: Option<OwnedFieldStrings>,
}

/// Parses and validates a [`ComponentDesc`] for `host_register_resource` or `host_register_message`.
///
/// # Safety
/// - `desc_ptr` must point to a readable [`ComponentDesc`] or be null.
unsafe fn parse_dynamic_non_component_desc(
    ctx: &mut LoadContext,
    desc_ptr: *const ComponentDesc,
    kind_label: &str,
) -> Result<ParsedNonComponentDesc, FfiResult> {
    // SAFETY: `read_descriptor` validates null, alignment, and `struct_size >= MIN_V0_2_SIZE`.
    let desc = match unsafe { read_descriptor::<ComponentDesc>(desc_ptr.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid {kind_label} ComponentDesc header ({err})"));
            return Err(err);
        }
    };

    let val_status = desc.validate();
    if val_status.is_err() {
        ctx.host_ctx.set_last_error(format!(
            "{kind_label} ComponentDesc validation failed ({val_status})"
        ));
        return Err(val_status);
    }

    let Some(layout) = desc.layout() else {
        ctx.host_ctx.set_last_error(format!(
            "{kind_label} ComponentDesc size/align violates C struct layout invariants"
        ));
        return Err(FfiResult::INVALID_ARGUMENT);
    };

    // SAFETY: `desc.validate()` verified `!desc.name.is_null()`.
    let Ok(name_str) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
        ctx.host_ctx.set_last_error(format!(
            "{kind_label} ComponentDesc `name` is not valid UTF-8"
        ));
        return Err(FfiResult::INVALID_UTF8);
    };
    if name_str.is_empty() {
        ctx.host_ctx.set_last_error(format!(
            "{kind_label} ComponentDesc `name` must not be empty"
        ));
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    if name_str.starts_with(BAM_REL_BIND_PREFIX) || name_str.starts_with(BAM_REL_BIND_SELF_PREFIX) {
        ctx.host_ctx.set_last_error(format!(
            "{kind_label} ComponentDesc `name` '{name_str}' cannot use relationship binding prefix"
        ));
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    if desc.required_component_count > 0
        || (desc.covers_slot(
            core::mem::offset_of!(ComponentDesc, required_init_desc_count),
            core::mem::size_of::<usize>(),
        ) && desc.required_init_desc_count > 0)
        || desc.has_hooks()
        || desc.clone_fn.is_some()
    {
        ctx.host_ctx.set_last_error(format!(
            "{kind_label} '{name_str}' does not support required components, lifecycle hooks, or clone_fn"
        ));
        return Err(FfiResult::UNSUPPORTED);
    }

    let canonical_name = if !name_str.contains("::") && ctx.registry.get_by_name(name_str).is_none()
    {
        if let Some(ref ns) = ctx.manifest_name {
            format!("{ns}::{name_str}")
        } else {
            name_str.to_owned()
        }
    } else {
        name_str.to_owned()
    };

    // SAFETY: `desc.validate()` verified `!desc.fields.is_null()` when `desc.field_count > 0`.
    let (schema_fingerprint, fields, owned_field_strings) =
        match unsafe { extract_owned_dynamic_fields(&desc) } {
            Ok(v) => v,
            Err((err, msg)) => {
                ctx.host_ctx.set_last_error(msg);
                return Err(err);
            }
        };

    Ok(ParsedNonComponentDesc {
        desc,
        layout,
        canonical_name,
        schema_fingerprint,
        fields,
        owned_field_strings,
    })
}

/// Registers a dynamic singleton resource described by `desc_ptr` inside `load_ctx` and writes
/// its 1-based [`BamComponent`] handle (`BAM_STORAGE_FLAG_RESOURCE`) into `*out_component`.
///
/// # Safety
/// - `load_ctx` must point to the active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`ComponentDesc`], or null.
/// - `out_component` must point to writable [`BamComponent`] storage, or null.
pub unsafe extern "C" fn host_register_resource(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const ComponentDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_component.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = match unsafe { validate_active_load_ctx(load_ctx) } {
        Ok(c) => c,
        Err(err) => return err,
    };
    ctx.host_ctx.clear_last_error();

    // SAFETY: `desc_ptr` validity is upheld by the FFI caller.
    let parsed = match unsafe { parse_dynamic_non_component_desc(ctx, desc_ptr, "Resource") } {
        Ok(p) => p,
        Err(err) => return err,
    };
    let name_ref = parsed.canonical_name.as_str();
    let storage_flags =
        component_flags_to_storage_flags(parsed.desc.flags) | BAM_STORAGE_FLAG_RESOURCE;
    let existing_entry = ctx.registry.get_by_name(name_ref).cloned();
    let existing_comp_id = existing_entry.as_ref().map(|e| e.component_id);

    if let Err(collision) = ctx.registry.check_registration_collision(
        name_ref,
        parsed.desc.uuid,
        parsed.desc.size,
        parsed.desc.align,
        storage_flags,
        existing_comp_id,
    ) {
        ctx.host_ctx.set_last_error(collision.to_string());
        return FfiResult::TYPE_MISMATCH;
    }
    if let Some(ref existing) = existing_entry
        && existing.is_dynamic
        && existing.schema_fingerprint != 0
        && parsed.schema_fingerprint != 0
        && existing.schema_fingerprint != parsed.schema_fingerprint
    {
        ctx.host_ctx.set_last_error(format!(
            "Resource ComponentDesc '{name_ref}' schema_fingerprint mismatch (existing=0x{:016x}, new=0x{:016x})",
            existing.schema_fingerprint, parsed.schema_fingerprint
        ));
        return FfiResult::TYPE_MISMATCH;
    }
    if let Err(drop_mismatch) = ctx
        .registry
        .check_dynamic_drop_compatibility(name_ref, parsed.desc.drop_fn.is_some())
    {
        ctx.host_ctx.set_last_error(drop_mismatch.to_string());
        return FfiResult::TYPE_MISMATCH;
    }

    let is_active = Arc::clone(&ctx.is_active);
    let owner = ctx.mod_id;
    let staged_log = Arc::clone(&ctx.staged_drop_bindings);

    let (comp_id, drop_slot, was_fresh_id): (ComponentId, Option<usize>, bool) = {
        let world = ctx.world_mut();
        world.init_resource::<WorldDropGuard>();

        let (slot_opt, trampoline_opt) = if let Some(drop_fn) = parsed.desc.drop_fn {
            let bound = world
                .get_resource_mut::<WorldDropGuard>()
                .and_then(|mut g| {
                    g.bind_or_update(parsed.desc.uuid, drop_fn, Arc::clone(&is_active))
                });
            let Some((slot, tramp, staged_binding)) = bound else {
                ctx.host_ctx.set_last_error(format!(
                    "exhausted MAX_DROP_TRAMPOLINES ({MAX_DROP_TRAMPOLINES}) foreign destructor slots"
                ));
                return FfiResult::CAPACITY_EXCEEDED;
            };
            lock_unpoison(&staged_log).push(staged_binding);
            (Some(slot), Some(tramp))
        } else {
            (None, None)
        };

        let reusable_comp_id = existing_entry.as_ref().and_then(|entry| {
            if !entry.is_dynamic || entry.drop_slot == slot_opt {
                Some(entry.component_id)
            } else {
                None
            }
        });

        if let Some(existing_id) = reusable_comp_id {
            (existing_id, slot_opt, false)
        } else {
            // SAFETY: `parsed.layout` was verified by `ComponentDesc::layout()`; `trampoline_opt`
            // satisfies Bevy's `OwningPtr` drop contract.
            let bevy_desc = unsafe {
                ComponentDescriptor::new_with_layout(
                    format!("bam::dynamic_res::{name_ref}"),
                    StorageType::Table,
                    parsed.layout,
                    trampoline_opt,
                    !parsed.desc.is_immutable(),
                    parsed.desc.is_summary_tick(),
                    ComponentCloneBehavior::Ignore,
                    None,
                )
            };
            let new_id = world.register_component_with_descriptor(bevy_desc);
            if let Some(slot) = slot_opt
                && let Some(hooks) = world.register_component_hooks_by_id(new_id)
            {
                hooks.on_insert(on_insert_hook_for_slot(slot));
            }
            (new_id, slot_opt, true)
        }
    };
    if was_fresh_id {
        ctx.fresh_dynamic_comp_ids.push(comp_id);
    }

    let handle = match ctx
        .registry
        .register_resource_from_desc(DynamicResourceReg {
            comp_id,
            desc: &parsed.desc,
            name: name_ref,
            owner,
            drop_slot,
            schema_fingerprint: parsed.schema_fingerprint,
            fields: parsed.fields,
            owned_field_strings: parsed.owned_field_strings,
        }) {
        Ok(h) => h,
        Err(err) => {
            ctx.host_ctx.set_last_error(err.to_string());
            return FfiResult::TYPE_MISMATCH;
        }
    };

    if !ctx.world_ptr.is_null() {
        let world = ctx.world_mut();
        if world.resource_entities().get(comp_id).is_none() {
            world.spawn(bevy_ecs::resource::IsResource::new(comp_id));
        }
        if let Some(slot) = drop_slot {
            let mut map = world.get_resource_or_insert_with::<DynamicComponentHookMap>(
                DynamicComponentHookMap::default,
            );
            map.set_drop_slot(comp_id, slot);
        }
    }

    ctx.host_ctx.set_snapshot(ctx.registry.snapshot_arc());
    if !ctx.world_ptr.is_null() {
        let updated_reg = ctx.registry.clone();
        ctx.world_mut().insert_resource(updated_reg);
    }
    if !ctx.registered_components.contains(&handle) {
        ctx.registered_components.push(handle);
    }

    // SAFETY: `out_component` was verified non-null and caller guarantees writable storage.
    unsafe {
        *out_component = handle;
    }
    FfiResult::OK
}

/// Registers a dynamic buffered message channel described by `desc_ptr` inside `load_ctx` and
/// writes its 1-based [`BamComponent`] handle (`BAM_STORAGE_FLAG_MESSAGE`) into `*out_component`.
///
/// # Safety
/// - `load_ctx` must point to the active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`ComponentDesc`], or null.
/// - `out_component` must point to writable [`BamComponent`] storage, or null.
pub unsafe extern "C" fn host_register_message(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const ComponentDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_component.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = match unsafe { validate_active_load_ctx(load_ctx) } {
        Ok(c) => c,
        Err(err) => return err,
    };
    ctx.host_ctx.clear_last_error();

    // SAFETY: `desc_ptr` validity is upheld by the FFI caller.
    let parsed = match unsafe { parse_dynamic_non_component_desc(ctx, desc_ptr, "Message") } {
        Ok(p) => p,
        Err(err) => return err,
    };
    let name_ref = parsed.canonical_name.as_str();
    let storage_flags =
        component_flags_to_storage_flags(parsed.desc.flags) | BAM_STORAGE_FLAG_MESSAGE;
    let existing_entry = ctx.registry.get_by_name(name_ref).cloned();
    let existing_comp_id = existing_entry.as_ref().map(|e| e.component_id);

    if let Err(collision) = ctx.registry.check_registration_collision(
        name_ref,
        parsed.desc.uuid,
        parsed.desc.size,
        parsed.desc.align,
        storage_flags,
        existing_comp_id,
    ) {
        ctx.host_ctx.set_last_error(collision.to_string());
        return FfiResult::TYPE_MISMATCH;
    }
    if let Some(ref existing) = existing_entry
        && existing.is_dynamic
        && existing.schema_fingerprint != 0
        && parsed.schema_fingerprint != 0
        && existing.schema_fingerprint != parsed.schema_fingerprint
    {
        ctx.host_ctx.set_last_error(format!(
            "Message ComponentDesc '{name_ref}' schema_fingerprint mismatch (existing=0x{:016x}, new=0x{:016x})",
            existing.schema_fingerprint, parsed.schema_fingerprint
        ));
        return FfiResult::TYPE_MISMATCH;
    }

    let is_active = Arc::clone(&ctx.is_active);
    let owner = ctx.mod_id;
    let handle = {
        let (registry, world) = ctx.registry_and_world_mut();
        match registry.register_message_from_desc(
            world,
            DynamicMessageReg {
                desc: &parsed.desc,
                name: name_ref,
                owner,
                is_active,
                schema_fingerprint: parsed.schema_fingerprint,
                fields: parsed.fields,
                owned_field_strings: parsed.owned_field_strings,
            },
        ) {
            Ok(h) => h,
            Err(err) => {
                ctx.host_ctx.set_last_error(err.to_string());
                return FfiResult::TYPE_MISMATCH;
            }
        }
    };

    ctx.host_ctx.set_snapshot(ctx.registry.snapshot_arc());
    if !ctx.world_ptr.is_null() {
        if let Some(entry) = ctx.registry.get_by_handle(handle) {
            let comp_id = entry.component_id;
            let world = ctx.world_mut();
            if world.resource_entities().get(comp_id).is_none() {
                let msg_entity = world
                    .iter_entities()
                    .find(|entity_ref| entity_ref.contains_id(comp_id))
                    .map(|entity_ref| entity_ref.id());
                if let Some(ent) = msg_entity
                    && let Ok(mut ent_mut) = world.get_entity_mut(ent)
                {
                    ent_mut.insert(bevy_ecs::resource::IsResource::new(comp_id));
                }
            }
        }
        let updated_reg = ctx.registry.clone();
        ctx.world_mut().insert_resource(updated_reg);
    }
    if !ctx.registered_components.contains(&handle) {
        ctx.registered_components.push(handle);
    }

    // SAFETY: `out_component` was verified non-null and caller guarantees writable storage.
    unsafe {
        *out_component = handle;
    }
    FfiResult::OK
}

#[cfg(test)]
#[path = "../../tests/unit/register_tests.rs"]
mod tests;
