use super::par_for_each_chunk_readonly_impl;
use crate::ffi::iter::{
    HostIter, QueryPlan, QueryTermMeta, SparseRowScratch, host_iter_column_info,
    host_iter_column_readonly, host_iter_column_rows_readonly, host_iter_entities,
    host_iter_next_chunk,
};
use bamabi_sys::{
    AccessMode, BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE, BAM_STORAGE_FLAG_SPARSE_SET,
    BamColumnInfo, BamComponent, BamIter, ComponentUuid, FfiResult,
};
use bevy_ecs::{change_detection::Tick, component::Component, world::World};
use core::ffi::c_void;
use core::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

#[repr(C)]
#[derive(Component, Clone, Copy, Debug)]
struct ParRoVal(i64);

#[repr(C)]
#[derive(Component, Clone, Copy, Debug)]
#[component(storage = "SparseSet")]
struct ParRoSparse(i64);

struct SumCtx {
    sum: AtomicI64,
    count: AtomicUsize,
    nested_rc: AtomicUsize,
    fail_after: usize,
}

unsafe extern "C" fn dense_sum_cb(sub_iter: *mut BamIter, ud: *mut c_void) -> FfiResult {
    // SAFETY: `ud` points to a live `SumCtx` across the synchronous `par_for_each_chunk_readonly` scope.
    let ctx = unsafe { &*ud.cast::<SumCtx>() };
    // Nested call must be rejected with `ACCESS_CONFLICT`.
    // SAFETY: `sub_iter` is an active worker `BamIter`.
    let nested = unsafe { par_for_each_chunk_readonly_impl(sub_iter, Some(noop_cb), ud) };
    ctx.nested_rc.store(nested.0 as usize, Ordering::Relaxed);

    let mut info = BamColumnInfo {
        component: BamComponent::INVALID,
        mode: AccessMode::READ,
        uuid: ComponentUuid::from_bytes([0; 16]),
        size: 0,
        align: 0,
        storage_flags: 0,
        reserved0: 0,
    };
    // SAFETY: `sub_iter` and `info` are valid.
    let info_rc = unsafe { host_iter_column_info(sub_iter.cast_const(), 0, &raw mut info) };
    assert!(info_rc.is_ok(), "host_iter_column_info failed: {info_rc}");
    assert_eq!(info.size, core::mem::size_of::<ParRoVal>());

    let mut len = 0usize;
    // SAFETY: `sub_iter` is an active worker `BamIter`.
    while unsafe { host_iter_next_chunk(sub_iter, &raw mut len) }.get() {
        // SAFETY: `sub_iter` is positioned on a valid chunk of length `len`.
        let col = unsafe { host_iter_column_readonly(sub_iter.cast_const(), 0) };
        // SAFETY: `sub_iter` is positioned on a valid chunk of length `len`.
        let ents = unsafe { host_iter_entities(sub_iter.cast_const()) };
        assert!(!col.is_null() && !ents.is_null());
        // SAFETY: `col` points to `len` contiguous `ParRoVal` elements aligned to `align_of::<ParRoVal>()`.
        #[allow(clippy::cast_ptr_alignment)]
        let slice = unsafe { core::slice::from_raw_parts(col.cast::<ParRoVal>(), len) };
        let mut local_sum = 0i64;
        for v in slice {
            local_sum += v.0;
        }
        ctx.sum.fetch_add(local_sum, Ordering::Relaxed);
        let prev = ctx.count.fetch_add(len, Ordering::Relaxed);
        if ctx.fail_after > 0 && prev + len >= ctx.fail_after {
            return FfiResult::PANIC_CAUGHT;
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn sparse_sum_cb(sub_iter: *mut BamIter, ud: *mut c_void) -> FfiResult {
    // SAFETY: `ud` points to a live `SumCtx`.
    let ctx = unsafe { &*ud.cast::<SumCtx>() };
    let mut len = 0usize;
    // SAFETY: `sub_iter` is an active worker `BamIter`.
    while unsafe { host_iter_next_chunk(sub_iter, &raw mut len) }.get() {
        // SAFETY: `sub_iter` is positioned on a valid archetypal chunk.
        let rows = unsafe { host_iter_column_rows_readonly(sub_iter, 0) };
        assert!(!rows.is_null());
        // SAFETY: `rows` points to `len` non-null `*const u8` pointers to `ParRoSparse`.
        let row_ptrs = unsafe { core::slice::from_raw_parts(rows, len) };
        let mut local_sum = 0i64;
        for &p in row_ptrs {
            // SAFETY: Each row pointer targets a live, properly aligned `ParRoSparse` component.
            #[allow(clippy::cast_ptr_alignment)]
            let val = unsafe { (*p.cast::<ParRoSparse>()).0 };
            local_sum += val;
        }
        ctx.sum.fetch_add(local_sum, Ordering::Relaxed);
        ctx.count.fetch_add(len, Ordering::Relaxed);
    }
    FfiResult::OK
}

unsafe extern "C" fn noop_cb(_iter: *mut BamIter, _ud: *mut c_void) -> FfiResult {
    FfiResult::OK
}

#[test]
fn par_readonly_dense_sparse_permissions_and_error_propagation() {
    let mut world = World::new();
    let val_cid = world.register_component::<ParRoVal>();
    let sparse_cid = world.register_component::<ParRoSparse>();

    let mut expected_dense = 0i64;
    for i in 1..=600i64 {
        world.spawn(ParRoVal(i));
        expected_dense += i;
    }
    let mut expected_sparse = 0i64;
    for i in 1..=300i64 {
        world.spawn(ParRoSparse(i * 3));
        expected_sparse += i * 3;
    }

    let dense_tid = world
        .archetypes()
        .iter()
        .find(|a| a.contains(val_cid))
        .unwrap()
        .table_id();
    let sparse_aid = world
        .archetypes()
        .iter()
        .find(|a| a.contains(sparse_cid))
        .unwrap()
        .id();

    let mk_term = |cid, mode, flags, size| QueryTermMeta {
        component: BamComponent::from_raw(1),
        component_id: cid,
        mode,
        flags: 0,
        uuid: ComponentUuid::from_canonical_name("iter_ext::test"),
        size,
        align: 8,
        storage_flags: flags,
        msg_read_batch_fn: None,
        msg_send_fn: None,
        msg_write_batch_fn: None,
    };

    // 1. Dense read-only parity + nested `ACCESS_CONFLICT`.
    let ro_plan = QueryPlan::from_terms(vec![mk_term(
        val_cid,
        AccessMode::READ,
        0,
        core::mem::size_of::<ParRoVal>(),
    )]);
    let tids = [dense_tid];
    let mut iter = HostIter::new_dense(
        &world.storages().tables,
        &tids,
        &ro_plan,
        Tick::new(0),
        Tick::new(1),
    );
    let ctx = SumCtx {
        sum: AtomicI64::new(0),
        count: AtomicUsize::new(0),
        nested_rc: AtomicUsize::new(0),
        fail_after: 0,
    };
    // SAFETY: `iter` and `ctx` are live on stack across the synchronous call.
    let rc = unsafe {
        par_for_each_chunk_readonly_impl(
            iter.as_ffi_ptr(),
            Some(dense_sum_cb),
            (&raw const ctx).cast_mut().cast::<c_void>(),
        )
    };
    assert_eq!(rc, FfiResult::OK);
    assert_eq!(ctx.count.load(Ordering::Relaxed), 600);
    assert_eq!(ctx.sum.load(Ordering::Relaxed), expected_dense);
    assert_eq!(
        ctx.nested_rc.load(Ordering::Relaxed),
        FfiResult::ACCESS_CONFLICT.0 as usize
    );
    assert_eq!(iter.stats().1, 600);

    // 2. Error propagation stops dispatch and surfaces first non-OK code.
    iter.reset();
    let err_ctx = SumCtx {
        sum: AtomicI64::new(0),
        count: AtomicUsize::new(0),
        nested_rc: AtomicUsize::new(0),
        fail_after: 100,
    };
    // SAFETY: `iter` and `err_ctx` are live on stack.
    let err_rc = unsafe {
        par_for_each_chunk_readonly_impl(
            iter.as_ffi_ptr(),
            Some(dense_sum_cb),
            (&raw const err_ctx).cast_mut().cast::<c_void>(),
        )
    };
    assert_eq!(err_rc, FfiResult::PANIC_CAUGHT);

    // 3. Sparse-set read-only parallel parity.
    let sparse_plan = QueryPlan::from_terms(vec![mk_term(
        sparse_cid,
        AccessMode::READ,
        BAM_STORAGE_FLAG_SPARSE_SET,
        core::mem::size_of::<ParRoSparse>(),
    )]);
    let aids = [sparse_aid];
    let mut scratch = SparseRowScratch::default();
    let mut sparse_iter = HostIter::new_archetypal(
        &world.storages().tables,
        world.archetypes(),
        &world.storages().sparse_sets,
        &aids,
        &sparse_plan,
        &mut scratch,
        Tick::new(0),
        Tick::new(1),
    );
    let sparse_ctx = SumCtx {
        sum: AtomicI64::new(0),
        count: AtomicUsize::new(0),
        nested_rc: AtomicUsize::new(0),
        fail_after: 0,
    };
    // SAFETY: `sparse_iter` and `sparse_ctx` are live on stack.
    let sparse_rc = unsafe {
        par_for_each_chunk_readonly_impl(
            sparse_iter.as_ffi_ptr(),
            Some(sparse_sum_cb),
            (&raw const sparse_ctx).cast_mut().cast::<c_void>(),
        )
    };
    assert_eq!(sparse_rc, FfiResult::OK);
    assert_eq!(sparse_ctx.count.load(Ordering::Relaxed), 300);
    assert_eq!(sparse_ctx.sum.load(Ordering::Relaxed), expected_sparse);

    // 4. Reject EVERY mutable AccessMode (`WRITE`, `OPTIONAL_WRITE`, `RESOURCE_WRITE`, `MESSAGE_WRITE`).
    for (mode, flags) in [
        (AccessMode::WRITE, 0),
        (AccessMode::OPTIONAL_WRITE, 0),
        (AccessMode::RESOURCE_WRITE, BAM_STORAGE_FLAG_RESOURCE),
        (AccessMode::MESSAGE_WRITE, BAM_STORAGE_FLAG_MESSAGE),
    ] {
        let plan = QueryPlan::from_terms(vec![mk_term(val_cid, mode, flags, 8)]);
        let mut mut_iter = HostIter::new_dense(
            &world.storages().tables,
            &tids,
            &plan,
            Tick::new(0),
            Tick::new(1),
        );
        // SAFETY: Valid stack `HostIter` and callback.
        let deny = unsafe {
            par_for_each_chunk_readonly_impl(
                mut_iter.as_ffi_ptr(),
                Some(noop_cb),
                core::ptr::null_mut(),
            )
        };
        assert_eq!(deny, FfiResult::PERMISSION_DENIED, "mode={mode:?}");
    }

    // 5. Exercise `noop_cb` directly on read-only iter plus null `chunk_fn` / null `iter` guards.
    iter.reset();
    // SAFETY: `iter` is valid and read-only; intentional null guards check early returns.
    unsafe {
        assert_eq!(
            par_for_each_chunk_readonly_impl(
                iter.as_ffi_ptr(),
                Some(noop_cb),
                core::ptr::null_mut(),
            ),
            FfiResult::OK
        );
        assert_eq!(
            par_for_each_chunk_readonly_impl(iter.as_ffi_ptr(), None, core::ptr::null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            par_for_each_chunk_readonly_impl(
                core::ptr::null_mut(),
                Some(noop_cb),
                core::ptr::null_mut(),
            ),
            FfiResult::NULL_POINTER
        );
    }
}

#[test]
#[allow(clippy::cast_ptr_alignment)]
fn iter_read_removed_guards_and_batch_read() {
    use super::host_iter_read_removed;
    use bamabi_sys::FfiEntity;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::lifecycle::RemovedComponentEntity;
    use bevy_ecs::message::MessageCursor;

    let mut world = World::new();
    let val_cid = world.register_component::<ParRoVal>();
    let mk_term = |cid, mode, flags| QueryTermMeta {
        component: BamComponent::from_raw(1),
        component_id: cid,
        mode,
        flags: 0,
        uuid: ComponentUuid::from_canonical_name("iter_ext::removed"),
        size: core::mem::size_of::<ParRoVal>(),
        align: 8,
        storage_flags: flags,
        msg_read_batch_fn: None,
        msg_send_fn: None,
        msg_write_batch_fn: None,
    };
    let plan = QueryPlan::from_terms(vec![
        mk_term(val_cid, AccessMode::REMOVED_READ, 0),
        mk_term(val_cid, AccessMode::READ, 0),
        mk_term(val_cid, AccessMode::REMOVED_READ, BAM_STORAGE_FLAG_RESOURCE),
    ]);

    let mut unaligned_buf = [0u64; 2];
    let unaligned = (&raw mut unaligned_buf).cast::<u8>().wrapping_add(1);
    let mut out_entities = [FfiEntity::NULL; 4];
    let mut out_read = 0usize;

    let mut no_world_iter = HostIter::new_dense(
        &world.storages().tables,
        &[],
        &plan,
        Tick::new(0),
        Tick::new(1),
    );
    // SAFETY: Valid stack buffers and intentionally misaligned pointers (checked via `is_aligned()`).
    unsafe {
        assert_eq!(
            host_iter_read_removed(
                no_world_iter.as_ffi_ptr(),
                0,
                out_entities.as_mut_ptr(),
                4,
                core::ptr::null_mut(),
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_iter_read_removed(
                no_world_iter.as_ffi_ptr(),
                0,
                core::ptr::null_mut(),
                4,
                &raw mut out_read,
            ),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_iter_read_removed(
                no_world_iter.as_ffi_ptr(),
                0,
                out_entities.as_mut_ptr(),
                4,
                unaligned.cast::<usize>(),
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_iter_read_removed(
                no_world_iter.as_ffi_ptr(),
                0,
                unaligned.cast::<FfiEntity>(),
                4,
                &raw mut out_read,
            ),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            host_iter_read_removed(
                core::ptr::null_mut(),
                0,
                out_entities.as_mut_ptr(),
                4,
                &raw mut out_read,
            ),
            FfiResult::NULL_POINTER
        );

        no_world_iter.in_par_iter = true;
        assert_eq!(
            host_iter_read_removed(
                no_world_iter.as_ffi_ptr(),
                0,
                out_entities.as_mut_ptr(),
                4,
                &raw mut out_read,
            ),
            FfiResult::ACCESS_CONFLICT
        );
        no_world_iter.in_par_iter = false;

        assert_eq!(
            host_iter_read_removed(
                no_world_iter.as_ffi_ptr(),
                9,
                out_entities.as_mut_ptr(),
                4,
                &raw mut out_read,
            ),
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            host_iter_read_removed(
                no_world_iter.as_ffi_ptr(),
                1,
                out_entities.as_mut_ptr(),
                4,
                &raw mut out_read,
            ),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            host_iter_read_removed(
                no_world_iter.as_ffi_ptr(),
                2,
                out_entities.as_mut_ptr(),
                4,
                &raw mut out_read,
            ),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            host_iter_read_removed(
                no_world_iter.as_ffi_ptr(),
                0,
                core::ptr::null_mut(),
                0,
                &raw mut out_read,
            ),
            FfiResult::OK
        );
        // Missing `world`
        assert_eq!(
            host_iter_read_removed(
                no_world_iter.as_ffi_ptr(),
                0,
                out_entities.as_mut_ptr(),
                4,
                &raw mut out_read,
            ),
            FfiResult::INVALID_STATE
        );
    }
    drop(no_world_iter);

    // With `world` but missing `msg_cursors`, empty `msg_cursors`, wrong cursor type, then real cursor.
    let mut iter_no_cursors = HostIter::new_dense(
        &world.storages().tables,
        &[],
        &plan,
        Tick::new(0),
        Tick::new(1),
    )
    .with_world(world.as_unsafe_world_cell_readonly());
    // SAFETY: Valid `HostIter` and stack buffers.
    unsafe {
        assert_eq!(
            host_iter_read_removed(
                iter_no_cursors.as_ffi_ptr(),
                0,
                out_entities.as_mut_ptr(),
                4,
                &raw mut out_read,
            ),
            FfiResult::INVALID_STATE
        );
    }
    drop(iter_no_cursors);

    let mut wrong_cursors: Vec<Option<Box<dyn core::any::Any + Send + Sync>>> =
        vec![Some(Box::new(123u32))];
    {
        let mut iter_wrong = HostIter::new_dense(
            &world.storages().tables,
            &[],
            &plan,
            Tick::new(0),
            Tick::new(1),
        )
        .with_world(world.as_unsafe_world_cell_readonly())
        .with_msg_cursors(&mut wrong_cursors);
        // SAFETY: Valid `HostIter` and stack buffers.
        unsafe {
            assert_eq!(
                host_iter_read_removed(
                    iter_wrong.as_ffi_ptr(),
                    0,
                    out_entities.as_mut_ptr(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::INVALID_STATE
            );
        }
    }
    wrong_cursors[0] = None;
    {
        let mut iter_none = HostIter::new_dense(
            &world.storages().tables,
            &[],
            &plan,
            Tick::new(0),
            Tick::new(1),
        )
        .with_world(world.as_unsafe_world_cell_readonly())
        .with_msg_cursors(&mut wrong_cursors);
        // SAFETY: Valid `HostIter` and stack buffers.
        unsafe {
            assert_eq!(
                host_iter_read_removed(
                    iter_none.as_ffi_ptr(),
                    0,
                    out_entities.as_mut_ptr(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::INVALID_STATE
            );
        }
    }

    // Valid cursor before any entity has removed components -> OK with 0 read.
    let mut real_cursors: Vec<Option<Box<dyn core::any::Any + Send + Sync>>> = vec![Some(
        Box::new(MessageCursor::<RemovedComponentEntity>::default()),
    )];
    {
        let mut iter_empty = HostIter::new_dense(
            &world.storages().tables,
            &[],
            &plan,
            Tick::new(0),
            Tick::new(1),
        )
        .with_world(world.as_unsafe_world_cell_readonly())
        .with_msg_cursors(&mut real_cursors);
        // SAFETY: Valid `HostIter` and stack buffers.
        unsafe {
            assert_eq!(
                host_iter_read_removed(
                    iter_empty.as_ffi_ptr(),
                    0,
                    out_entities.as_mut_ptr(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::OK
            );
            assert_eq!(out_read, 0);
        }
    }

    // Despawn two entities with `ParRoVal` and read removed entities cleanly.
    let e1 = world.spawn(ParRoVal(10)).id();
    let e2 = world.spawn(ParRoVal(20)).id();
    world.despawn(e1);
    world.despawn(e2);
    {
        let mut iter_live = HostIter::new_dense(
            &world.storages().tables,
            &[],
            &plan,
            Tick::new(0),
            Tick::new(1),
        )
        .with_world(world.as_unsafe_world_cell_readonly())
        .with_msg_cursors(&mut real_cursors);
        // SAFETY: Valid `HostIter` and stack buffers.
        unsafe {
            assert_eq!(
                host_iter_read_removed(
                    iter_live.as_ffi_ptr(),
                    0,
                    out_entities.as_mut_ptr(),
                    4,
                    &raw mut out_read,
                ),
                FfiResult::OK
            );
            assert_eq!(out_read, 2);
            assert_eq!(
                [
                    Entity::try_from_bits(out_entities[0].to_bits()).unwrap(),
                    Entity::try_from_bits(out_entities[1].to_bits()).unwrap(),
                ],
                [e1, e2]
            );
        }
    }
}

#[test]
fn iter_read_removed_partial_batch_conservation() {
    use super::host_iter_read_removed;
    use bamabi_sys::FfiEntity;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::lifecycle::RemovedComponentEntity;
    use bevy_ecs::message::MessageCursor;

    let mut world = World::new();
    let val_cid = world.register_component::<ParRoVal>();
    let plan = QueryPlan::from_terms(vec![QueryTermMeta {
        component: BamComponent::from_raw(1),
        component_id: val_cid,
        mode: AccessMode::REMOVED_READ,
        flags: 0,
        uuid: ComponentUuid::from_canonical_name("iter_ext::removed_cons"),
        size: core::mem::size_of::<ParRoVal>(),
        align: 8,
        storage_flags: 0,
        msg_read_batch_fn: None,
        msg_send_fn: None,
        msg_write_batch_fn: None,
    }]);

    let total = 15usize;
    let mut spawned = Vec::with_capacity(total);
    for i in 0..total {
        spawned.push(world.spawn(ParRoVal(i as i64)).id());
    }
    for &e in &spawned {
        world.despawn(e);
    }

    for batch_size in [1usize, 2, 7, 15, usize::MAX] {
        let mut cursors: Vec<Option<Box<dyn core::any::Any + Send + Sync>>> = vec![Some(Box::new(
            MessageCursor::<RemovedComponentEntity>::default(),
        ))];
        let buf_len = if batch_size == usize::MAX {
            total
        } else {
            batch_size
        };
        let mut out_entities = vec![FfiEntity::NULL; buf_len];
        let mut collected = Vec::with_capacity(total);

        let mut iter = HostIter::new_dense(
            &world.storages().tables,
            &[],
            &plan,
            Tick::new(0),
            Tick::new(1),
        )
        .with_world(world.as_unsafe_world_cell_readonly())
        .with_msg_cursors(&mut cursors);

        loop {
            let mut out_read = 0usize;
            // SAFETY: Valid `HostIter`, `out_entities` has `buf_len` writable slots (and at least
            // `total` slots when `batch_size == usize::MAX`).
            let rc = unsafe {
                host_iter_read_removed(
                    iter.as_ffi_ptr(),
                    0,
                    out_entities.as_mut_ptr(),
                    batch_size,
                    &raw mut out_read,
                )
            };
            assert_eq!(rc, FfiResult::OK);
            assert!(out_read <= buf_len);
            if out_read == 0 {
                break;
            }
            for &ffi_e in &out_entities[..out_read] {
                collected.push(Entity::try_from_bits(ffi_e.to_bits()).unwrap());
            }
        }
        assert_eq!(collected, spawned);
    }
}

#[test]
#[allow(clippy::cast_ptr_alignment)]
fn host_iter_next_chunk_batch_and_column_ticks_with_miri_interleaving() {
    use crate::ffi::iter::{
        host_iter_column_mutable, host_iter_column_ticks, host_iter_mark_changed_range,
        host_iter_next_chunk_batch,
    };
    use bamabi_sys::{BAM_TERM_FLAG_CHANGED, FfiBool, FfiEntity};

    #[repr(C)]
    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
    struct BatchCompB(i64);

    let mut world = World::new();
    let cid_a = world.register_component::<ParRoVal>();
    let cid_b = world.register_component::<BatchCompB>();
    let cid_sp = world.register_component::<ParRoSparse>();

    let e0 = world
        .spawn((ParRoVal(10), BatchCompB(100), ParRoSparse(1)))
        .id();
    let e1 = world
        .spawn((ParRoVal(20), BatchCompB(200), ParRoSparse(2)))
        .id();
    let _e_no_sp = world.spawn((ParRoVal(30), BatchCompB(300))).id();

    let added_tick = world.change_tick();
    world.increment_change_tick();
    let mutate_tick = world.change_tick();
    world.get_mut::<BatchCompB>(e1).unwrap().0 = 205;
    let last_run = added_tick;
    world.increment_change_tick();
    let this_run = world.change_tick();

    let plan = QueryPlan::from_terms(vec![
        QueryTermMeta {
            component: BamComponent::from_raw(1),
            component_id: cid_a,
            mode: AccessMode::WRITE,
            flags: 0,
            uuid: ComponentUuid::from_canonical_name("batch_ticks::A"),
            size: core::mem::size_of::<ParRoVal>(),
            align: 8,
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
        QueryTermMeta {
            component: BamComponent::from_raw(2),
            component_id: cid_b,
            mode: AccessMode::READ,
            flags: 0,
            uuid: ComponentUuid::from_canonical_name("batch_ticks::B"),
            size: core::mem::size_of::<BatchCompB>(),
            align: 8,
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
        QueryTermMeta {
            component: BamComponent::from_raw(3),
            component_id: cid_sp,
            mode: AccessMode::OPTIONAL_READ,
            flags: 0,
            uuid: ComponentUuid::from_canonical_name("batch_ticks::Sp"),
            size: core::mem::size_of::<ParRoSparse>(),
            align: 8,
            storage_flags: BAM_STORAGE_FLAG_SPARSE_SET,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
        QueryTermMeta {
            component: BamComponent::from_raw(4),
            component_id: cid_b,
            mode: AccessMode::HAS,
            flags: 0,
            uuid: ComponentUuid::from_canonical_name("batch_ticks::HasB"),
            size: 0,
            align: 1,
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
    ]);
    let _ = BAM_TERM_FLAG_CHANGED;

    let tables = &world.storages().tables;
    let archetypes = &world.archetypes();
    let sparse_sets = &world.storages().sparse_sets;
    let matched_archs: Vec<_> = archetypes
        .iter()
        .filter(|a| a.contains(cid_a) && a.contains(cid_b))
        .map(|a| a.id())
        .collect();
    let mut scratch = SparseRowScratch::default();
    scratch.ensure_terms(plan.terms.len());
    let mut iter = HostIter::new_archetypal(
        tables,
        archetypes,
        sparse_sets,
        &matched_archs,
        &plan,
        &mut scratch,
        last_run,
        this_run,
    );

    // Null guards.
    let mut out_count = 0usize;
    let mut out_ents: *const FfiEntity = core::ptr::null();
    let mut out_cols = [core::ptr::dangling_mut::<u8>(); 6];
    // SAFETY: Intentional invalid-pointer guard checks.
    unsafe {
        assert_eq!(
            host_iter_next_chunk_batch(
                core::ptr::null_mut(),
                &raw mut out_count,
                &raw mut out_ents,
                out_cols.as_mut_ptr(),
                6,
                0,
            ),
            FfiBool::FALSE
        );
        assert_eq!(
            host_iter_column_ticks(
                core::ptr::null_mut(),
                0,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            ),
            FfiResult::NULL_POINTER
        );
    }

    let mut total_rows = 0usize;
    let mut saw_e0_changed = false;
    let mut saw_e1_changed = false;
    // Mark column 0 changed selectively via `mark_changed_mask = 0b01`!
    // SAFETY: `iter` and output stack buffers are valid.
    while unsafe {
        host_iter_next_chunk_batch(
            iter.as_ffi_ptr(),
            &raw mut out_count,
            &raw mut out_ents,
            out_cols.as_mut_ptr(),
            6,
            1,
        )
    }
    .get()
    {
        total_rows += out_count;
        assert!(!out_ents.is_null());
        assert!(!out_cols[0].is_null());
        assert!(!out_cols[1].is_null());
        assert!(!out_cols[3].is_null());
        assert!(
            out_cols[4].is_null(),
            "trailing unused column slots must be zeroed"
        );
        assert!(out_cols[5].is_null());

        let mut added_ptr: *const u32 = core::ptr::null();
        let mut changed_ptr: *const u32 = core::ptr::null();
        let mut out_last = 0u32;
        let mut out_this = 0u32;

        // Interleave `host_iter_column_ticks`, `host_iter_column_mutable`, `host_iter_mark_changed_range`,
        // and dereference of `*const u32` tick slices (Tree-Borrows verification!).
        // SAFETY: Valid `HostIter` with active chunk.
        unsafe {
            assert_eq!(
                host_iter_column_ticks(
                    iter.as_ffi_ptr(),
                    0,
                    &raw mut added_ptr,
                    &raw mut changed_ptr,
                    &raw mut out_last,
                    &raw mut out_this,
                ),
                FfiResult::OK
            );
            assert_eq!(out_last, last_run.get());
            assert_eq!(out_this, this_run.get());
            assert!(!added_ptr.is_null());
            assert!(!changed_ptr.is_null());

            let col0_mut = host_iter_column_mutable(iter.as_ffi_ptr(), 0);
            assert_eq!(col0_mut, out_cols[0]);
            (*col0_mut.cast::<ParRoVal>()).0 += 1;
            assert_eq!(
                host_iter_mark_changed_range(iter.as_ffi_ptr(), 0, 0, 1),
                FfiResult::OK
            );
            // Re-read `added_ptr` / `changed_ptr` after mutable column/mark calls.
            assert_eq!(*added_ptr, added_tick.get());
            assert_eq!(*changed_ptr, this_run.get());

            // Column 1 (`BatchCompB`, READ) ticks reflect prior mutation on `e1`.
            assert_eq!(
                host_iter_column_ticks(
                    iter.as_ffi_ptr(),
                    1,
                    core::ptr::null_mut(),
                    &raw mut changed_ptr,
                    core::ptr::null_mut(),
                    core::ptr::null_mut(),
                ),
                FfiResult::OK
            );
            for r in 0..out_count {
                let ent = (*out_ents.add(r)).to_bits();
                let chg = *changed_ptr.add(r);
                if ent == e0.to_bits() && chg == added_tick.get() {
                    saw_e0_changed = true;
                }
                if ent == e1.to_bits() && chg == mutate_tick.get() {
                    saw_e1_changed = true;
                }
            }

            // Column 2 (`ParRoSparse`) is SparseSet -> `UNSUPPORTED`.
            assert_eq!(
                host_iter_column_ticks(
                    iter.as_ffi_ptr(),
                    2,
                    &raw mut added_ptr,
                    &raw mut changed_ptr,
                    &raw mut out_last,
                    &raw mut out_this,
                ),
                FfiResult::UNSUPPORTED
            );
            // Column 3 (`HAS`) has no backing component array -> `INVALID_COMPONENT`.
            assert_eq!(
                host_iter_column_ticks(
                    iter.as_ffi_ptr(),
                    3,
                    &raw mut added_ptr,
                    &raw mut changed_ptr,
                    &raw mut out_last,
                    &raw mut out_this,
                ),
                FfiResult::INVALID_COMPONENT
            );
            // Out of range column index -> `OUT_OF_RANGE`.
            assert_eq!(
                host_iter_column_ticks(
                    iter.as_ffi_ptr(),
                    4,
                    &raw mut added_ptr,
                    &raw mut changed_ptr,
                    &raw mut out_last,
                    &raw mut out_this,
                ),
                FfiResult::OUT_OF_RANGE
            );
        }
    }
    assert_eq!(total_rows, 3);
    assert!(saw_e0_changed);
    assert!(saw_e1_changed);
    assert_eq!(world.get::<ParRoVal>(e0).unwrap().0, 11);
}

#[test]
#[allow(clippy::cast_ptr_alignment)]
fn term_flags_changed_added_write_read_filter_only_optional_and_or_group() {
    use crate::ffi::iter::{
        host_iter_column_mutable, host_iter_column_readonly, host_iter_entities,
    };
    use bamabi_sys::{BAM_TERM_FLAG_ADDED, BAM_TERM_FLAG_CHANGED};

    #[repr(C)]
    #[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
    struct FlagB(i64);

    let mut world = World::new();
    let cid_a = world.register_component::<ParRoVal>();
    let cid_b = world.register_component::<FlagB>();
    let cid_sp = world.register_component::<ParRoSparse>();

    // Spawn `e_old` at frame 1, advance tick, spawn `e_new` & mutate `e_old`'s `FlagB` later.
    let e_old = world.spawn((ParRoVal(1), FlagB(10))).id();
    let e_neither = world.spawn((ParRoVal(2), FlagB(20))).id();
    world.increment_change_tick();
    let last_run = world.change_tick();
    world.increment_change_tick();

    // Mutate `ParRoVal` on `e_old`, keep `e_neither` untouched, spawn `e_new` with `ParRoSparse` added.
    world.get_mut::<ParRoVal>(e_old).unwrap().0 = 100;
    let e_new = world.spawn((ParRoVal(3), FlagB(30), ParRoSparse(9))).id();
    world.increment_change_tick();
    let this_run = world.change_tick();

    // 1. `WRITE | BAM_TERM_FLAG_CHANGED` must keep column ptr (NOT suppressed!) and filter to `e_old` + `e_new`.
    let write_changed_plan = QueryPlan::from_terms(vec![QueryTermMeta {
        component: BamComponent::from_raw(1),
        component_id: cid_a,
        mode: AccessMode::WRITE,
        flags: BAM_TERM_FLAG_CHANGED,
        uuid: ComponentUuid::from_canonical_name("tf::A"),
        size: core::mem::size_of::<ParRoVal>(),
        align: 8,
        storage_flags: 0,
        msg_read_batch_fn: None,
        msg_send_fn: None,
        msg_write_batch_fn: None,
    }]);
    let mut matched = Vec::new();
    let table_ids = [world.entity(e_old).archetype().table_id()];
    let mut iter = HostIter::new_dense(
        &world.storages().tables,
        &table_ids,
        &write_changed_plan,
        last_run,
        this_run,
    );
    let mut count = 0usize;
    // SAFETY: Valid `HostIter`.
    while unsafe { host_iter_next_chunk(iter.as_ffi_ptr(), &mut count) }.get() {
        // SAFETY: Chunk active and column 0 is `WRITE | CHANGED` (must return non-null ptr!).
        unsafe {
            let mut_ptr = host_iter_column_mutable(iter.as_ffi_ptr(), 0).cast::<ParRoVal>();
            assert!(
                !mut_ptr.is_null(),
                "WRITE | CHANGED must NOT suppress column pointer"
            );
            let ents = host_iter_entities(iter.as_ffi_ptr());
            for r in 0..count {
                (*mut_ptr.add(r)).0 += 1000;
                matched.push((*ents.add(r)).to_bits());
            }
        }
    }
    matched.sort();
    let mut expected = vec![e_old.to_bits(), e_new.to_bits()];
    expected.sort();
    assert_eq!(matched, expected);
    assert_eq!(world.get::<ParRoVal>(e_old).unwrap().0, 1100);
    assert_eq!(world.get::<ParRoVal>(e_neither).unwrap().0, 2);

    // 2. `READ | ADDED` (`or_group = 0`) combined with `or_group = 1` `(FILTER_ONLY | CHANGED)` OR `(OPTIONAL_READ | CHANGED)`!
    let or_plan = QueryPlan::from_terms_with_or_groups(
        vec![
            QueryTermMeta {
                component: BamComponent::from_raw(1),
                component_id: cid_b,
                mode: AccessMode::READ,
                flags: BAM_TERM_FLAG_ADDED,
                uuid: ComponentUuid::from_canonical_name("tf::B"),
                size: core::mem::size_of::<FlagB>(),
                align: 8,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: BamComponent::from_raw(2),
                component_id: cid_a,
                mode: AccessMode::FILTER_ONLY,
                flags: BAM_TERM_FLAG_CHANGED,
                uuid: ComponentUuid::from_canonical_name("tf::FilterA"),
                size: core::mem::size_of::<ParRoVal>(),
                align: 8,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: BamComponent::from_raw(3),
                component_id: cid_sp,
                mode: AccessMode::OPTIONAL_READ,
                flags: BAM_TERM_FLAG_CHANGED,
                uuid: ComponentUuid::from_canonical_name("tf::Sp"),
                size: core::mem::size_of::<ParRoSparse>(),
                align: 8,
                storage_flags: BAM_STORAGE_FLAG_SPARSE_SET,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
        ],
        &[0, 1, 1],
    );
    let mut scratch = SparseRowScratch::default();
    scratch.ensure_terms(or_plan.terms.len());
    let archs: Vec<_> = world.archetypes().iter().map(|a| a.id()).collect();
    let mut or_iter = HostIter::new_archetypal(
        &world.storages().tables,
        world.archetypes(),
        &world.storages().sparse_sets,
        &archs,
        &or_plan,
        &mut scratch,
        last_run,
        this_run,
    );
    let mut or_matched = Vec::new();
    // SAFETY: Valid `HostIter`.
    while unsafe { host_iter_next_chunk(or_iter.as_ffi_ptr(), &mut count) }.get() {
        // SAFETY: Valid chunk; col 0 (`READ | ADDED`) must return non-null ptr.
        unsafe {
            assert!(!host_iter_column_readonly(or_iter.as_ffi_ptr(), 0).is_null());
            let ents = host_iter_entities(or_iter.as_ffi_ptr());
            for r in 0..count {
                or_matched.push((*ents.add(r)).to_bits());
            }
        }
    }
    assert_eq!(or_matched, vec![e_new.to_bits()]);
}
