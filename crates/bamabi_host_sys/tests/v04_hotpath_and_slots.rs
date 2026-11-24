//! Comprehensive integration test suite for ABI `0.4.0` (`0x0004_0000`) FFI slots `73..=89`,
//! `BamObserverDesc::ctx_mut_callback`, `BamScheduleSetDesc` (`run_if_ctx`, `parent_sets`),
//! `CascadeHierarchyCache` in `host_iter_up_cascade`, `RandomQueryView` `Or<...>` filtering,
//! `CommandArenaConfig`, and `LoadContext::allows_system`.

use bamabi_host_sys::{
    BamQuerySpecOwned, CommandArenaConfig, ComponentOptions, GuestSystem, HOST_API, LoadContext,
    ModHost, ModId, ModRegistry, ModScheduleMap, ModScheduleSetRegistry, ModScoped,
    ensure_children_registered_in_world, ensure_relation_graph_in_world, host_bind_named_object,
    host_cmd_insert_relation, host_cmd_observe, host_configure_schedule_set, host_intern_symbol,
    host_iter_entities, host_iter_next_chunk, host_iter_up_cascade, host_load_host_ctx,
    host_mount_system_in_schedule, host_msg_send, host_register_observer, host_register_relation,
    host_register_system_handle, host_sys_entity_get, host_sys_host_ctx, host_sys_query,
    host_sys_res_ptr, mount_mod_systems_in_world,
};
use bamabi_macros::ModComponent;
use bamabi_sys::{
    AccessMode, BAM_HOST_API_SLOT_COUNT, BAM_OBSERVER_FLAG_PROPAGATE,
    BAM_SCHEDULE_SET_FLAG_PARALLEL, BAM_SPAWN_COL_FLAG_SPLAT, BamCommands, BamComponent,
    BamHostApi, BamIter, BamObserverDesc, BamParOptions, BamQueryDesc, BamRelationDesc,
    BamRowRange, BamScheduleSetDesc, BamSpawnColumnDesc, BamSymbol, BamSystem, BamSystemCtx,
    BamSystemDesc, BamSystemSet, BamTermDesc, BamTimeInfo, ComponentDesc, ComponentUuid, FfiBool,
    FfiEntity, FfiResult, ScheduleId, StrView,
};
use bevy_ecs::{
    component::Component, entity::Entity, event::Event, hierarchy::ChildOf, prelude::Resource,
    schedule::ScheduleLabel, system::System, world::World,
};
use core::ffi::c_void;
use core::ptr::{null, null_mut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "v04_test::Pos")]
struct Pos {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[component(storage = "SparseSet")]
#[mod_component(name = "v04_test::SparseVel")]
struct SparseVel {
    dx: f32,
    dy: f32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "v04_test::Team")]
struct Team {
    id: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "v04_test::Tint")]
struct Tint {
    rgba: u32,
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "v04_test::GameTickRes")]
struct GameTickRes {
    tick: u64,
}

#[repr(C)]
#[derive(Event, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "v04_test::CtxMutPing")]
struct CtxMutPing {
    value: i32,
}

#[test]
fn host_api_exposes_all_90_v04_slots() {
    assert_eq!(BAM_HOST_API_SLOT_COUNT, 90);
    assert_eq!(
        core::mem::size_of::<BamHostApi>(),
        BamHostApi::MIN_V0_4_SIZE
    );
    assert!(HOST_API.has_all_v0_2_slots());
    assert!(HOST_API.has_all_v0_3_slots());
    assert!(HOST_API.has_all_v0_4_slots());
    assert!(HOST_API.has_all_slots());
}

#[test]
fn slots_73_74_75_76_and_system_filter() {
    let mut world = World::new();
    let reg = ModRegistry::new();
    let is_active = Arc::new(AtomicBool::new(true));
    let mut load_ctx = LoadContext::with_manifest_name(
        reg,
        is_active,
        &raw mut world,
        Some(ModId(7)),
        Some(Arc::from("v04_mod")),
    )
    .with_system_filter(["allowed_sys"]);

    // SAFETY: `load_ctx` is active inside init window; descriptors & out pointers are valid.
    unsafe {
        let host_ctx = host_load_host_ctx(load_ctx.as_ffi_ptr());
        assert!(!host_ctx.is_null());

        // Slot 73: register_resource
        let res_uuid = ComponentUuid::from_canonical_name("v04_mod::DynRes");
        let res_desc = ComponentDesc::new(c"DynRes".as_ptr(), res_uuid, 8, 4);
        let mut res_handle = BamComponent::INVALID;
        let reg_res = HOST_API.register_resource.unwrap();
        assert_eq!(
            reg_res(load_ctx.as_ffi_ptr(), &res_desc, &raw mut res_handle),
            FfiResult::OK
        );
        assert!(res_handle.is_valid());
        assert!(
            load_ctx
                .registry
                .snapshot()
                .get_by_handle(res_handle)
                .unwrap()
                .is_resource()
        );

        // Slot 74: register_message
        let msg_uuid = ComponentUuid::from_canonical_name("v04_mod::DynMsg");
        let msg_desc = ComponentDesc::new(c"DynMsg".as_ptr(), msg_uuid, 4, 4);
        let mut msg_handle = BamComponent::INVALID;
        let reg_msg = HOST_API.register_message.unwrap();
        assert_eq!(
            reg_msg(load_ctx.as_ffi_ptr(), &msg_desc, &raw mut msg_handle),
            FfiResult::OK
        );
        assert!(msg_handle.is_valid());
        assert!(
            load_ctx
                .registry
                .snapshot()
                .get_by_handle(msg_handle)
                .unwrap()
                .is_message()
        );

        // Slot 75: resolve_named_object
        let host_ctx = host_load_host_ctx(load_ctx.as_ffi_ptr());
        let entity = world.spawn_empty().id();
        let name_view = StrView::from_str("spawn::boss");
        let missing_view = StrView::from_str("spawn::missing");
        assert_eq!(
            host_bind_named_object(host_ctx, &name_view, FfiEntity::from_bits(entity.to_bits())),
            FfiResult::OK
        );
        let resolve_obj = HOST_API.resolve_named_object.unwrap();
        let mut out_entity = FfiEntity::NULL;
        assert_eq!(
            resolve_obj(host_ctx, &name_view, &raw mut out_entity),
            FfiResult::OK
        );
        assert_eq!(out_entity.to_bits(), entity.to_bits());
        assert_eq!(
            resolve_obj(host_ctx, &missing_view, &raw mut out_entity),
            FfiResult::NOT_FOUND
        );
        assert_eq!(out_entity, FfiEntity::NULL);

        // Slot 76: resolve_symbol_str
        let sym_in = StrView::from_str("combat.damage");
        let mut sym = BamSymbol::INVALID;
        assert_eq!(
            host_intern_symbol(host_ctx, &sym_in, &raw mut sym),
            FfiResult::OK
        );
        let resolve_sym = HOST_API.resolve_symbol_str.unwrap();
        let mut out_view = StrView::empty();
        assert_eq!(resolve_sym(host_ctx, sym, &raw mut out_view), FfiResult::OK);
        assert_eq!(out_view.as_str().unwrap(), "combat.damage");
        assert_eq!(
            resolve_sym(host_ctx, BamSymbol::INVALID, &raw mut out_view),
            FfiResult::INVALID_ARGUMENT
        );

        // LoadContext::allows_system filtering
        assert!(load_ctx.allows_system("allowed_sys"));
        assert!(!load_ctx.allows_system("blocked_sys"));
        unsafe extern "C" fn noop_sys(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
            FfiResult::OK
        }
        let blocked = BamSystemDesc::new(c"blocked_sys".as_ptr(), null(), 0, noop_sys, null_mut());
        let mut sys_h = BamSystem::INVALID;
        assert_eq!(
            host_register_system_handle(load_ctx.as_ffi_ptr(), &blocked, &raw mut sys_h),
            FfiResult::PERMISSION_DENIED
        );
        let allowed = BamSystemDesc::new(c"allowed_sys".as_ptr(), null(), 0, noop_sys, null_mut());
        assert_eq!(
            host_register_system_handle(load_ctx.as_ffi_ptr(), &allowed, &raw mut sys_h),
            FfiResult::OK
        );
    }
    load_ctx.finish_init();
}

#[test]
fn slots_77_78_79_80_sparse_untracked_mark_ranges_ticks_and_time_info() {
    #[repr(C)]
    struct SysCheck {
        last_run: u32,
        this_run: u32,
        delta: f32,
        fixed_delta: f32,
        elapsed: f64,
        overstep: f32,
        marked_entities: usize,
    }

    unsafe extern "C" fn sparse_and_time_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `SysCheck` valid for this call.
        let out = unsafe { &mut *(user_data.cast::<SysCheck>()) };
        let mut iter: *mut BamIter = null_mut();
        // SAFETY: `sys_ctx` is active with query 0 (`SparseVel`).
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }

        // Slot 79: sys_current_ticks
        // SAFETY: `sys_ctx` is active and output pointers are valid.
        let rc = unsafe {
            (HOST_API.sys_current_ticks.unwrap())(
                sys_ctx,
                &raw mut out.last_run,
                &raw mut out.this_run,
            )
        };
        if rc.is_err() {
            return rc;
        }

        // Slot 80: sys_time_info
        let mut tinfo = BamTimeInfo::new();
        // SAFETY: `sys_ctx` is active and `tinfo` is initialized with `MIN_V0_4_SIZE`.
        let rc = unsafe { (HOST_API.sys_time_info.unwrap())(sys_ctx, &raw mut tinfo) };
        if rc.is_err() {
            return rc;
        }
        out.delta = tinfo.delta_secs;
        out.fixed_delta = tinfo.fixed_delta_secs;
        out.elapsed = tinfo.elapsed_secs;
        out.overstep = tinfo.overstep_fraction;

        let mut chunk_len = 0usize;
        // SAFETY: `iter` is active.
        while unsafe { host_iter_next_chunk(iter, &mut chunk_len) }.get() {
            // Slot 77: iter_column_rows_mutable_untracked
            // SAFETY: `iter` is active on a chunk of `chunk_len` sparse rows.
            let row_ptrs_ptr =
                unsafe { (HOST_API.iter_column_rows_mutable_untracked.unwrap())(iter, 0) };
            if row_ptrs_ptr.is_null() {
                return FfiResult::NULL_POINTER;
            }
            // SAFETY: `row_ptrs_ptr` points to `chunk_len` valid `*mut u8` row pointers.
            let row_ptrs_mut = unsafe { core::slice::from_raw_parts(row_ptrs_ptr, chunk_len) };
            for &ptr in row_ptrs_mut {
                // SAFETY: `ptr` points to valid `SparseVel` storage.
                unsafe {
                    let mut vel = core::ptr::read_unaligned(ptr.cast::<SparseVel>());
                    vel.dx += 5.0;
                    core::ptr::write_unaligned(ptr.cast::<SparseVel>(), vel);
                }
            }
            // Slot 78: iter_mark_changed_ranges (mark rows 0..1 and 2..3 if present)
            let ranges = [
                BamRowRange::new(0, 1),
                BamRowRange::new(2, if chunk_len >= 3 { 1 } else { 0 }),
            ];
            let range_count = if chunk_len >= 3 { 2 } else { 1 };
            // SAFETY: `iter` and `ranges` are valid for `range_count`.
            let rc = unsafe {
                (HOST_API.iter_mark_changed_ranges.unwrap())(iter, 0, ranges.as_ptr(), range_count)
            };
            if rc.is_err() {
                return rc;
            }
            out.marked_entities += if chunk_len >= 3 { 2 } else { 1 };
        }
        FfiResult::OK
    }

    let mut world = World::new();
    let vel_id = world.register_component::<SparseVel>();
    let time_desc = {
        let mut reg = ModRegistry::new();
        let vel_h = reg
            .register_component::<SparseVel>(vel_id, ComponentOptions::new().sparse_set())
            .unwrap();
        let mut time_info = BamTimeInfo::new();
        time_info.delta_secs = 0.016;
        time_info.fixed_delta_secs = 0.02;
        time_info.elapsed_secs = 4.5;
        time_info.overstep_fraction = 0.25;
        // Register `"bamabi::TimeInfo"` dynamically as a resource so slot 80 finds it cleanly.
        let mut lc = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            &raw mut world,
            None,
        );
        let mut time_h = BamComponent::INVALID;
        let desc = ComponentDesc::new(
            c"bamabi::TimeInfo".as_ptr(),
            ComponentUuid::from_canonical_name("bamabi::TimeInfo"),
            core::mem::size_of::<BamTimeInfo>(),
            core::mem::align_of::<BamTimeInfo>(),
        );
        // SAFETY: Valid descriptor & output handle.
        unsafe {
            assert_eq!(
                (HOST_API.register_resource.unwrap())(lc.as_ffi_ptr(), &desc, &raw mut time_h),
                FfiResult::OK
            );
        }
        let reg_after = lc.registry.clone();
        lc.finish_init();
        let time_cid = reg_after
            .snapshot()
            .get_by_handle(time_h)
            .unwrap()
            .component_id;
        world.insert_resource(reg_after);
        // SAFETY: `time_cid` was registered with `Layout::new::<BamTimeInfo>()`.
        unsafe {
            let mut boxed = core::mem::ManuallyDrop::new(time_info);
            let nn = core::ptr::NonNull::from(&mut *boxed).cast::<u8>();
            world.insert_resource_by_id(
                time_cid,
                bevy_ecs::ptr::OwningPtr::new(nn),
                bevy_ecs::change_detection::MaybeLocation::caller(),
            );
        }
        (vel_h, world.resource::<ModRegistry>().clone())
    };
    let (vel_h, reg) = time_desc;

    let ents: Vec<Entity> = (0..4)
        .map(|i| {
            world
                .spawn(SparseVel {
                    dx: i as f32,
                    dy: 0.0,
                })
                .id()
        })
        .collect();

    let mut check = SysCheck {
        last_run: 0,
        this_run: 0,
        delta: 0.0,
        fixed_delta: 0.0,
        elapsed: 0.0,
        overstep: 0.0,
        marked_entities: 0,
    };

    let mut sys = GuestSystem::new(
        "sparse_time_sys",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            vel_h,
            AccessMode::WRITE,
        )])],
        sparse_and_time_cb,
        (&raw mut check).cast::<c_void>(),
        reg.clone(),
    );
    sys.set_error_policy(bamabi_host_sys::ModErrorPolicy::ReportToBevy);
    sys.initialize(&mut world);
    assert!(
        sys.init_error().is_none(),
        "init_error: {:?}",
        sys.init_error()
    );
    // Advance tick once so entities are not all "newly added" on first run.
    sys.run((), &mut world).unwrap();
    assert!(sys.is_enabled(), "sys faulted on run 1");

    // Clear check and run second tick: now CHANGED query should see ONLY rows 0 and 2!
    let prev_changed = world.change_tick();
    check.marked_entities = 0;
    sys.run((), &mut world).unwrap();
    assert!(sys.is_enabled(), "sys faulted on run 2");
    assert!(
        check.this_run >= check.last_run,
        "this_run={} last_run={}",
        check.this_run,
        check.last_run
    );
    assert!((check.delta - 0.016).abs() < 1e-6);
    assert!((check.fixed_delta - 0.02).abs() < 1e-6);
    assert!((check.elapsed - 4.5).abs() < 1e-9);
    assert!((check.overstep - 0.25).abs() < 1e-6);
    assert_eq!(check.marked_entities, 2);

    // Row 0 and 2 were marked changed; rows 1 and 3 were NOT marked changed!
    unsafe extern "C" fn count_changed_cb(ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        let mut iter: *mut BamIter = null_mut();
        // SAFETY: `ctx` is active.
        if unsafe { host_sys_query(ctx, 0, &mut iter) }.is_err() {
            return FfiResult::INTERNAL_ERROR;
        }
        let mut len = 0usize;
        // SAFETY: `iter` is active for this system invocation.
        while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {
            // SAFETY: `ud` points to live `AtomicUsize`.
            unsafe { (*(ud.cast::<AtomicUsize>())).fetch_add(len, Ordering::Relaxed) };
        }
        FfiResult::OK
    }
    let counter = AtomicUsize::new(0);
    let mut changed_sys = GuestSystem::new(
        "changed_check",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            vel_h,
            AccessMode::CHANGED,
        )])],
        count_changed_cb,
        (&raw const counter).cast_mut().cast::<c_void>(),
        reg,
    );
    changed_sys.initialize(&mut world);
    // Re-sync `changed_sys` last_run just after `prev_changed`
    let _ = changed_sys.run((), &mut world);
    let initial_changed = counter.swap(0, Ordering::Relaxed);
    assert!(initial_changed >= 2);
    // Run `sys` again and verify `changed_sys` sees exactly 2 entities (rows 0 & 2)!
    let _ = sys.run((), &mut world);
    let _ = changed_sys.run((), &mut world);
    assert_eq!(counter.load(Ordering::Relaxed), 2);
    let _ = (ents, prev_changed);
}

#[test]
fn slot_81_par_for_each_chunk_ex_partition_commands_merge_deterministically() {
    struct ParExState {
        tint_h: BamComponent,
        part_sum: AtomicUsize,
    }

    unsafe extern "C" fn worker_cb(
        iter: *mut BamIter,
        worker_cmds: *mut BamCommands,
        partition_idx: usize,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `ParExState` across the joined `pool.scope`.
        let st = unsafe { &*(user_data.cast::<ParExState>()) };
        st.part_sum.fetch_add(partition_idx + 1, Ordering::Relaxed);
        let mut chunk_len = 0usize;
        // SAFETY: `iter` is active for this partition.
        while unsafe { host_iter_next_chunk(iter, &mut chunk_len) }.get() {
            // SAFETY: `iter` is active and has `chunk_len` entities.
            let ents_ptr = unsafe { host_iter_entities(iter) };
            for i in 0..chunk_len {
                // SAFETY: `ents_ptr` has `chunk_len` elements.
                let ent = unsafe { *ents_ptr.add(i) };
                let tint = Tint {
                    rgba: (partition_idx as u32) + 100,
                };
                // SAFETY: `worker_cmds` belongs exclusively to this worker partition.
                let rc = unsafe {
                    (HOST_API.cmd_insert.unwrap())(
                        worker_cmds,
                        ent,
                        st.tint_h,
                        (&raw const tint).cast::<u8>(),
                        core::mem::size_of::<Tint>(),
                    )
                };
                if rc.is_err() {
                    return rc;
                }
            }
        }
        FfiResult::OK
    }

    unsafe extern "C" fn sys_cb(sys_ctx: *mut BamSystemCtx, user_data: *mut c_void) -> FfiResult {
        let mut iter: *mut BamIter = null_mut();
        // SAFETY: `sys_ctx` is active.
        let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
        if rc.is_err() {
            return rc;
        }
        let opts = BamParOptions::new().with_chunk_size(4);
        // SAFETY: `iter`, `worker_cb`, `user_data`, and `opts` are valid.
        unsafe {
            (HOST_API.par_for_each_chunk_ex.unwrap())(iter, Some(worker_cb), user_data, &opts)
        }
    }

    let mut world = World::new();
    let pos_id = world.register_component::<Pos>();
    let tint_id = world.register_component::<Tint>();
    let mut reg = ModRegistry::new();
    let pos_h = reg
        .register_component::<Pos>(pos_id, Default::default())
        .unwrap();
    let tint_h = reg
        .register_component::<Tint>(tint_id, Default::default())
        .unwrap();
    world.insert_resource(reg.clone());

    let ents: Vec<Entity> = (0..10)
        .map(|i| {
            world
                .spawn(Pos {
                    x: i as f32,
                    y: 0.0,
                })
                .id()
        })
        .collect();

    let state = ParExState {
        tint_h,
        part_sum: AtomicUsize::new(0),
    };
    let mut sys = GuestSystem::new(
        "par_ex_sys",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            pos_h,
            AccessMode::READ,
        )])],
        sys_cb,
        (&raw const state).cast_mut().cast::<c_void>(),
        reg,
    );
    sys.initialize(&mut world);
    let _ = sys.run((), &mut world);

    // 10 entities with chunk_size = 4 -> 3 partitions (indices 0, 1, 2 -> sum (1+2+3) = 6).
    assert_eq!(state.part_sum.load(Ordering::Relaxed), 6);
    for &e in &ents {
        let tint = world.get::<Tint>(e).expect("worker command merged Tint");
        assert!(tint.rgba >= 100 && tint.rgba <= 102);
    }
}

#[test]
fn slots_82_83_84_relations_and_cascade_hierarchy_cache() {
    unsafe extern "C" fn rel_query_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        #[repr(C)]
        struct RelCtx {
            rel: BamComponent,
            root: FfiEntity,
            child: FfiEntity,
            targets_len: usize,
            sources_len: usize,
            edge_val: u64,
            cascade_owner: FfiEntity,
            cascade_x: f32,
        }
        // SAFETY: `user_data` points to `RelCtx`.
        let ctx_data = unsafe { &mut *(user_data.cast::<RelCtx>()) };
        let mut targets: *const FfiEntity = null();
        // SAFETY: Active `sys_ctx` and valid output pointers.
        let rc = unsafe {
            (HOST_API.sys_relation_targets.unwrap())(
                sys_ctx,
                ctx_data.root,
                ctx_data.rel,
                &raw mut targets,
                &raw mut ctx_data.targets_len,
            )
        };
        if rc.is_err() {
            return rc;
        }
        let mut sources: *const FfiEntity = null();
        // SAFETY: Active `sys_ctx` and valid output pointers.
        let rc = unsafe {
            (HOST_API.sys_relation_sources.unwrap())(
                sys_ctx,
                ctx_data.child,
                ctx_data.rel,
                &raw mut sources,
                &raw mut ctx_data.sources_len,
            )
        };
        if rc.is_err() {
            return rc;
        }
        let mut edge_ptr: *const c_void = null();
        let mut edge_size = 0usize;
        // SAFETY: Active `sys_ctx` and valid output pointers.
        let rc = unsafe {
            (HOST_API.sys_relation_edge_data.unwrap())(
                sys_ctx,
                ctx_data.root,
                ctx_data.rel,
                ctx_data.child,
                &raw mut edge_ptr,
                &raw mut edge_size,
            )
        };
        if rc.is_err() {
            return rc;
        }
        if edge_size == 8 && !edge_ptr.is_null() {
            // SAFETY: Verified 8-byte u64 payload.
            ctx_data.edge_val = unsafe { core::ptr::read_unaligned(edge_ptr.cast::<u64>()) };
        }

        // Also test `host_iter_up_cascade` across the custom relation!
        let mut iter: *mut BamIter = null_mut();
        // SAFETY: Query 0 declares `Pos` read access.
        if unsafe { host_sys_query(sys_ctx, 0, &mut iter) }.is_err() {
            return FfiResult::INTERNAL_ERROR;
        }
        let mut pos_ptr: *const c_void = null();
        // Walk from `child` up along `ChildOf` (`BamComponent::INVALID`) to find `Pos` on `root`.
        // SAFETY: `iter` is active and output pointers are valid.
        let rc = unsafe {
            host_iter_up_cascade(
                iter,
                BamComponent::INVALID,
                ctx_data.child,
                0,
                &raw mut pos_ptr,
                &raw mut ctx_data.cascade_owner,
            )
        };
        if rc.is_ok() && !pos_ptr.is_null() {
            // SAFETY: `pos_ptr` points to `Pos`.
            ctx_data.cascade_x = unsafe { (*(pos_ptr.cast::<Pos>())).x };
        }
        FfiResult::OK
    }

    let mut world = World::new();
    ensure_children_registered_in_world(&mut world);
    ensure_relation_graph_in_world(&mut world);
    let pos_id = world.register_component::<Pos>();
    let mut reg = ModRegistry::new();
    let pos_h = reg
        .register_component::<Pos>(pos_id, Default::default())
        .unwrap();
    world.insert_resource(reg);

    let mut load_ctx = LoadContext::with_world_and_mod_id(
        world.resource::<ModRegistry>().clone(),
        Arc::new(AtomicBool::new(true)),
        &raw mut world,
        None,
    );
    let mut rel_h = BamComponent::INVALID;
    let rel_desc = BamRelationDesc::new(
        c"v04::Link".as_ptr(),
        ComponentUuid::from_canonical_name("v04::Link"),
    )
    .with_payload_layout(8, 8);
    // SAFETY: `load_ctx`, `rel_desc`, and `rel_h` are valid.
    unsafe {
        assert_eq!(
            host_register_relation(load_ctx.as_ffi_ptr(), &rel_desc, &raw mut rel_h),
            FfiResult::OK
        );
    }
    let reg = load_ctx.registry.clone();
    load_ctx.finish_init();
    world.insert_resource(reg.clone());

    let root = world.spawn(Pos { x: 42.5, y: 1.0 }).id();
    let child = world.spawn(ChildOf(root)).id();

    // Insert edge `(root -> child)` with u64 payload `0xDEAD_BEEF_1234_5678` via `host_cmd_insert_relation`.
    {
        let mut storage = bamabi_host_sys::HostCommandsStorage::new();
        let snap = reg.snapshot_arc();
        {
            let mut cmds =
                bamabi_host_sys::HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let payload = 0xDEAD_BEEF_1234_5678u64;
            // SAFETY: Valid `cmds` and payload.
            unsafe {
                assert_eq!(
                    host_cmd_insert_relation(
                        cmds.as_ffi_ptr(),
                        FfiEntity::from_bits(root.to_bits()),
                        rel_h,
                        FfiEntity::from_bits(child.to_bits()),
                        (&raw const payload).cast::<c_void>(),
                        8,
                    ),
                    FfiResult::OK
                );
            }
        }
        storage.apply_to_world(&mut world, &snap);
    }

    #[repr(C)]
    struct RelCtx {
        rel: BamComponent,
        root: FfiEntity,
        child: FfiEntity,
        targets_len: usize,
        sources_len: usize,
        edge_val: u64,
        cascade_owner: FfiEntity,
        cascade_x: f32,
    }
    let mut rctx = RelCtx {
        rel: rel_h,
        root: FfiEntity::from_bits(root.to_bits()),
        child: FfiEntity::from_bits(child.to_bits()),
        targets_len: 0,
        sources_len: 0,
        edge_val: 0,
        cascade_owner: FfiEntity::NULL,
        cascade_x: 0.0,
    };

    let mut sys = GuestSystem::new(
        "rel_sys",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            pos_h,
            AccessMode::READ,
        )])],
        rel_query_cb,
        (&raw mut rctx).cast::<c_void>(),
        reg,
    );
    sys.initialize(&mut world);
    let _ = sys.run((), &mut world);

    assert_eq!(rctx.targets_len, 1);
    assert_eq!(rctx.sources_len, 1);
    assert_eq!(rctx.edge_val, 0xDEAD_BEEF_1234_5678);
    assert_eq!(rctx.cascade_owner.to_bits(), root.to_bits());
    assert!((rctx.cascade_x - 42.5).abs() < 1e-6);
}

#[test]
fn slots_85_86_87_88_89_batch_commands_and_zero_copy_msg_slice() {
    let mut world = World::new();
    world.insert_resource(CommandArenaConfig {
        max_retained_slabs: 1,
    });
    let pos_id = world.register_component::<Pos>();
    let team_id = world.register_component::<Team>();
    let mut reg = ModRegistry::new();
    let pos_h = reg
        .register_component::<Pos>(pos_id, Default::default())
        .unwrap();
    let team_h = reg
        .register_component::<Team>(team_id, Default::default())
        .unwrap();
    world.insert_resource(reg.clone());

    // Dynamically register a buffered message stream for slot 88.
    let mut lc = LoadContext::with_world_and_mod_id(
        reg,
        Arc::new(AtomicBool::new(true)),
        &raw mut world,
        Some(ModId(9)),
    );
    let msg_desc = ComponentDesc::new(
        c"v04_test::DynEvt".as_ptr(),
        ComponentUuid::from_canonical_name("v04_test::DynEvt"),
        4,
        4,
    );
    let mut msg_h = BamComponent::INVALID;
    // SAFETY: Valid `lc`, `msg_desc`, and `msg_h`.
    unsafe {
        assert_eq!(
            (HOST_API.register_message.unwrap())(lc.as_ffi_ptr(), &msg_desc, &raw mut msg_h),
            FfiResult::OK
        );
    }
    let reg = lc.registry.clone();
    lc.finish_init();
    world.insert_resource(reg.clone());

    // Push 3 messages into `DynEvt` and verify slot 88 (`msg_read_slice`) reads zero-copy.
    unsafe extern "C" fn msg_slice_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let mut iter: *mut BamIter = null_mut();
        // SAFETY: `sys_ctx` is active.
        if unsafe { host_sys_query(sys_ctx, 0, &mut iter) }.is_err() {
            return FfiResult::INTERNAL_ERROR;
        }
        let mut ptr: *const u8 = null();
        let mut count = 0usize;
        // SAFETY: Active `iter`, valid outputs.
        let rc =
            unsafe { (HOST_API.msg_read_slice.unwrap())(iter, 0, &raw mut ptr, &raw mut count) };
        if rc.is_err() {
            return rc;
        }
        if count > 0 && !ptr.is_null() {
            let mut sum = 0u32;
            for i in 0..count {
                // SAFETY: `ptr` holds `count` aligned 4-byte `u32` entries.
                sum += unsafe { core::ptr::read_unaligned(ptr.add(i * 4).cast::<u32>()) };
            }
            // SAFETY: `user_data` points to `AtomicUsize`.
            unsafe { (*(user_data.cast::<AtomicUsize>())).store(sum as usize, Ordering::Relaxed) };
        }
        FfiResult::OK
    }

    // Use a writer GuestSystem + reader GuestSystem to exercise `host_msg_send` + `msg_read_slice`.
    unsafe extern "C" fn send_msgs_cb(sys_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        let mut iter: *mut BamIter = null_mut();
        // SAFETY: Active `sys_ctx`.
        if unsafe { host_sys_query(sys_ctx, 0, &mut iter) }.is_err() {
            return FfiResult::INTERNAL_ERROR;
        }
        for v in [10u32, 20u32, 30u32] {
            // SAFETY: `iter` is active and `v` is 4 bytes.
            let rc = unsafe { host_msg_send(iter, 0, (&raw const v).cast::<u8>(), 4) };
            if rc.is_err() {
                return rc;
            }
        }
        FfiResult::OK
    }
    let mut writer_sys = GuestSystem::new(
        "msg_writer",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            msg_h,
            AccessMode::MESSAGE_WRITE,
        )])],
        send_msgs_cb,
        null_mut(),
        reg.clone(),
    );
    writer_sys.set_error_policy(bamabi_host_sys::ModErrorPolicy::ReportToBevy);
    writer_sys.initialize(&mut world);
    assert!(
        writer_sys.init_error().is_none(),
        "writer init_error: {:?}",
        writer_sys.init_error()
    );
    writer_sys.run((), &mut world).unwrap();
    assert!(writer_sys.is_enabled(), "writer_sys faulted");

    let msg_sum = AtomicUsize::new(0);
    let mut reader_sys = GuestSystem::new(
        "msg_reader",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            msg_h,
            AccessMode::MESSAGE_READ,
        )])],
        msg_slice_cb,
        (&raw const msg_sum).cast_mut().cast::<c_void>(),
        reg.clone(),
    );
    reader_sys.initialize(&mut world);
    assert!(
        reader_sys.init_error().is_none(),
        "reader init_error: {:?}",
        reader_sys.init_error()
    );
    reader_sys.run((), &mut world).unwrap();
    assert!(reader_sys.is_enabled(), "reader_sys faulted");
    assert_eq!(msg_sum.load(Ordering::Relaxed), 60);

    // Exercise slots 89 (`cmd_spawn_batch_ex`), 86 (`cmd_insert_batch`), 87 (`cmd_remove_batch`), 85 (`cmd_despawn_batch`)!
    let mut storage = bamabi_host_sys::HostCommandsStorage::new();
    let snap = reg.snapshot_arc();
    let mut spawned_ffi = [FfiEntity::NULL; 4];
    {
        let mut cmds =
            bamabi_host_sys::HostCommands::new(&mut storage, world.entity_allocator(), &snap)
                .with_mod_context(Some(ModId(9)), Some(Arc::new(AtomicBool::new(true))));
        let pos_col = [
            Pos { x: 1.0, y: 0.0 },
            Pos { x: 2.0, y: 0.0 },
            Pos { x: 3.0, y: 0.0 },
            Pos { x: 4.0, y: 0.0 },
        ];
        let splat_team = Team { id: 77 };
        let cols = [
            BamSpawnColumnDesc::dense(
                pos_h,
                pos_col.as_ptr().cast::<u8>(),
                core::mem::size_of::<Pos>(),
            ),
            BamSpawnColumnDesc::splat(
                team_h,
                (&raw const splat_team).cast::<u8>(),
                core::mem::size_of::<Team>(),
            ),
        ];
        // SAFETY: Valid `cmds`, `cols`, and `spawned_ffi`.
        unsafe {
            assert_eq!(
                (HOST_API.cmd_spawn_batch_ex.unwrap())(
                    cmds.as_ffi_ptr(),
                    cols.as_ptr(),
                    cols.len(),
                    4,
                    spawned_ffi.as_mut_ptr(),
                ),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);

    for (idx, ffi_e) in spawned_ffi.iter().enumerate() {
        let e = Entity::from_bits(ffi_e.to_bits());
        assert_eq!(world.get::<Pos>(e).unwrap().x, (idx + 1) as f32);
        assert_eq!(world.get::<Team>(e).unwrap().id, 77);
        assert!(world.get::<ModScoped>(e).is_some());
    }

    // Slot 86: cmd_insert_batch (splat Team { id: 99 } onto first 2 entities)
    // Slot 87: cmd_remove_batch (remove Pos from entities 2..4)
    {
        let mut cmds =
            bamabi_host_sys::HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let new_team = Team { id: 99 };
        // SAFETY: Valid `cmds`, entities, and splat payload.
        unsafe {
            assert_eq!(
                (HOST_API.cmd_insert_batch.unwrap())(
                    cmds.as_ffi_ptr(),
                    spawned_ffi[0..2].as_ptr(),
                    2,
                    team_h,
                    (&raw const new_team).cast::<u8>(),
                    core::mem::size_of::<Team>(),
                    BAM_SPAWN_COL_FLAG_SPLAT,
                ),
                FfiResult::OK
            );
            assert_eq!(
                (HOST_API.cmd_remove_batch.unwrap())(
                    cmds.as_ffi_ptr(),
                    spawned_ffi[2..4].as_ptr(),
                    2,
                    pos_h,
                ),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(
        world
            .get::<Team>(Entity::from_bits(spawned_ffi[0].to_bits()))
            .unwrap()
            .id,
        99
    );
    assert_eq!(
        world
            .get::<Team>(Entity::from_bits(spawned_ffi[1].to_bits()))
            .unwrap()
            .id,
        99
    );
    assert!(
        world
            .get::<Pos>(Entity::from_bits(spawned_ffi[2].to_bits()))
            .is_none()
    );
    assert!(
        world
            .get::<Pos>(Entity::from_bits(spawned_ffi[3].to_bits()))
            .is_none()
    );

    // Slot 85: cmd_despawn_batch
    {
        let mut cmds =
            bamabi_host_sys::HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        // SAFETY: Valid `cmds` and `spawned_ffi`.
        unsafe {
            assert_eq!(
                (HOST_API.cmd_despawn_batch.unwrap())(
                    cmds.as_ffi_ptr(),
                    spawned_ffi.as_ptr(),
                    spawned_ffi.len(),
                ),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    for ffi_e in spawned_ffi {
        assert!(
            world
                .get_entity(Entity::from_bits(ffi_e.to_bits()))
                .is_err()
        );
    }
}

#[test]
fn ctx_mut_observer_and_schedule_set_run_if_and_parent_sets() {
    #[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
    struct TestUpdate;

    let mut world = World::new();
    world.init_resource::<ModHost>();
    let custom_sched = ScheduleId::custom(1024);
    let mut sched_map = ModScheduleMap::new();
    assert!(sched_map.register(custom_sched, TestUpdate));
    world.insert_resource(sched_map);
    world.init_resource::<ModScheduleSetRegistry>();

    let evt_key = world.register_event_key::<CtxMutPing>();
    world.insert_resource(GameTickRes { tick: 7 });
    let res_id = world.components().component_id::<GameTickRes>().unwrap();
    let mut reg = ModRegistry::new();
    let evt_h = reg.register_event::<CtxMutPing>(evt_key).unwrap();
    let res_h = reg.register_resource::<GameTickRes>(res_id).unwrap();
    world.insert_resource(reg);

    let mut load_ctx = LoadContext::with_manifest_name(
        world.resource::<ModRegistry>().clone(),
        Arc::new(AtomicBool::new(true)),
        &raw mut world,
        None,
        Some(Arc::from("v04_sched")),
    );

    // 1. Register `ctx_mut_callback` observer on `CtxMutPing`.
    let obs_hits = AtomicUsize::new(0);
    unsafe extern "C" fn ping_ctx_mut_obs_real(
        _entity: FfiEntity,
        event_data: *mut u8,
        propagate: *mut FfiBool,
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `event_data` is `CtxMutPing`, `propagate` is stack `FfiBool`, `sys_ctx` is active.
        unsafe {
            let evt = core::ptr::read_unaligned(event_data.cast::<CtxMutPing>());
            let mut out_res = BamComponent::INVALID;
            let _ = bamabi_host_sys::host_resolve_component(
                host_sys_host_ctx(sys_ctx),
                c"v04_test::GameTickRes".as_ptr(),
                null(),
                &raw mut out_res,
            );
            *propagate = FfiBool::FALSE;
            let _ = evt;
            (*(user_data.cast::<AtomicUsize>())).fetch_add(1, Ordering::Relaxed);
        }
        FfiResult::OK
    }
    let obs_terms = [BamTermDesc::new(res_h, AccessMode::RESOURCE_READ)];
    let obs_queries = [BamQueryDesc::new(obs_terms.as_ptr(), obs_terms.len())];
    let mut obs_desc = BamObserverDesc::new_with_ctx(
        evt_h,
        {
            unsafe extern "C" fn unused(
                _e: FfiEntity,
                _d: *const c_void,
                _c: *mut BamSystemCtx,
                _u: *mut c_void,
            ) -> FfiResult {
                FfiResult::OK
            }
            unused
        },
        obs_queries.as_ptr(),
        obs_queries.len(),
        (&raw const obs_hits).cast_mut().cast::<c_void>(),
    );
    obs_desc.ctx_callback = None;
    obs_desc.ctx_mut_callback = Some(ping_ctx_mut_obs_real);
    obs_desc.flags = BAM_OBSERVER_FLAG_PROPAGATE;
    obs_desc.name = c"ping_ctx_mut".as_ptr();
    // SAFETY: `load_ctx` and `obs_desc` are valid.
    unsafe {
        assert_eq!(
            host_register_observer(load_ctx.as_ffi_ptr(), &obs_desc),
            FfiResult::OK
        );
    }
    {
        let mut storage = bamabi_host_sys::HostCommandsStorage::new();
        let snap = load_ctx.registry.snapshot_arc();
        {
            let mut cmds =
                bamabi_host_sys::HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let mut out_obs_ent = FfiEntity::NULL;
            // SAFETY: Valid `cmds`, `obs_desc`, and `out_obs_ent`.
            unsafe {
                assert_eq!(
                    host_cmd_observe(cmds.as_ffi_ptr(), &obs_desc, &raw mut out_obs_ent),
                    FfiResult::OK
                );
            }
        }
        storage.apply_to_world(&mut world, &snap);
    }

    // 2. Configure parent + child `BamScheduleSetDesc` with `run_if_ctx` + `run_if_query` + `parent_sets`!
    unsafe extern "C" fn set_cond_cb(sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        let res_handle = BamComponent::from_raw(ud as usize as u32);
        // SAFETY: `sys_ctx` is active with `GameTickRes` in query 0.
        let res_ptr = unsafe { host_sys_res_ptr(sys_ctx, res_handle, null_mut()) };
        if res_ptr.is_null() {
            return FfiResult::NOT_FOUND;
        }
        // SAFETY: `res_ptr` points to `GameTickRes`.
        let tick = unsafe { core::ptr::read_unaligned(res_ptr.cast::<GameTickRes>()).tick };
        if tick >= 10 {
            FfiResult::OK
        } else {
            FfiResult::NOT_FOUND
        }
    }

    let sys_runs = AtomicUsize::new(0);
    unsafe extern "C" fn gated_sys_cb(_sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        // SAFETY: `ud` points to `AtomicUsize`.
        unsafe { (*(ud.cast::<AtomicUsize>())).fetch_add(1, Ordering::Relaxed) };
        FfiResult::OK
    }

    let parent_desc = BamScheduleSetDesc::new(c"ParentSet".as_ptr())
        .with_flags(BAM_SCHEDULE_SET_FLAG_PARALLEL)
        .with_schedule(custom_sched);
    let mut parent_h = BamSystemSet::INVALID;
    // SAFETY: Valid `load_ctx`, `parent_desc`, `parent_h`.
    unsafe {
        assert_eq!(
            host_configure_schedule_set(load_ctx.as_ffi_ptr(), &parent_desc, &raw mut parent_h),
            FfiResult::OK
        );
    }

    let cond_terms = [BamTermDesc::new(res_h, AccessMode::RESOURCE_READ)];
    let cond_q = BamQueryDesc::new(cond_terms.as_ptr(), cond_terms.len());
    let child_parents = [parent_h];
    let child_desc = BamScheduleSetDesc::new(c"ChildSet".as_ptr())
        .with_schedule(custom_sched)
        .with_run_if_ctx(set_cond_cb, &cond_q, res_h.0 as usize as *mut c_void)
        .with_parent_sets(child_parents.as_ptr(), 1);
    let mut child_h = BamSystemSet::INVALID;
    // SAFETY: Valid `load_ctx`, `child_desc`, `child_h`.
    unsafe {
        assert_eq!(
            host_configure_schedule_set(load_ctx.as_ffi_ptr(), &child_desc, &raw mut child_h),
            FfiResult::OK
        );
    }

    let sys_desc = BamSystemDesc::new(
        c"gated_set_sys".as_ptr(),
        null(),
        0,
        gated_sys_cb,
        (&raw const sys_runs).cast_mut().cast::<c_void>(),
    )
    .with_schedule(custom_sched);
    let mut sys_h = BamSystem::INVALID;
    // SAFETY: Valid `load_ctx`, `sys_desc`, `sys_h`.
    unsafe {
        assert_eq!(
            host_register_system_handle(load_ctx.as_ffi_ptr(), &sys_desc, &raw mut sys_h),
            FfiResult::OK
        );
        assert_eq!(
            host_mount_system_in_schedule(load_ctx.as_ffi_ptr(), sys_h, custom_sched, child_h,),
            FfiResult::OK
        );
    }

    let systems = core::mem::take(&mut load_ctx.registered_systems);
    load_ctx.finish_init();

    // Verify observer spawned via `host_cmd_observe` runs `ctx_mut_callback`!
    world.trigger(CtxMutPing { value: 3 });
    assert_eq!(obs_hits.load(Ordering::Relaxed), 1);

    // Mount systems onto `TestUpdate` schedule and verify `run_if_ctx` gates when `tick < 10`, runs when `tick >= 10`!
    mount_mod_systems_in_world(&mut world, "v04_sched", 0, systems).unwrap();
    world.run_schedule(TestUpdate);
    assert_eq!(sys_runs.load(Ordering::Relaxed), 0);

    world.resource_mut::<GameTickRes>().tick = 10;
    world.run_schedule(TestUpdate);
    assert_eq!(sys_runs.load(Ordering::Relaxed), 1);
}

#[test]
fn random_query_view_or_groups_respects_filter_clauses() {
    unsafe extern "C" fn random_or_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        #[repr(C)]
        struct Pair {
            matching: FfiEntity,
            non_matching: FfiEntity,
            match_ok: bool,
            non_match_rejected: bool,
        }
        // SAFETY: `user_data` points to `Pair`.
        let p = unsafe { &mut *(user_data.cast::<Pair>()) };
        // SAFETY: `sys_ctx` is active and query 0 declares `Pos` READ + `Or<Team, Tint>`.
        let ptr1 = unsafe { host_sys_entity_get(sys_ctx, 0, p.matching, 0) };
        p.match_ok = !ptr1.is_null();
        // SAFETY: Same.
        let ptr2 = unsafe { host_sys_entity_get(sys_ctx, 0, p.non_matching, 0) };
        p.non_match_rejected = ptr2.is_null();
        FfiResult::OK
    }

    let mut world = World::new();
    let pos_id = world.register_component::<Pos>();
    let team_id = world.register_component::<Team>();
    let tint_id = world.register_component::<Tint>();
    let mut reg = ModRegistry::new();
    let pos_h = reg
        .register_component::<Pos>(pos_id, Default::default())
        .unwrap();
    let team_h = reg
        .register_component::<Team>(team_id, Default::default())
        .unwrap();
    let tint_h = reg
        .register_component::<Tint>(tint_id, Default::default())
        .unwrap();
    world.insert_resource(reg.clone());

    let matching = world.spawn((Pos { x: 1.0, y: 2.0 }, Team { id: 1 })).id();
    let non_matching = world.spawn(Pos { x: 3.0, y: 4.0 }).id();

    #[repr(C)]
    struct Pair {
        matching: FfiEntity,
        non_matching: FfiEntity,
        match_ok: bool,
        non_match_rejected: bool,
    }
    let mut pair = Pair {
        matching: FfiEntity::from_bits(matching.to_bits()),
        non_matching: FfiEntity::from_bits(non_matching.to_bits()),
        match_ok: false,
        non_match_rejected: false,
    };

    let spec = BamQuerySpecOwned::new(vec![
        BamTermDesc::new(pos_h, AccessMode::READ),
        BamTermDesc::new(team_h, AccessMode::FILTER_ONLY).with_or_group(1),
        BamTermDesc::new(tint_h, AccessMode::FILTER_ONLY).with_or_group(1),
    ]);
    let mut sys = GuestSystem::new(
        "random_or_sys",
        vec![spec],
        random_or_cb,
        (&raw mut pair).cast::<c_void>(),
        reg,
    );
    sys.initialize(&mut world);
    assert!(
        sys.init_error().is_none(),
        "init_error: {:?}",
        sys.init_error()
    );
    sys.run((), &mut world).unwrap();
    assert!(sys.is_enabled(), "random_or_sys faulted");

    assert!(pair.match_ok);
    assert!(pair.non_match_rejected);
}

#[test]
fn command_arena_config_limits_retained_slabs() {
    use bamabi_host_sys::CommandArenaConfig;

    let mut world = World::new();
    let mut reg = ModRegistry::new();
    let pos_id = world.register_component::<Pos>();
    let pos_h = reg
        .register_component::<Pos>(pos_id, Default::default())
        .unwrap();
    world.insert_resource(reg.clone());
    let snap = reg.snapshot_arc();
    let target = world.spawn_empty().id();

    let default_cfg = CommandArenaConfig::default();
    assert_eq!(default_cfg.max_retained_slabs, 2);

    let mut storage = bamabi_host_sys::HostCommandsStorage::new();
    let big_payload = Pos { x: 1.0, y: 2.0 };
    {
        let mut cmds =
            bamabi_host_sys::HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        for _ in 0..4000 {
            // SAFETY: Valid `cmds` and `Pos` payload.
            unsafe {
                assert_eq!(
                    (HOST_API.cmd_insert.unwrap())(
                        cmds.as_ffi_ptr(),
                        FfiEntity::from_bits(target.to_bits()),
                        pos_h,
                        (&raw const big_payload).cast::<u8>(),
                        core::mem::size_of::<Pos>(),
                    ),
                    FfiResult::OK
                );
            }
        }
    }
    assert!(storage.arena_allocated_bytes() > 0);
    world.insert_resource(CommandArenaConfig {
        max_retained_slabs: 1,
    });
    storage.apply_to_world(&mut world, &snap);
    let retained_1 = storage.arena_allocated_bytes();
    assert!(retained_1 > 0);

    world.insert_resource(CommandArenaConfig {
        max_retained_slabs: 0,
    });
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(storage.arena_allocated_bytes(), 0);
}

#[test]
fn unaligned_output_pointers_return_invalid_argument_without_aborting() {
    use bamabi_host_sys::{
        ensure_symbol_tables_in_world, host_bind_named_object, host_register_component,
        host_resolve_component,
    };

    let mut world = World::new();
    world.insert_resource(ModRegistry::new());
    ensure_symbol_tables_in_world(&mut world);
    let pos_id = world.register_component::<Pos>();
    {
        let mut reg = world.resource_mut::<ModRegistry>();
        let _ = reg.register_component::<Pos>(pos_id, Default::default());
    }
    let pos_h = world
        .resource::<ModRegistry>()
        .get_by_name("v04_test::Pos")
        .unwrap()
        .handle;

    let mut buf = [0u8; 128];
    let base_addr = buf.as_mut_ptr() as usize;
    let unaligned_addr = |align: usize| -> usize {
        if align <= 1 {
            base_addr
        } else {
            let rem = base_addr % align;
            let aligned = if rem == 0 {
                base_addr
            } else {
                base_addr + (align - rem)
            };
            aligned + 1
        }
    };

    let named_e = world.spawn_empty().id();
    let mut lc = LoadContext::with_world_and_mod_id(
        world.resource::<ModRegistry>().clone(),
        Arc::new(AtomicBool::new(true)),
        &raw mut world,
        Some(ModId(77)),
    );
    // SAFETY: `lc` is active in init.
    let host_ctx = unsafe { host_load_host_ctx(lc.as_ffi_ptr()) };

    // Slot 55: `intern_symbol` (`*mut BamSymbol`, align 4)
    let sym_view = StrView::from_str("unaligned_probe_sym");
    let unaligned_sym = unaligned_addr(core::mem::align_of::<BamSymbol>()) as *mut BamSymbol;
    let rc_intern = {
        // SAFETY: Non-null unaligned pointer must be rejected before dereference.
        unsafe { (HOST_API.intern_symbol.unwrap())(host_ctx, &raw const sym_view, unaligned_sym) }
    };
    assert_eq!(rc_intern, FfiResult::INVALID_ARGUMENT);
    let mut valid_sym = BamSymbol::INVALID;
    let rc_intern_ok = {
        // SAFETY: Valid aligned output.
        unsafe { host_intern_symbol(host_ctx, &raw const sym_view, &raw mut valid_sym) }
    };
    assert_eq!(rc_intern_ok, FfiResult::OK);

    // Slot 76: `resolve_symbol_str` (`*mut StrView`, align 8)
    let unaligned_view = unaligned_addr(core::mem::align_of::<StrView>()) as *mut StrView;
    let rc_sym_str = {
        // SAFETY: Non-null unaligned pointer must be rejected before dereference.
        unsafe { (HOST_API.resolve_symbol_str.unwrap())(host_ctx, valid_sym, unaligned_view) }
    };
    assert_eq!(rc_sym_str, FfiResult::INVALID_ARGUMENT);

    // Slot 75: `resolve_named_object` (`*mut FfiEntity`, align 8) — missing, valid, and error paths
    let obj_name = StrView::from_str("unaligned_probe_obj");
    let rc_bind = {
        // SAFETY: Valid binding.
        unsafe {
            host_bind_named_object(
                host_ctx,
                &raw const obj_name,
                FfiEntity::from_bits(named_e.to_bits()),
            )
        }
    };
    assert_eq!(rc_bind, FfiResult::OK);
    let unaligned_ent = unaligned_addr(core::mem::align_of::<FfiEntity>()) as *mut FfiEntity;
    let rc_resolve_obj = {
        // SAFETY: Non-null unaligned pointer must be rejected before dereference.
        unsafe {
            (HOST_API.resolve_named_object.unwrap())(host_ctx, &raw const obj_name, unaligned_ent)
        }
    };
    assert_eq!(rc_resolve_obj, FfiResult::INVALID_ARGUMENT);
    let missing_name = StrView::from_str("missing_probe_obj");
    let rc_resolve_missing = {
        // SAFETY: Non-null unaligned pointer must be rejected even on NOT_FOUND branch.
        unsafe {
            (HOST_API.resolve_named_object.unwrap())(
                host_ctx,
                &raw const missing_name,
                unaligned_ent,
            )
        }
    };
    assert_eq!(rc_resolve_missing, FfiResult::INVALID_ARGUMENT);

    // Slot 73 & 74: `register_resource` and `register_message` (`*mut BamComponent`, align 4)
    let unaligned_comp = unaligned_addr(core::mem::align_of::<BamComponent>()) as *mut BamComponent;
    let res_desc = ComponentDesc::new(
        c"v04_test::UnalignedRes".as_ptr(),
        ComponentUuid::from_canonical_name("v04_test::UnalignedRes"),
        4,
        4,
    );
    let rc_reg_res = {
        // SAFETY: Non-null unaligned pointer must be rejected before dereference.
        unsafe {
            (HOST_API.register_resource.unwrap())(
                lc.as_ffi_ptr(),
                &raw const res_desc,
                unaligned_comp,
            )
        }
    };
    assert_eq!(rc_reg_res, FfiResult::INVALID_ARGUMENT);
    let msg_desc = ComponentDesc::new(
        c"v04_test::UnalignedMsg".as_ptr(),
        ComponentUuid::from_canonical_name("v04_test::UnalignedMsg"),
        4,
        4,
    );
    let rc_reg_msg = {
        // SAFETY: Non-null unaligned pointer must be rejected before dereference.
        unsafe {
            (HOST_API.register_message.unwrap())(
                lc.as_ffi_ptr(),
                &raw const msg_desc,
                unaligned_comp,
            )
        }
    };
    assert_eq!(rc_reg_msg, FfiResult::INVALID_ARGUMENT);
    // Also verify `register_component` and `resolve_component` reject unaligned output pointers.
    let rc_reg_comp = {
        // SAFETY: Non-null unaligned pointer must be rejected before dereference.
        unsafe { host_register_component(lc.as_ffi_ptr(), &raw const res_desc, unaligned_comp) }
    };
    assert_eq!(rc_reg_comp, FfiResult::INVALID_ARGUMENT);
    let rc_res_comp = {
        // SAFETY: Non-null unaligned pointer must be rejected before dereference.
        unsafe {
            host_resolve_component(host_ctx, c"v04_test::Pos".as_ptr(), null(), unaligned_comp)
        }
    };
    assert_eq!(rc_res_comp, FfiResult::INVALID_ARGUMENT);
    lc.finish_init();

    // Slot 89: `cmd_spawn_batch_ex` (`*mut FfiEntity`, align 8) — both `column_count == 0` and `> 0`.
    let snap = world.resource::<ModRegistry>().snapshot_arc();
    let mut storage = bamabi_host_sys::HostCommandsStorage::new();
    let mut cmds =
        bamabi_host_sys::HostCommands::new(&mut storage, world.entity_allocator(), &snap);
    let rc_spawn_ex_0 = {
        // SAFETY: Non-null unaligned `out_entities` with `column_count == 0`, `entity_count == 2`.
        unsafe {
            (HOST_API.cmd_spawn_batch_ex.unwrap())(cmds.as_ffi_ptr(), null(), 0, 2, unaligned_ent)
        }
    };
    assert_eq!(rc_spawn_ex_0, FfiResult::INVALID_ARGUMENT);
    let pos_vals = [Pos { x: 1.0, y: 2.0 }, Pos { x: 3.0, y: 4.0 }];
    let cols = [BamSpawnColumnDesc::dense(
        pos_h,
        pos_vals.as_ptr().cast::<u8>(),
        core::mem::size_of::<Pos>(),
    )];
    let rc_spawn_ex_1 = {
        // SAFETY: Non-null unaligned `out_entities` with `column_count == 1`, `entity_count == 2`.
        unsafe {
            (HOST_API.cmd_spawn_batch_ex.unwrap())(
                cmds.as_ffi_ptr(),
                cols.as_ptr(),
                cols.len(),
                2,
                unaligned_ent,
            )
        }
    };
    assert_eq!(rc_spawn_ex_1, FfiResult::INVALID_ARGUMENT);
    let rc_spawn_empty = {
        // SAFETY: `cmd_spawn_empty` with unaligned `out_entity`.
        unsafe { (HOST_API.cmd_spawn_empty.unwrap())(cmds.as_ffi_ptr(), unaligned_ent) }
    };
    assert_eq!(rc_spawn_empty, FfiResult::INVALID_ARGUMENT);
    drop(cmds);

    // Slots 7 & 8 (`sys_query` and `sys_commands`): `*mut *mut BamIter` / `*mut *mut BamCommands`
    // must reject misaligned output pointers with `INVALID_ARGUMENT` both before `sys_ctx` lookup
    // and inside a live `GuestSystem` invocation without aborting (`SIGABRT`).
    let unaligned_iter_out =
        unaligned_addr(core::mem::align_of::<*mut BamIter>()) as *mut *mut BamIter;
    let unaligned_cmds_out =
        unaligned_addr(core::mem::align_of::<*mut BamCommands>()) as *mut *mut BamCommands;
    // SAFETY: Non-null misaligned output pointers must be rejected before dereference.
    unsafe {
        assert_eq!(
            (HOST_API.sys_query.unwrap())(null_mut(), 0, unaligned_iter_out),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            (HOST_API.sys_commands.unwrap())(null_mut(), unaligned_cmds_out),
            FfiResult::INVALID_ARGUMENT
        );
    }

    #[repr(C)]
    struct SysOutAlignCheck {
        unaligned_iter_out: *mut *mut BamIter,
        unaligned_cmds_out: *mut *mut BamCommands,
        query_rc: FfiResult,
        cmds_rc: FfiResult,
        valid_query_ok: bool,
        valid_cmds_ok: bool,
    }
    unsafe extern "C" fn sys_out_align_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `SysOutAlignCheck` for the duration of this system run.
        let st = unsafe { &mut *(user_data.cast::<SysOutAlignCheck>()) };
        let mut valid_iter: *mut BamIter = null_mut();
        let mut valid_cmds: *mut BamCommands = null_mut();
        // SAFETY: Active `sys_ctx`; unaligned out-ptrs must return `INVALID_ARGUMENT` cleanly.
        unsafe {
            st.query_rc = (HOST_API.sys_query.unwrap())(sys_ctx, 0, st.unaligned_iter_out);
            st.cmds_rc = (HOST_API.sys_commands.unwrap())(sys_ctx, st.unaligned_cmds_out);
            st.valid_query_ok = (HOST_API.sys_query.unwrap())(sys_ctx, 0, &raw mut valid_iter)
                .is_ok()
                && !valid_iter.is_null();
            st.valid_cmds_ok = (HOST_API.sys_commands.unwrap())(sys_ctx, &raw mut valid_cmds)
                .is_ok()
                && !valid_cmds.is_null();
        }
        FfiResult::OK
    }
    let mut sys_align_check = SysOutAlignCheck {
        unaligned_iter_out,
        unaligned_cmds_out,
        query_rc: FfiResult::OK,
        cmds_rc: FfiResult::OK,
        valid_query_ok: false,
        valid_cmds_ok: false,
    };
    let mut align_sys = GuestSystem::new(
        "sys_ctx_out_align_probe",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            pos_h,
            AccessMode::READ,
        )])],
        sys_out_align_cb,
        (&raw mut sys_align_check).cast::<c_void>(),
        world.resource::<ModRegistry>().clone(),
    );
    align_sys.initialize(&mut world);
    align_sys.run((), &mut world).unwrap();
    assert_eq!(sys_align_check.query_rc, FfiResult::INVALID_ARGUMENT);
    assert_eq!(sys_align_check.cmds_rc, FfiResult::INVALID_ARGUMENT);
    assert!(sys_align_check.valid_query_ok);
    assert!(sys_align_check.valid_cmds_ok);
}
