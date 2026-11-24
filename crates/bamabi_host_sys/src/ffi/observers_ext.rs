//! Dynamic observer execution (`ModObserverState`, `guest_observer_runner`), extended deferred
//! trigger (`cmd_trigger_ex`), and runtime observer (`cmd_observe`) entrypoints.

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};

use bamabi_sys::{
    BamCommands, BamEventKind, BamIter, BamObserverDesc, BamTriggerDesc, FfiBool, FfiEntity,
    FfiResult, read_descriptor,
};
use bevy_ecs::{
    change_detection::Tick,
    component::{Component, ComponentId},
    entity::Entity,
    event::{EntityComponentsTrigger, EntityTrigger, EventKey, GlobalTrigger},
    hierarchy::ChildOf,
    lifecycle::{AddEvent, DespawnEvent, DiscardEvent, InsertEvent, RemoveEvent},
    message::Messages,
    observer::{Observer, TriggerContext},
    prelude::Resource,
    ptr::PtrMut,
    world::{DeferredWorld, World},
};
use bevy_platform::collections::HashSet;

use crate::{
    ffi::{
        commands::{CommandArenaConfig, HostCommands, HostCommandsStorage},
        register::{GuestObserverBinding, build_observer_binding_from_desc},
    },
    handle_header::{
        AlignedBuf, HostContext, decode_live_entity, decode_optional_entity, ffi_try,
        init_opt_out_ptr, next_non_zero_generation, validate_commands_mut,
    },
    loader::{LoadedModInfo, ModHost, ModId},
    registry::{EntityEventTriggerFn, ModRegistry, TriggerEventFn},
    sync_ptr::GuestFnPtr,
    system::{
        ModErrorPolicy, ModErrorPolicyConfig, ModSystemError, ModSystemFaulted,
        access_builder::initialize_query_groups_in_world,
        dispatch_mod_observer_fault_to_fallback_handler,
        error_policy::{ActiveGuestFaultGuard, dispatch_mod_fault_to_fallback_handler},
        guest_system::{HostSystemCtx, build_query_slot_iters},
        query_group::{CachedQueryGroup, MAX_SYSTEM_QUERIES},
    },
};

struct ObserverCtxState {
    query_groups: Vec<CachedQueryGroup>,
    last_run: Tick,
    last_error_buf: String,
}

/// ECS [`Component`] storing the shared [`GuestObserverBinding`] and per-observer enablement flag
/// on each spawned guest observer entity.
#[derive(Component)]
pub(crate) struct ModObserverState {
    pub(crate) binding: Arc<GuestObserverBinding>,
    pub(crate) enabled: Arc<AtomicBool>,
    ctx_state: Option<Arc<Mutex<ObserverCtxState>>>,
    cmd_storage_pool: Arc<Mutex<Vec<HostCommandsStorage>>>,
}

impl ModObserverState {
    pub(crate) fn new(binding: GuestObserverBinding) -> Self {
        Self {
            binding: Arc::new(binding),
            enabled: Arc::new(AtomicBool::new(true)),
            ctx_state: None,
            cmd_storage_pool: Arc::new(Mutex::new(Vec::with_capacity(1))),
        }
    }

    pub(crate) fn new_in_world(world: &mut World, binding: GuestObserverBinding) -> Self {
        if binding.ctx_callback.is_none() && binding.ctx_mut_callback.is_none() {
            return Self::new(binding);
        }

        let mod_id = binding.mod_id;
        let (mod_namespace, generation) = mod_id
            .and_then(|id| {
                world.get_resource::<ModHost>().and_then(|mh| {
                    mh.get_mod(id).map(|m| {
                        (
                            Some(Arc::<str>::from(m.name.as_ref())),
                            mh.mod_generation(&m.name),
                        )
                    })
                })
            })
            .unwrap_or((None, 0));

        let snap = world
            .get_resource::<ModRegistry>()
            .map(ModRegistry::snapshot_arc)
            .unwrap_or_else(|| binding.registry.latest_snapshot_arc());

        let mut query_groups: Vec<CachedQueryGroup> = binding
            .query_specs
            .iter()
            .cloned()
            .map(|spec| {
                let mut group = CachedQueryGroup::new(spec);
                if let Some(ns) = mod_namespace.as_ref() {
                    group.set_system_identity(Some(Arc::clone(ns)), Arc::clone(&binding.name));
                }
                group
            })
            .collect();

        match initialize_query_groups_in_world(world, &snap, &mut query_groups) {
            Ok(_access) => {
                let cell = world.as_unsafe_world_cell();
                for group in &mut query_groups {
                    group.update_archetypes(cell);
                }
                let change_tick = world.change_tick();
                let last_run = Tick::new(change_tick.get().wrapping_sub(Tick::MAX.get()));
                Self {
                    binding: Arc::new(binding),
                    enabled: Arc::new(AtomicBool::new(true)),
                    ctx_state: Some(Arc::new(Mutex::new(ObserverCtxState {
                        query_groups,
                        last_run,
                        last_error_buf: String::new(),
                    }))),
                    cmd_storage_pool: Arc::new(Mutex::new(Vec::with_capacity(1))),
                }
            }
            Err(err) => {
                let obs_name = binding.name.to_string();
                let status = err.status_code();
                let err_detail = err.to_string();
                let mod_ns_owned = mod_namespace.as_deref().map(str::to_owned);
                let ns_label = mod_ns_owned.as_deref().unwrap_or("unscoped");
                let last_run = world.change_tick();

                tracing::error!(
                    observer = %obs_name,
                    mod_id = ?mod_id,
                    namespace = %ns_label,
                    generation,
                    error = %err,
                    "guest mod ctx observer failed query initialization and was quarantined"
                );

                if let Some(id) = mod_id {
                    ModHost::record_fault_in_world(world, id);
                }

                let fault_msg = ModSystemFaulted {
                    mod_id,
                    mod_namespace: mod_ns_owned.clone(),
                    generation,
                    system_name: obs_name.clone(),
                    status,
                    panicked: false,
                    last_error: Some(err_detail.clone()),
                    init_error: Some(err),
                };
                if let Some(mut msgs) = world.get_resource_mut::<Messages<ModSystemFaulted>>() {
                    msgs.write(fault_msg);
                }

                let policy = world
                    .get_resource::<ModErrorPolicyConfig>()
                    .map_or(ModErrorPolicy::default(), |c| c.0);
                if matches!(
                    policy,
                    ModErrorPolicy::ReportToBevy | ModErrorPolicy::DelegateToBevyFallback
                ) {
                    dispatch_mod_fault_to_fallback_handler(
                        world,
                        ModSystemError {
                            mod_id,
                            mod_namespace: mod_ns_owned,
                            generation,
                            system_name: obs_name,
                            status,
                            panicked: false,
                            last_error: Some(err_detail),
                        },
                        last_run,
                    );
                }

                Self {
                    binding: Arc::new(binding),
                    enabled: Arc::new(AtomicBool::new(false)),
                    ctx_state: None,
                    cmd_storage_pool: Arc::new(Mutex::new(Vec::new())),
                }
            }
        }
    }
}

/// Per-`World` transient state for an active trigger dispatch (`cmd_trigger`, `cmd_trigger_ex`, or
/// lifecycle trigger), conveying the current target entity, mutable propagation flag, and explicit
/// watched component IDs across `guest_observer_runner`.
#[derive(Resource, Clone, Debug, Default)]
struct DynamicTriggerState {
    active: bool,
    current_target: Option<Entity>,
    propagate: bool,
    watched_ids: Vec<ComponentId>,
}

struct DynamicTriggerGuard<'w> {
    world: &'w mut World,
    prev_active: bool,
    prev_target: Option<Entity>,
    prev_propagate: bool,
    prev_watched_ids: Vec<ComponentId>,
}

impl<'w> DynamicTriggerGuard<'w> {
    fn enter(
        world: &'w mut World,
        current_target: Option<Entity>,
        propagate: bool,
        watched_ids: &[ComponentId],
    ) -> Self {
        world.init_resource::<DynamicTriggerState>();
        let mut prev_active = false;
        let mut prev_target = None;
        let mut prev_propagate = false;
        let mut prev_watched_ids = Vec::new();
        if let Some(mut state) = world.get_resource_mut::<DynamicTriggerState>() {
            prev_active = state.active;
            if prev_active {
                prev_target = state.current_target;
                prev_propagate = state.propagate;
                prev_watched_ids = core::mem::take(&mut state.watched_ids);
            }
            state.active = true;
            state.current_target = current_target;
            state.propagate = propagate;
            state.watched_ids.clear();
            state.watched_ids.extend_from_slice(watched_ids);
        }
        Self {
            world,
            prev_active,
            prev_target,
            prev_propagate,
            prev_watched_ids,
        }
    }

    fn world_mut(&mut self) -> &mut World {
        self.world
    }

    fn propagate(&self) -> bool {
        self.world
            .get_resource::<DynamicTriggerState>()
            .is_some_and(|s| s.active && s.propagate)
    }
}

impl Drop for DynamicTriggerGuard<'_> {
    fn drop(&mut self) {
        if let Some(mut state) = self.world.get_resource_mut::<DynamicTriggerState>() {
            state.active = self.prev_active;
            if self.prev_active {
                state.current_target = self.prev_target;
                state.propagate = self.prev_propagate;
                state.watched_ids = core::mem::take(&mut self.prev_watched_ids);
            } else {
                state.current_target = None;
                state.propagate = false;
                state.watched_ids.clear();
            }
        }
    }
}

/// Executes `f` with a scoped [`DynamicTriggerState`] installed on `world`, restoring any prior
/// nested trigger state when `f` returns.
pub(crate) fn with_dynamic_trigger_state<R>(
    world: &mut World,
    current_target: Option<Entity>,
    propagate: bool,
    watched_ids: &[ComponentId],
    f: impl FnOnce(&mut World) -> R,
) -> R {
    let mut guard = DynamicTriggerGuard::enter(world, current_target, propagate, watched_ids);
    f(guard.world_mut())
}

/// Selects the read-only payload pointer passed to `BamObserverFn` or `BamObserverCtxFn`.
#[inline]
fn observer_read_data_ptr(
    binding: &GuestObserverBinding,
    wire_entity: &FfiEntity,
    event_read_ptr: *const c_void,
) -> *const c_void {
    if binding.synthesizes_ffi_entity_payload || binding.event_kind.is_component_lifecycle() {
        core::ptr::from_ref(wire_entity).cast::<c_void>()
    } else if binding.event_size == 0 {
        core::ptr::null()
    } else {
        event_read_ptr
    }
}

/// Dynamic [`ObserverRunner`](bevy_ecs::observer::ObserverRunner) dispatching Bevy `Event`,
/// `EntityEvent`, `Ready`, or lifecycle events into a registered guest [`BamObserverFn`],
/// [`BamObserverMutFn`], [`BamObserverCtxFn`], or [`BamObserverCtxMutFn`].
///
/// # Safety
/// - `world` must be the [`DeferredWorld`] owning `observer`.
/// - `event_ptr` must point to an initialized value matching the registered event layout.
pub(crate) unsafe fn guest_observer_runner(
    mut world: DeferredWorld,
    observer: Entity,
    _trigger_ctx: &TriggerContext,
    event_ptr: PtrMut,
    _trigger_ptr: PtrMut,
) {
    let Some((binding, enabled, ctx_state, cmd_storage_pool)) =
        world.get::<ModObserverState>(observer).map(|state| {
            (
                Arc::clone(&state.binding),
                Arc::clone(&state.enabled),
                state.ctx_state.clone(),
                Arc::clone(&state.cmd_storage_pool),
            )
        })
    else {
        return;
    };
    if !binding.is_active.load(Ordering::Acquire) || !enabled.load(Ordering::Acquire) {
        return;
    }

    let trigger_info = world
        .get_resource::<DynamicTriggerState>()
        .filter(|s| s.active)
        .map(|s| {
            let watched = if !binding.event_kind.is_component_lifecycle()
                && !binding.watched_components.is_empty()
            {
                s.watched_ids.clone()
            } else {
                Vec::new()
            };
            (s.current_target, s.propagate, watched)
        });

    let target_ffi = if binding.event_kind.is_component_lifecycle() {
        let entity = match binding.event_kind {
            // SAFETY: `event_ptr` points to the lifecycle event struct matching `binding.event_kind`.
            BamEventKind::ADD => unsafe { &*(event_ptr.as_ptr().cast::<AddEvent>()) }.entity,
            // SAFETY: Same as above (`InsertEvent`).
            BamEventKind::INSERT => unsafe { &*(event_ptr.as_ptr().cast::<InsertEvent>()) }.entity,
            BamEventKind::DISCARD => {
                // SAFETY: Same as above (`DiscardEvent`).
                unsafe { &*(event_ptr.as_ptr().cast::<DiscardEvent>()) }.entity
            }
            // SAFETY: Same as above (`RemoveEvent`).
            BamEventKind::REMOVE => unsafe { &*(event_ptr.as_ptr().cast::<RemoveEvent>()) }.entity,
            BamEventKind::DESPAWN => {
                // SAFETY: Same as above (`DespawnEvent`).
                unsafe { &*(event_ptr.as_ptr().cast::<DespawnEvent>()) }.entity
            }
            _ => Entity::PLACEHOLDER,
        };
        if entity == Entity::PLACEHOLDER {
            FfiEntity::NULL
        } else {
            FfiEntity::from_bits(entity.to_bits())
        }
    } else if let Some(extract_fn) = binding.extract_target_fn {
        // SAFETY: `event_ptr` holds an initialized `EntityEvent` matching `binding.event_id`.
        match unsafe { extract_fn(event_ptr.as_ptr()) } {
            Some(entity) => FfiEntity::from_bits(entity.to_bits()),
            None => FfiEntity::NULL,
        }
    } else if let Some(active_target) = trigger_info.as_ref().and_then(|(t, _, _)| *t) {
        FfiEntity::from_bits(active_target.to_bits())
    } else if let Some(watched) = binding.target_entity {
        FfiEntity::from_bits(watched.to_bits())
    } else {
        FfiEntity::NULL
    };

    if !binding.event_kind.is_component_lifecycle() && !binding.watched_components.is_empty() {
        let active_watched = trigger_info
            .as_ref()
            .map_or(&[][..], |(_, _, watched)| watched.as_slice());
        let matched = if !active_watched.is_empty() {
            binding
                .watched_components
                .iter()
                .any(|cid| active_watched.contains(cid))
        } else if let Some(target_ent) = Entity::try_from_bits(target_ffi.0)
            .filter(|&ent| target_ffi != FfiEntity::NULL && ent != Entity::PLACEHOLDER)
        {
            world.get_entity(target_ent).is_ok_and(|ent_ref| {
                binding
                    .watched_components
                    .iter()
                    .all(|cid| ent_ref.contains_id(*cid))
            })
        } else {
            false
        };
        if !matched {
            return;
        }
    }

    let initial_propagate = trigger_info
        .as_ref()
        .map_or(binding.is_propagating, |(_, p, _)| *p);
    let mut propagate_ffi = FfiBool::from_bool(initial_propagate);

    let mut storage = cmd_storage_pool
        .try_lock()
        .ok()
        .and_then(|mut pool| pool.pop())
        .unwrap_or_else(HostCommandsStorage::new);
    let max_retained_slabs = world
        .get_resource::<CommandArenaConfig>()
        .map_or(2, |cfg| cfg.max_retained_slabs);
    let snap = binding.registry.latest_snapshot_arc();
    let mut status = FfiResult::OK;
    let mut wire_entity = target_ffi;
    let mut last_error: Option<String> = None;

    let mut_data_ptr: *mut u8 =
        if binding.event_kind.is_component_lifecycle() || binding.event_size == 0 {
            core::ptr::null_mut()
        } else if binding.synthesizes_ffi_entity_payload {
            (&raw mut wire_entity).cast::<u8>()
        } else {
            event_ptr.as_ptr()
        };

    if let Some(GuestFnPtr(mut_cb)) = binding.mut_callback {
        let cell = world.as_unsafe_world_cell();
        let allocator = cell.entity_allocator();
        let mut host_cmds = HostCommands::new(&mut storage, allocator, &snap)
            .with_mod_context(binding.mod_id, Some(Arc::clone(&binding.is_active)))
            .with_registry(&binding.registry);
        // SAFETY: `binding.is_active` is verified true, `mut_data_ptr` is null or valid writable
        // storage for this event, `propagate_ffi` is stack-scoped, and `host_cmds` is poisoned before exit.
        status = unsafe {
            mut_cb(
                target_ffi,
                mut_data_ptr,
                &mut propagate_ffi,
                host_cmds.as_ffi_ptr(),
                binding.user_data.as_ptr(),
            )
        };
    }

    if let Some(mut trigger_state) = world.get_resource_mut::<DynamicTriggerState>()
        && trigger_state.active
        && binding.mut_callback.is_some()
    {
        trigger_state.propagate = propagate_ffi.get();
    }

    let event_read_ptr = event_ptr.as_ptr().cast::<c_void>();

    if status == FfiResult::OK
        && let Some(GuestFnPtr(callback)) = binding.callback
    {
        let cell = world.as_unsafe_world_cell();
        let allocator = cell.entity_allocator();
        let mut host_cmds = HostCommands::new(&mut storage, allocator, &snap)
            .with_mod_context(binding.mod_id, Some(Arc::clone(&binding.is_active)))
            .with_registry(&binding.registry);
        let data_ptr = observer_read_data_ptr(&binding, &wire_entity, event_read_ptr);
        // SAFETY: `binding.is_active` is verified true and `host_cmds` is poisoned before exit.
        status = unsafe {
            callback(
                target_ffi,
                data_ptr,
                host_cmds.as_ffi_ptr(),
                binding.user_data.as_ptr(),
            )
        };
    }

    if status == FfiResult::OK
        && (binding.ctx_mut_callback.is_some() || binding.ctx_callback.is_some())
    {
        let (mod_ns_arc, generation) = binding
            .mod_id
            .and_then(|id| {
                world.get_resource::<ModHost>().and_then(|mh| {
                    mh.get_mod(id).map(|m| {
                        (
                            Some(Arc::<str>::from(m.name.as_ref())),
                            mh.mod_generation(&m.name),
                        )
                    })
                })
            })
            .unwrap_or((None, 0));

        if let Some(state_arc) = ctx_state.as_ref() {
            match state_arc.try_lock() {
                Ok(mut guard) => {
                    let state = &mut *guard;
                    let err_buf = core::mem::take(&mut state.last_error_buf);
                    let last_run = state.last_run;

                    let (cb_status, reported_fault, reclaimed_err, ctx_mut_ran) = {
                        let cell = world.as_unsafe_world_cell();
                        let allocator = cell.entity_allocator();
                        let change_tick = cell.increment_change_tick();
                        for group in &mut state.query_groups {
                            group.update_archetypes(cell);
                        }

                        // SAFETY: `cell` grants observer access and `query_groups` was validated at init.
                        let (count, mut host_iters, query_views) = unsafe {
                            build_query_slot_iters(
                                cell,
                                &mut state.query_groups,
                                last_run,
                                change_tick,
                            )
                        };
                        let mut iter_ptrs: [*mut BamIter; MAX_SYSTEM_QUERIES] =
                            [core::ptr::null_mut(); MAX_SYSTEM_QUERIES];
                        for i in 0..count {
                            if let Some(ref mut it) = host_iters[i] {
                                iter_ptrs[i] = it.as_ffi_ptr();
                            }
                        }

                        let mut host_cmds = HostCommands::new(&mut storage, allocator, &snap)
                            .with_mod_context(binding.mod_id, Some(Arc::clone(&binding.is_active)))
                            .with_registry(&binding.registry);
                        let cmds_ptr = host_cmds.as_ffi_ptr();
                        let host_ctx = HostContext::with_error_buffer(Arc::clone(&snap), err_buf)
                            .with_namespace(mod_ns_arc.clone());
                        let mut sys_ctx = HostSystemCtx::new_reused_full(
                            host_ctx,
                            next_non_zero_generation(),
                            &iter_ptrs[..count],
                            &query_views[..count],
                            cmds_ptr,
                            (cell, last_run, change_tick),
                            (&[], Vec::new(), Vec::new()),
                        );
                        let host_ctx_ptr = sys_ctx.host_ctx_ffi_ptr();
                        let sys_ctx_ptr = sys_ctx.as_ffi_ptr();

                        let mut res = FfiResult::OK;
                        let mut reported: Option<String> = None;
                        let mut ran_ctx_mut = false;

                        if let Some(GuestFnPtr(ctx_mut_cb)) = binding.ctx_mut_callback {
                            ran_ctx_mut = true;
                            // SAFETY: `host_ctx_ptr` points to `sys_ctx.host_ctx`, `mod_ns_arc` and
                            // `binding.name` outlive `fault_guard`, and `fault_guard` finishes below.
                            let fault_guard = unsafe {
                                ActiveGuestFaultGuard::enter(
                                    host_ctx_ptr,
                                    binding.mod_id,
                                    mod_ns_arc.as_deref(),
                                    generation,
                                    &binding.name,
                                )
                            };
                            // SAFETY: `binding.is_active` is true, `mut_data_ptr` satisfies event layout,
                            // `propagate_ffi` is stack-scoped, and `sys_ctx` / `host_cmds` / `host_iters` are poisoned before exit.
                            res = unsafe {
                                ctx_mut_cb(
                                    target_ffi,
                                    mut_data_ptr,
                                    &mut propagate_ffi,
                                    sys_ctx_ptr,
                                    binding.user_data.as_ptr(),
                                )
                            };
                            reported = fault_guard.finish();
                        }

                        if res == FfiResult::OK
                            && reported.is_none()
                            && let Some(GuestFnPtr(ctx_cb)) = binding.ctx_callback
                        {
                            let data_ptr =
                                observer_read_data_ptr(&binding, &wire_entity, event_read_ptr);
                            // SAFETY: `host_ctx_ptr` points to `sys_ctx.host_ctx`, `mod_ns_arc` and
                            // `binding.name` outlive `fault_guard`, and `fault_guard` finishes below.
                            let fault_guard = unsafe {
                                ActiveGuestFaultGuard::enter(
                                    host_ctx_ptr,
                                    binding.mod_id,
                                    mod_ns_arc.as_deref(),
                                    generation,
                                    &binding.name,
                                )
                            };
                            // SAFETY: `binding.is_active` is true, `data_ptr` satisfies event layout,
                            // and `sys_ctx` / `host_cmds` / `host_iters` headers are poisoned before exit.
                            res = unsafe {
                                ctx_cb(
                                    target_ffi,
                                    data_ptr,
                                    sys_ctx_ptr,
                                    binding.user_data.as_ptr(),
                                )
                            };
                            reported = fault_guard.finish();
                        }

                        let reclaimed = sys_ctx.reclaim_error_buffer();
                        drop(host_iters);
                        (res, reported, reclaimed, ran_ctx_mut)
                    };
                    state.last_error_buf = reclaimed_err;

                    if ctx_mut_ran
                        && let Some(mut trigger_state) =
                            world.get_resource_mut::<DynamicTriggerState>()
                        && trigger_state.active
                    {
                        trigger_state.propagate = propagate_ffi.get();
                    }

                    if cb_status == FfiResult::PANIC_CAUGHT || cb_status.is_err() {
                        status = cb_status;
                    } else if reported_fault.is_some() {
                        status = FfiResult::INTERNAL_ERROR;
                    } else {
                        state.last_run = world.change_tick();
                    }

                    if status != FfiResult::OK {
                        last_error = reported_fault.filter(|s| !s.is_empty()).or_else(|| {
                            (!state.last_error_buf.is_empty()).then(|| state.last_error_buf.clone())
                        });
                    }
                }
                Err(TryLockError::WouldBlock) => {
                    status = FfiResult::ACCESS_CONFLICT;
                    last_error =
                        Some("re-entrant ctx_callback observer invocation rejected".to_string());
                }
                Err(TryLockError::Poisoned(poisoned)) => {
                    let _guard = poisoned.into_inner();
                    status = FfiResult::INTERNAL_ERROR;
                    last_error =
                        Some("ctx_callback observer query state mutex was poisoned".to_string());
                }
            }
        } else {
            status = FfiResult::INVALID_STATE;
            last_error = Some("ctx_callback observer missing initialized query state".to_string());
        }
    }

    if status == FfiResult::OK {
        if !storage.is_empty() {
            let pool = Arc::clone(&cmd_storage_pool);
            world.commands().queue(move |w: &mut World| {
                let cap = w
                    .get_resource::<CommandArenaConfig>()
                    .map_or(2, |cfg| cfg.max_retained_slabs);
                storage.apply_to_world(w, &snap);
                storage.clear_with_retained_slabs(cap);
                if let Ok(mut guard) = pool.lock()
                    && guard.len() < 2
                {
                    guard.push(storage);
                }
            });
        } else {
            storage.clear_with_retained_slabs(max_retained_slabs);
            if let Ok(mut guard) = cmd_storage_pool.try_lock()
                && guard.len() < 2
            {
                guard.push(storage);
            }
        }
        return;
    }

    storage.clear_with_retained_slabs(max_retained_slabs);
    if let Ok(mut guard) = cmd_storage_pool.try_lock()
        && guard.len() < 2
    {
        guard.push(storage);
    }
    let policy = world
        .get_resource::<ModErrorPolicyConfig>()
        .map_or(ModErrorPolicy::default(), |c| c.0);
    if !matches!(policy, ModErrorPolicy::DelegateToBevyFallback) {
        // Quarantine only this faulting observer; keep sibling systems/observers in the mod active.
        enabled.store(false, Ordering::Release);
    }

    let mod_id = binding.mod_id;
    let (mod_namespace, generation) = mod_id
        .and_then(|id| {
            world.get_resource::<ModHost>().and_then(|mh| {
                mh.get_mod(id)
                    .map(|m| (Some(m.name.to_string()), mh.mod_generation(&m.name)))
            })
        })
        .unwrap_or((None, 0));
    let panicked = status == FfiResult::PANIC_CAUGHT;
    let obs_name = binding.name.to_string();
    let last_run = world.change_tick();
    tracing::error!(
        observer = %obs_name,
        mod_id = ?mod_id,
        namespace = %mod_namespace.as_deref().unwrap_or("unscoped"),
        generation,
        %status,
        panicked,
        last_error = ?last_error,
        "guest mod observer faulted; discarding uncommitted commands and routing to FallbackErrorHandler"
    );

    let fault_msg = ModSystemFaulted {
        mod_id,
        mod_namespace: mod_namespace.clone(),
        generation,
        system_name: obs_name.clone(),
        status,
        panicked,
        last_error: last_error.clone(),
        init_error: None,
    };
    if let Some(id) = mod_id {
        let entity_opt = world
            .get_resource_mut::<ModHost>()
            .and_then(|mut host| host.increment_fault_count(id));
        if let Some(entity) = entity_opt
            && let Some(mut info) = world.get_mut::<LoadedModInfo>(entity)
        {
            info.fault_count = info.fault_count.saturating_add(1);
        }
    }
    if let Some(mut msgs) = world.get_resource_mut::<Messages<ModSystemFaulted>>() {
        msgs.write(fault_msg);
    }

    dispatch_mod_observer_fault_to_fallback_handler(
        &world,
        ModSystemError {
            mod_id,
            mod_namespace,
            generation,
            system_name: obs_name,
            status,
            panicked,
            last_error,
        },
        last_run,
    );
}

/// Attaches event key, target entity, and watched components to a dynamic [`Observer`].
fn build_observer_component(binding: &GuestObserverBinding) -> Observer {
    let mut obs = Observer::with_dynamic_runner(guest_observer_runner);
    // SAFETY: `binding.event_key` was resolved/registered in `world` with matching payload layout.
    obs = unsafe { obs.with_event_key(binding.event_key) };
    if let Some(target) = binding.target_entity {
        obs.watch_entity(target);
    }
    if binding.event_kind.is_component_lifecycle() && !binding.watched_components.is_empty() {
        obs = obs.with_components(binding.watched_components.iter().copied());
    }
    obs
}

/// Spawns all init-registered [`GuestObserverBinding`]s into `world`, flushing immediately and
/// returning the spawned observer entity IDs so `LoadedMod` can despawn them on unload.
pub(crate) fn spawn_registered_observers_in_world(
    world: &mut World,
    registered_observers: Vec<GuestObserverBinding>,
    registry: &ModRegistry,
    mod_id: ModId,
) -> Vec<Entity> {
    if registered_observers.is_empty() {
        return Vec::new();
    }
    let mut observer_entities = Vec::with_capacity(registered_observers.len());
    for mut obs_binding in registered_observers {
        obs_binding.registry = registry.clone();
        obs_binding.mod_id = Some(mod_id);
        let obs = build_observer_component(&obs_binding);
        let obs_state = ModObserverState::new_in_world(world, obs_binding);
        let obs_entity = world.spawn((obs, obs_state)).id();
        observer_entities.push(obs_entity);
    }
    world.flush();
    observer_entities
}

#[derive(Clone, Copy)]
enum PropagateRelationship {
    ChildOf,
    DynamicEntity(ComponentId),
}

fn next_propagation_parent(
    world: &World,
    entity: Entity,
    rel: PropagateRelationship,
) -> Option<Entity> {
    match rel {
        PropagateRelationship::ChildOf => world.get::<ChildOf>(entity).map(ChildOf::parent),
        PropagateRelationship::DynamicEntity(cid) => {
            let ent = world.get_entity(entity).ok()?;
            let ptr = ent.get_by_id(cid).ok()?;
            // SAFETY: Validated at `host_cmd_trigger_ex` call time that `cid` has `size == 8`.
            let bits = unsafe { core::ptr::read_unaligned(ptr.as_ptr().cast::<u64>()) };
            Entity::try_from_bits(bits)
        }
    }
}

struct CustomTriggerDispatch<'a> {
    event_key: EventKey,
    entity_trigger_fn: Option<EntityEventTriggerFn>,
    trigger_fn: Option<TriggerEventFn>,
    watched_ids: &'a [ComponentId],
    payload: &'a mut AlignedBuf,
}

impl CustomTriggerDispatch<'_> {
    fn dispatch_at(&mut self, world: &mut World, target: Option<Entity>, propagate: bool) -> bool {
        let mut guard = DynamicTriggerGuard::enter(world, target, propagate, self.watched_ids);
        let w = guard.world_mut();
        if let Some(et_fn) = self.entity_trigger_fn {
            if let Some(t) = target
                && t != Entity::PLACEHOLDER
                && w.entities().contains(t)
            {
                et_fn(w, t);
            }
        } else if !self.watched_ids.is_empty()
            && let Some(t) = target
        {
            let mut trigger = EntityComponentsTrigger {
                components: self.watched_ids,
                ..Default::default()
            };
            let event_mut = self.payload.as_ptr_mut();
            let trigger_mut = PtrMut::from(&mut trigger);
            // SAFETY: `event_key` matches `payload` layout and `trigger_mut` is `EntityComponentsTrigger`.
            unsafe {
                w.trigger_dynamic_targets_components(
                    self.event_key,
                    t,
                    self.watched_ids,
                    event_mut,
                    trigger_mut,
                );
            }
        } else if let Some(tfn) = self.trigger_fn {
            // SAFETY: `payload.as_ptr()` holds `meta.size` bytes matching the host event type.
            unsafe {
                let _ = tfn(w, target, self.payload.as_ptr());
            }
        } else if let Some(t) = target {
            let mut trigger = EntityTrigger;
            let event_mut = self.payload.as_ptr_mut();
            let trigger_mut = PtrMut::from(&mut trigger);
            // SAFETY: `event_key` matches `payload` layout and `trigger_mut` is `EntityTrigger`.
            unsafe {
                w.trigger_dynamic_targets(self.event_key, t, event_mut, trigger_mut);
            }
        } else {
            let mut trigger = GlobalTrigger;
            let event_mut = self.payload.as_ptr_mut();
            let trigger_mut = PtrMut::from(&mut trigger);
            // SAFETY: `event_key` matches `payload` layout and `trigger_mut` is `GlobalTrigger`.
            unsafe {
                w.trigger_dynamic(self.event_key, event_mut, trigger_mut);
            }
        }
        guard.propagate()
    }
}

/// Triggers a single Bevy lifecycle event struct (`AddEvent`, `InsertEvent`, etc.) for `target`.
///
/// # Safety
/// `event_key` must match the Bevy lifecycle event type `E`.
unsafe fn trigger_lifecycle<E>(
    world: &mut World,
    event_key: EventKey,
    target: Entity,
    watched_ids: &[ComponentId],
    mut event: E,
) {
    let mut trigger = EntityComponentsTrigger {
        components: watched_ids,
        ..Default::default()
    };
    // SAFETY: Caller guarantees `event_key` matches `E` and `EntityComponentsTrigger`.
    unsafe {
        world.trigger_dynamic_targets_components(
            event_key,
            target,
            watched_ids,
            PtrMut::from(&mut event),
            PtrMut::from(&mut trigger),
        );
    }
}

fn dispatch_lifecycle_at_target(
    world: &mut World,
    kind: BamEventKind,
    target: Entity,
    watched_ids: &[ComponentId],
    propagate: bool,
) -> bool {
    let mut guard = DynamicTriggerGuard::enter(world, Some(target), propagate, watched_ids);
    let w = guard.world_mut();
    match kind {
        BamEventKind::ADD => {
            // SAFETY: `ADD` expects `AddEvent`.
            unsafe {
                trigger_lifecycle(
                    w,
                    bevy_ecs::lifecycle::ADD,
                    target,
                    watched_ids,
                    AddEvent { entity: target },
                );
            }
        }
        BamEventKind::INSERT => {
            // SAFETY: `INSERT` expects `InsertEvent`.
            unsafe {
                trigger_lifecycle(
                    w,
                    bevy_ecs::lifecycle::INSERT,
                    target,
                    watched_ids,
                    InsertEvent { entity: target },
                );
            }
        }
        BamEventKind::DISCARD => {
            // SAFETY: `DISCARD` expects `DiscardEvent`.
            unsafe {
                trigger_lifecycle(
                    w,
                    bevy_ecs::lifecycle::DISCARD,
                    target,
                    watched_ids,
                    DiscardEvent { entity: target },
                );
            }
        }
        BamEventKind::REMOVE => {
            // SAFETY: `REMOVE` expects `RemoveEvent`.
            unsafe {
                trigger_lifecycle(
                    w,
                    bevy_ecs::lifecycle::REMOVE,
                    target,
                    watched_ids,
                    RemoveEvent { entity: target },
                );
            }
        }
        BamEventKind::DESPAWN => {
            // SAFETY: `DESPAWN` expects `DespawnEvent`.
            unsafe {
                trigger_lifecycle(
                    w,
                    bevy_ecs::lifecycle::DESPAWN,
                    target,
                    watched_ids,
                    DespawnEvent { entity: target },
                );
            }
        }
        _ => {}
    }
    guard.propagate()
}

/// Dispatches a trigger at `target` and, when `is_propagating` is true, bubbles up `prop_rel`
/// parents with cycle detection until stopped or no live parent remains.
fn propagate_trigger(
    world: &mut World,
    target: Option<Entity>,
    is_propagating: bool,
    prop_rel: PropagateRelationship,
    mut dispatch: impl FnMut(&mut World, Option<Entity>, bool) -> bool,
) {
    if let Some(t) = target
        && !world.entities().contains(t)
    {
        return;
    }
    if !is_propagating {
        let _ = dispatch(world, target, false);
        return;
    }
    let Some(mut current) = target else {
        return;
    };
    let mut visited = HashSet::new();
    visited.insert(current);
    loop {
        if !dispatch(world, Some(current), true) {
            break;
        }
        let Some(next) = next_propagation_parent(world, current, prop_rel) else {
            break;
        };
        if next == Entity::PLACEHOLDER || !world.entities().contains(next) || !visited.insert(next)
        {
            break;
        }
        current = next;
    }
}

/// Reads an unaligned 8-byte `FfiEntity` from `data_ptr` and decodes it as a live [`Entity`].
///
/// # Safety
/// `data_ptr` must be non-null and readable for 8 bytes.
#[inline]
unsafe fn decode_entity_from_bytes(data_ptr: *const u8) -> Result<Entity, FfiResult> {
    // SAFETY: Caller guarantees `data_ptr` is readable for 8 bytes.
    let bits = unsafe { core::ptr::read_unaligned(data_ptr.cast::<u64>()) };
    decode_live_entity(FfiEntity::from_bits(bits))
}

/// Queues an extended deferred trigger described by [`BamTriggerDesc`].
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `desc` must point to a readable [`BamTriggerDesc`], or null.
pub unsafe extern "C" fn host_cmd_trigger_ex(
    commands: *mut BamCommands,
    desc: *const BamTriggerDesc,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamTriggerDesc::MIN_V0_2_SIZE`.
    let parsed = ffi_try!(unsafe { read_descriptor::<BamTriggerDesc>(desc.cast::<c_void>()) });
    let val = parsed.validate();
    if val.is_err() {
        return val;
    }

    let snap = host_cmds.snapshot();
    let mut target_entity = ffi_try!(decode_optional_entity(parsed.entity()));

    let is_propagating = parsed.is_propagating();
    let prop_rel = if is_propagating && parsed.propagate_relationship.is_valid() {
        let Some(rel_meta) = snap.get_by_handle(parsed.propagate_relationship) else {
            return FfiResult::INVALID_COMPONENT;
        };
        if rel_meta.is_resource() || rel_meta.is_message() {
            return FfiResult::INVALID_COMPONENT;
        }
        if rel_meta.validates_entity_payload {
            PropagateRelationship::ChildOf
        } else if rel_meta.size == core::mem::size_of::<FfiEntity>()
            && rel_meta.align <= core::mem::align_of::<FfiEntity>()
        {
            PropagateRelationship::DynamicEntity(rel_meta.component_id)
        } else {
            return FfiResult::INVALID_COMPONENT;
        }
    } else {
        PropagateRelationship::ChildOf
    };

    let mut watched_ids = Vec::with_capacity(parsed.watched_component_count + 1);
    if parsed.watched_component_count > 0 {
        // SAFETY: `parsed.validate()` verified `!parsed.watched_components.is_null()`.
        let handles = unsafe {
            core::slice::from_raw_parts(parsed.watched_components, parsed.watched_component_count)
        };
        for &handle in handles {
            let Some(m) = snap.get_by_handle(handle) else {
                return FfiResult::INVALID_COMPONENT;
            };
            if m.is_resource() || m.is_message() {
                return FfiResult::INVALID_COMPONENT;
            }
            if !watched_ids.contains(&m.component_id) {
                watched_ids.push(m.component_id);
            }
        }
    }

    let kind = parsed.event_kind;
    if kind.is_custom() {
        let Some(meta) = snap.get_by_handle(parsed.event) else {
            return FfiResult::INVALID_COMPONENT;
        };
        let Some(event_key) = meta.event_key else {
            return FfiResult::INVALID_COMPONENT;
        };
        if meta.is_resource()
            || meta.is_message()
            || meta.drop_fn.is_some()
            || meta.drop_slot.is_some()
        {
            return FfiResult::INVALID_COMPONENT;
        }
        if meta.size != parsed.data_len {
            return FfiResult::INVALID_ARGUMENT;
        }
        if meta.is_entity_event() && target_entity.is_none() {
            if meta.synthesizes_ffi_entity_payload {
                if parsed.data_len != core::mem::size_of::<FfiEntity>() || parsed.data_ptr.is_null()
                {
                    return FfiResult::INVALID_ARGUMENT;
                }
                // SAFETY: `parsed.data_ptr` is non-null and readable for 8 bytes.
                target_entity = Some(ffi_try!(unsafe {
                    decode_entity_from_bytes(parsed.data_ptr)
                }));
            } else if let Some(extract_fn) = meta.extract_event_target_fn {
                // SAFETY: `parsed.data_ptr` holds `meta.size` initialized bytes matching `EntityEvent`.
                let Some(extracted) = (unsafe { extract_fn(parsed.data_ptr) }) else {
                    return FfiResult::INVALID_ENTITY;
                };
                if extracted == Entity::PLACEHOLDER {
                    return FfiResult::INVALID_ENTITY;
                }
                target_entity = Some(extracted);
            } else {
                return FfiResult::INVALID_ENTITY;
            }
        }
        if is_propagating && target_entity.is_none() {
            return FfiResult::INVALID_ENTITY;
        }
        // SAFETY: `parsed.data_ptr` is readable for `meta.size` bytes when `meta.size > 0`.
        let mut payload =
            ffi_try!(unsafe { AlignedBuf::copy_from(parsed.data_ptr, meta.size, meta.align) });
        let entity_trigger_fn = meta.entity_event_trigger_fn;
        let trigger_fn = meta.trigger_event_fn;

        host_cmds.queue_custom(move |world, _snap| {
            let mut ctx = CustomTriggerDispatch {
                event_key,
                entity_trigger_fn,
                trigger_fn,
                watched_ids: &watched_ids,
                payload: &mut payload,
            };
            propagate_trigger(world, target_entity, is_propagating, prop_rel, |w, t, p| {
                ctx.dispatch_at(w, t, p)
            });
        });
        return FfiResult::OK;
    }

    if kind.is_component_lifecycle() {
        let Some(start_target) = target_entity else {
            return FfiResult::INVALID_ENTITY;
        };
        if parsed.data_len != 0 {
            return FfiResult::INVALID_ARGUMENT;
        }
        if parsed.event.is_valid() {
            let Some(m) = snap.get_by_handle(parsed.event) else {
                return FfiResult::INVALID_COMPONENT;
            };
            if m.is_resource() || m.is_message() {
                return FfiResult::INVALID_COMPONENT;
            }
            if !watched_ids.contains(&m.component_id) {
                watched_ids.push(m.component_id);
            }
        }
        if watched_ids.is_empty() {
            return FfiResult::INVALID_COMPONENT;
        }
        host_cmds.queue_custom(move |world, _snap| {
            propagate_trigger(
                world,
                Some(start_target),
                is_propagating,
                prop_rel,
                |w, t, p| {
                    let Some(ent) = t else {
                        return false;
                    };
                    dispatch_lifecycle_at_target(w, kind, ent, &watched_ids, p)
                },
            );
        });
        return FfiResult::OK;
    }

    if kind.is_ready() {
        let meta = if parsed.event.is_valid() {
            let Some(m) = snap.get_by_handle(parsed.event) else {
                return FfiResult::INVALID_COMPONENT;
            };
            m
        } else if let Some(m) = snap
            .get_by_name("bevy_scene::Ready")
            .or_else(|| snap.get_by_name("Ready"))
        {
            m
        } else {
            return FfiResult::NOT_FOUND;
        };
        let Some(event_key) = meta.event_key else {
            return FfiResult::INVALID_COMPONENT;
        };
        if meta.is_resource() || meta.is_message() {
            return FfiResult::INVALID_COMPONENT;
        }
        if target_entity.is_none() && parsed.data_len == core::mem::size_of::<FfiEntity>() {
            // SAFETY: `parsed.data_len == 8` and `parsed.validate()` verified non-null `data_ptr`.
            target_entity = Some(ffi_try!(unsafe {
                decode_entity_from_bytes(parsed.data_ptr)
            }));
        } else if parsed.data_len != 0 && parsed.data_len != meta.size {
            return FfiResult::INVALID_ARGUMENT;
        }
        let Some(start_target) = target_entity else {
            return FfiResult::INVALID_ENTITY;
        };
        let entity_trigger_fn = meta.entity_event_trigger_fn;
        let trigger_fn = meta.trigger_event_fn;
        let ffi_bytes = FfiEntity::from_bits(start_target.to_bits()).to_bits();
        // SAFETY: `ffi_bytes` is a local 8-byte `u64` matching `FfiEntity` layout.
        let mut payload = ffi_try!(unsafe {
            AlignedBuf::copy_from(
                (&raw const ffi_bytes).cast::<u8>(),
                core::mem::size_of::<FfiEntity>(),
                core::mem::align_of::<FfiEntity>(),
            )
        });

        host_cmds.queue_custom(move |world, _snap| {
            let mut ctx = CustomTriggerDispatch {
                event_key,
                entity_trigger_fn,
                trigger_fn,
                watched_ids: &watched_ids,
                payload: &mut payload,
            };
            propagate_trigger(
                world,
                Some(start_target),
                is_propagating,
                prop_rel,
                |w, t, p| ctx.dispatch_at(w, t, p),
            );
        });
        return FfiResult::OK;
    }

    FfiResult::INVALID_ARGUMENT
}

/// Queues deferred spawning of a runtime observer described by [`BamObserverDesc`], optionally
/// writing the reserved observer [`FfiEntity`] into `*out_entity` when non-null.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `desc` must point to a readable [`BamObserverDesc`], or null.
/// - When `out_entity` is non-null, it must point to writable [`FfiEntity`] storage.
pub unsafe extern "C" fn host_cmd_observe(
    commands: *mut BamCommands,
    desc: *const BamObserverDesc,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    // SAFETY: Caller guarantees writable `FfiEntity` storage if non-null.
    let out_slot = ffi_try!(unsafe { init_opt_out_ptr(out_entity, FfiEntity::NULL) });
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamObserverDesc::MIN_V0_2_SIZE`.
    let parsed = ffi_try!(unsafe { read_descriptor::<BamObserverDesc>(desc.cast::<c_void>()) });
    let val = parsed.validate();
    if val.is_err() {
        return val;
    }

    let snap = host_cmds.snapshot();
    let registry = host_cmds.registry().cloned().unwrap_or_default();
    let is_active = host_cmds
        .is_active()
        .cloned()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(true)));
    let mod_id = host_cmds.owner_mod_id();

    // SAFETY: `parsed` passed validation; `name` and `watched_components` satisfy caller contract.
    let binding = match unsafe {
        build_observer_binding_from_desc(&parsed, snap, registry, is_active, mod_id)
    } {
        Ok(b) => b,
        Err((status, _msg)) => return status,
    };

    let obs_entity = host_cmds.reserve_entity();
    if let Some(slot) = out_slot {
        *slot = FfiEntity::from_bits(obs_entity.to_bits());
    }

    host_cmds.queue_custom(move |world, _snap| {
        if !binding.is_active.load(Ordering::Acquire) {
            return;
        }
        let obs = build_observer_component(&binding);
        let obs_state = ModObserverState::new_in_world(world, binding);
        if let Ok(mut e) = world.spawn_empty_at(obs_entity) {
            e.insert((obs, obs_state));
        } else {
            return;
        }
        if let Some(id) = mod_id
            && let Some(mut mod_host) = world.get_resource_mut::<ModHost>()
            && let Some(loaded) = mod_host.get_mod_mut(id)
        {
            loaded.observer_entities.push(obs_entity);
        }
        world.flush();
    });

    FfiResult::OK
}

#[cfg(test)]
#[path = "../../tests/unit/observers_ext_tests.rs"]
mod tests;
