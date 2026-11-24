use super::*;
use crate::drop_trampoline::WorldDropGuard;
use crate::ffi::register::{LoadContext, host_register_component};
use crate::loader::symbols::ModNamedObjects;
use crate::system::ModQuarantine;
use bamabi_sys::{
    BAM_COMPONENT_FLAG_SPARSE_SET, BAM_COMPONENT_FLAG_SUMMARY_TICK, BAM_STORAGE_FLAG_RESOURCE,
    BamInstance, ComponentDesc, ComponentUuid, FfiResult, FfiSlice, StrView,
};
use core::ffi::{CStr, c_char};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[test]
fn snapshot_and_restore_cover_dynamic_resources_zsts_errors_and_cleanup() {
    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    static RES_DROPS: AtomicUsize = AtomicUsize::new(0);
    static FAIL_EXPORT: AtomicBool = AtomicBool::new(false);
    static FAIL_INIT: AtomicBool = AtomicBool::new(false);
    static FAIL_IMPORT: AtomicBool = AtomicBool::new(false);

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq)]
    struct DynResVal {
        count: u32,
    }

    unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}
    unsafe extern "C" fn drop_res(_ptr: *mut c_void) {
        RES_DROPS.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "C" fn export_res(
        src: *const c_void,
        sink: Option<crate::ffi::validation::BamExportFieldsSinkFn>,
        ctx: *mut c_void,
    ) -> FfiResult {
        if FAIL_EXPORT.load(Ordering::SeqCst) {
            return FfiResult::INVALID_STATE;
        }
        let Some(sink_fn) = sink else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `src` points to live `DynResVal`.
        let val = unsafe { (*src.cast::<DynResVal>()).count };
        let bytes = val.to_ne_bytes();
        let name_view = StrView::from_str("count");
        let tag_view = StrView::from_str("u32");
        let slice = FfiSlice::from_slice(&bytes);
        // SAFETY: `ctx`, `name_view`, `tag_view`, and `slice` live for this synchronous callback.
        unsafe {
            sink_fn(
                ctx,
                &raw const name_view,
                &raw const tag_view,
                &raw const slice,
            )
        }
    }
    unsafe extern "C" fn export_zst(
        _src: *const c_void,
        sink: Option<crate::ffi::validation::BamExportFieldsSinkFn>,
        ctx: *mut c_void,
    ) -> FfiResult {
        if FAIL_EXPORT.load(Ordering::SeqCst) {
            return FfiResult::CAPACITY_EXCEEDED;
        }
        let Some(sink_fn) = sink else {
            return FfiResult::NULL_POINTER;
        };
        let name_view = StrView::from_str("tag");
        let tag_view = StrView::from_str("()");
        let slice = FfiSlice::empty();
        // SAFETY: `ctx` is valid and `slice` has `len == 0`.
        unsafe {
            sink_fn(
                ctx,
                &raw const name_view,
                &raw const tag_view,
                &raw const slice,
            )
        }
    }
    unsafe extern "C" fn init_res(dst: *mut c_void) -> FfiResult {
        if FAIL_INIT.load(Ordering::SeqCst) {
            return FfiResult::CAPACITY_EXCEEDED;
        }
        // SAFETY: `dst` points to aligned writable `DynResVal`.
        unsafe { core::ptr::write(dst.cast::<DynResVal>(), DynResVal { count: 1 }) };
        FfiResult::OK
    }
    unsafe extern "C" fn import_res(
        dst: *mut c_void,
        name: *const StrView,
        _tag: *const StrView,
        payload: *const FfiSlice<u8>,
    ) -> FfiResult {
        if FAIL_IMPORT.load(Ordering::SeqCst) {
            return FfiResult::INVALID_ARGUMENT;
        }
        if name.is_null() || payload.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `name` and `payload` are readable pointers from caller.
        let (key, payload_ref) = unsafe { ((*name).try_as_str().unwrap_or(""), &*payload) };
        if key == "count" && payload_ref.len == 4 {
            // SAFETY: `payload_ref` holds 4 readable bytes.
            let v = u32::from_ne_bytes(unsafe { payload_ref.as_slice() }.try_into().unwrap());
            // SAFETY: `dst` points to aligned `DynResVal`.
            unsafe { (*dst.cast::<DynResVal>()).count = v };
            return FfiResult::OK;
        }
        FfiResult::NOT_FOUND
    }
    unsafe extern "C" fn resolve_sym(
        _inst: *const BamInstance,
        name: *const c_char,
    ) -> *const c_void {
        // SAFETY: `name` is a valid NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            return core::ptr::null();
        };
        match s {
            // Short-name fallback `"bam.v0.DynRes.export_fields"`
            "bam.v0.DynRes.export_fields" => export_res as *const c_void,
            "bam.v0.mig_cov::DynRes.init_default" => init_res as *const c_void,
            "bam.v0.mig_cov::DynRes.import_field" | "bam.v0.mig_cov::ZstComp.import_field" => {
                import_res as *const c_void
            }
            "bam.v0.mig_cov::ZstComp.export_fields" => export_zst as *const c_void,
            _ => core::ptr::null(),
        }
    }

    RES_DROPS.store(0, Ordering::SeqCst);
    FAIL_EXPORT.store(false, Ordering::SeqCst);
    FAIL_INIT.store(false, Ordering::SeqCst);
    FAIL_IMPORT.store(false, Ordering::SeqCst);

    // 1. Empty world without ModRegistry/ModScoped returns gracefully.
    let mut empty_world = World::new();
    assert!(snapshot_mod_dynamic_columns_for_reload(&mut empty_world, ModId(1)).is_ok());
    cleanup_orphaned_mod_scoped_entities(&mut empty_world, ModId(1));
    assert!(
        restore_mod_dynamic_columns_after_reload(
            &mut empty_world,
            ModId(1),
            ReloadColumnSnapshot::default()
        )
        .is_ok()
    );
    let non_empty_snap = ReloadColumnSnapshot {
        columns: vec![SnapshotComponentColumn {
            canonical_name: Arc::from("x"),
            is_resource: false,
            entities: Vec::new(),
        }],
        retained_zero_copy: Vec::new(),
        bytes_copied: 0,
    };
    assert!(
        restore_mod_dynamic_columns_after_reload(
            &mut empty_world,
            ModId(1),
            non_empty_snap.clone()
        )
        .is_ok()
    );

    // 2. Missing ModHost / ModId returns `ModNotFound`.
    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModHost::new());
    world.insert_resource(ModRegistry::new());
    world.insert_resource(ModQuarantine::new());
    world.insert_resource(ModNamedObjects::default());

    let active1 = Arc::new(AtomicBool::new(true));
    let id1 = LoadedMod::register_mock_for_test(
        &mut world,
        "mig_cov",
        Arc::clone(&active1),
        noop_shutdown,
        Some(resolve_sym),
        core::ptr::null_mut(),
    );

    let mut res_h = BamComponent::INVALID;
    let mut res_nowit_h = BamComponent::INVALID;
    let mut zst_h = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, Arc::clone(&active1), world_ptr, Some(id1));

        let res_desc = ComponentDesc::new(
            c"mig_cov::DynRes".as_ptr(),
            ComponentUuid::from_parts("mig_cov::DynRes", 4, 4, 1),
            4,
            4,
        )
        .with_drop(drop_res);
        let rc_res =
            // SAFETY: Valid `load_ctx`, `res_desc`, and `&mut res_h`.
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &res_desc, &mut res_h) };
        assert_eq!(rc_res, FfiResult::OK);

        let res_nw_desc = ComponentDesc::new(
            c"mig_cov::DynResNoWitness".as_ptr(),
            ComponentUuid::from_parts("mig_cov::DynResNoWitness", 4, 4, 2),
            4,
            4,
        );
        let rc_nw =
            // SAFETY: Valid `load_ctx`, `res_nw_desc`, and `&mut res_nowit_h`.
            unsafe {
                host_register_component(load_ctx.as_ffi_ptr(), &res_nw_desc, &mut res_nowit_h)
            };
        assert_eq!(rc_nw, FfiResult::OK);

        let zst_desc = ComponentDesc::new(
            c"mig_cov::ZstComp".as_ptr(),
            ComponentUuid::from_parts("mig_cov::ZstComp", 0, 1, 3),
            0,
            1,
        );
        let rc_zst =
            // SAFETY: Valid `load_ctx`, `zst_desc`, and `&mut zst_h`.
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &zst_desc, &mut zst_h) };
        assert_eq!(rc_zst, FfiResult::OK);
        load_ctx.finish_init();
    }

    // Mark `res_h` and `res_nowit_h` as `BAM_STORAGE_FLAG_RESOURCE` and spawn `IsResource` entities.
    let (res_cid, res_nw_cid, zst_cid) = {
        let mut reg = world.resource_mut::<ModRegistry>();
        let mut snap = (*reg.snapshot_arc()).clone();
        snap.entries[(res_h.0 - 1) as usize].storage_flags |= BAM_STORAGE_FLAG_RESOURCE;
        snap.entries[(res_nowit_h.0 - 1) as usize].storage_flags |= BAM_STORAGE_FLAG_RESOURCE;
        let c1 = snap.entries[(res_h.0 - 1) as usize].component_id;
        let c2 = snap.entries[(res_nowit_h.0 - 1) as usize].component_id;
        let c3 = snap.entries[(zst_h.0 - 1) as usize].component_id;
        reg.restore_snapshot(Arc::new(snap));
        (c1, c2, c3)
    };

    let res_ent = world.spawn(IsResource::new(res_cid)).id();
    // SAFETY: `res_cid` matches `DynResVal` layout `(4, 4)`.
    unsafe {
        OwningPtr::make(DynResVal { count: 77 }, |p| {
            world.entity_mut(res_ent).insert_by_id(res_cid, p);
        });
    }
    let res_nw_ent = world.spawn(IsResource::new(res_nw_cid)).id();
    // SAFETY: `res_nw_cid` matches `DynResVal` layout `(4, 4)`.
    unsafe {
        OwningPtr::make(DynResVal { count: 12 }, |p| {
            world.entity_mut(res_nw_ent).insert_by_id(res_nw_cid, p);
        });
    }

    let zst_ent = world.spawn(ModScoped(id1)).id();
    // SAFETY: `zst_cid` is zero-sized.
    unsafe {
        OwningPtr::make((), |p| {
            world.entity_mut(zst_ent).insert_by_id(zst_cid, p);
        });
    }

    // Failure during `export_fields` on resource returns `SchemaMigrationFailed`.
    FAIL_EXPORT.store(true, Ordering::SeqCst);
    assert!(matches!(
        snapshot_mod_dynamic_columns_for_reload(&mut world, id1),
        Err(ModLoadError::SchemaMigrationFailed { .. })
    ));
    FAIL_EXPORT.store(false, Ordering::SeqCst);

    // Successful snapshot of dynamic resource (short-name lookup) + no-witness resource + ZST entity.
    let snap = snapshot_mod_dynamic_columns_for_reload(&mut world, id1).unwrap();
    assert_eq!(snap.columns.len(), 2);
    assert!(!world.contains_resource_by_id(res_cid));

    // Also test `export_fields` error on an entity component (`FAIL_EXPORT` on ZST).
    // Re-insert ZST under a new mock mod id to hit entity `export_fields` error path.
    let other_id = ModId(999);
    assert!(matches!(
        snapshot_mod_dynamic_columns_for_reload(&mut world, id1),
        Ok(s) if s.columns.is_empty()
    ));

    // Test `restore_mod_dynamic_columns_after_reload` missing mod / error paths:
    assert!(matches!(
        restore_mod_dynamic_columns_after_reload(&mut world, other_id, non_empty_snap),
        Ok(())
    ));

    // Register v2 under `id2` and restore `DynRes` (both absent and overwriting existing) + ZST!
    let active2 = Arc::new(AtomicBool::new(true));
    let id2 = LoadedMod::register_mock_for_test(
        &mut world,
        "mig_cov",
        Arc::clone(&active2),
        noop_shutdown,
        Some(resolve_sym),
        core::ptr::null_mut(),
    );
    let mut res2_h = BamComponent::INVALID;
    let mut zst2_h = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, Arc::clone(&active2), world_ptr, Some(id2));
        let res_desc = ComponentDesc::new(
            c"mig_cov::DynRes".as_ptr(),
            ComponentUuid::from_parts("mig_cov::DynRes", 4, 4, 1),
            4,
            4,
        )
        .with_drop(drop_res);
        let rc_res2 =
            // SAFETY: Valid test pointers.
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &res_desc, &mut res2_h) };
        assert_eq!(rc_res2, FfiResult::OK);
        let zst_desc = ComponentDesc::new(
            c"mig_cov::ZstComp".as_ptr(),
            ComponentUuid::from_parts("mig_cov::ZstComp", 0, 1, 3),
            0,
            1,
        );
        let rc_zst2 =
            // SAFETY: Valid test pointers.
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &zst_desc, &mut zst2_h) };
        assert_eq!(rc_zst2, FfiResult::OK);
        load_ctx.finish_init();
    }
    let res2_cid = {
        let mut reg = world.resource_mut::<ModRegistry>();
        let mut reg_snap = (*reg.snapshot_arc()).clone();
        reg_snap.entries[(res2_h.0 - 1) as usize].storage_flags |= BAM_STORAGE_FLAG_RESOURCE;
        let cid = reg_snap.entries[(res2_h.0 - 1) as usize].component_id;
        reg.restore_snapshot(Arc::new(reg_snap));
        cid
    };

    // Test `init_default` and `import_field` failure branches during restore:
    FAIL_INIT.store(true, Ordering::SeqCst);
    assert!(matches!(
        restore_mod_dynamic_columns_after_reload(&mut world, id2, snap.clone()),
        Err(ModLoadError::SchemaMigrationFailed { .. })
    ));
    FAIL_INIT.store(false, Ordering::SeqCst);

    FAIL_IMPORT.store(true, Ordering::SeqCst);
    let drops_before = RES_DROPS.load(Ordering::SeqCst);
    assert!(matches!(
        restore_mod_dynamic_columns_after_reload(&mut world, id2, snap.clone()),
        Err(ModLoadError::SchemaMigrationFailed { .. })
    ));
    assert_eq!(RES_DROPS.load(Ordering::SeqCst), drops_before + 1);
    FAIL_IMPORT.store(false, Ordering::SeqCst);

    // Restore `DynRes` cleanly onto fresh `res2_cid` and verify restored value `77`.
    restore_mod_dynamic_columns_after_reload(&mut world, id2, snap.clone()).unwrap();
    let restored_count =
        // SAFETY: `res2_cid` holds initialized `DynResVal`.
        unsafe {
            world
                .get_resource_by_id(res2_cid)
                .unwrap()
                .deref::<DynResVal>()
                .count
        };
    assert_eq!(restored_count, 77);
    // Also exercise the `contains_resource_by_id -> remove_resource_by_id` branch when restoring
    // over an existing resource entry.
    restore_mod_dynamic_columns_after_reload(&mut world, id2, snap.clone()).unwrap();
    assert!(world.contains_resource_by_id(res2_cid));
    let restored_again_count =
        // SAFETY: `res2_cid` holds initialized `DynResVal`.
        unsafe {
            world
                .get_resource_by_id(res2_cid)
                .expect("res2_cid readable immediately after overwrite restore")
                .deref::<DynResVal>()
                .count
        };
    assert_eq!(restored_again_count, 77);
    let is_res_count_before_flush = {
        let mut q = world.query::<&IsResource>();
        q.iter(&world)
            .filter(|r| r.resource_component_id() == res2_cid)
            .count()
    };
    assert_eq!(is_res_count_before_flush, 1);

    world.flush();
    assert!(world.contains_resource_by_id(res2_cid));
    let restored_after_flush_count =
        // SAFETY: `res2_cid` holds initialized `DynResVal`.
        unsafe {
            world
                .get_resource_by_id(res2_cid)
                .expect("res2_cid persists after world.flush()")
                .deref::<DynResVal>()
                .count
        };
    assert_eq!(restored_after_flush_count, 77);
    let is_res_count_after_flush = {
        let mut q = world.query::<&IsResource>();
        q.iter(&world)
            .filter(|r| r.resource_component_id() == res2_cid)
            .count()
    };
    assert_eq!(is_res_count_after_flush, 1);

    // Missing `init_default` on non-POD & despawned entity skip & `insert_migrated_buf_into_entity`
    // despawn fallback (`invoke_drop_if_needed`).
    let entry_snapshot = world
        .resource::<ModRegistry>()
        .get_by_handle(res2_h)
        .unwrap()
        .clone();
    assert!(matches!(
        construct_migrated_buffer(&entry_snapshot, None, None, &[]),
        Err(ModLoadError::SchemaMigrationFailed { .. })
    ));
    let buf = construct_migrated_buffer(&entry_snapshot, Some(init_res), None, &[]).unwrap();
    let d_before = RES_DROPS.load(Ordering::SeqCst);
    insert_migrated_buf_into_entity(&mut world, Entity::PLACEHOLDER, &entry_snapshot, buf);
    assert_eq!(RES_DROPS.load(Ordering::SeqCst), d_before + 1);

    // Despawned entity in snapshot is skipped cleanly, and `cleanup_orphaned_mod_scoped_entities`
    // despawns `ModScoped(id1)` (active + Disabled) while preserving `ModScoped(id2)` and `IsResource`.
    let orphan_disabled = world.spawn((ModScoped(id1), Disabled)).id();
    let kept_other = world.spawn(ModScoped(id2)).id();
    world.despawn(zst_ent);
    restore_mod_dynamic_columns_after_reload(&mut world, id2, snap).unwrap();
    cleanup_orphaned_mod_scoped_entities(&mut world, id1);
    assert!(!world.entities().contains(orphan_disabled));
    assert!(world.entities().contains(kept_other));
}

#[test]
fn migrate_component_column_ffi_covers_validation_errors_and_zst_sparse_dense() {
    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe extern "C" fn dummy_drop(_ptr: *mut c_void) {}
    unsafe extern "C" fn noop_migrate(
        _src: *const c_void,
        dst: *mut c_void,
        _ud: *mut c_void,
    ) -> FfiResult {
        if !dst.is_null() {
            // Write zeros if `dst` is 8 bytes
            let _ = dst;
        }
        FfiResult::OK
    }
    unsafe extern "C" fn fail_sparse_migrate(
        _src: *const c_void,
        _dst: *mut c_void,
        _ud: *mut c_void,
    ) -> FfiResult {
        FfiResult::INVALID_STATE
    }

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModRegistry::new());
    world.insert_resource(ModHost::new());

    let mod_a = ModId(1);
    let mod_b = ModId(2);
    let active_a = Arc::new(AtomicBool::new(true));

    let mut zst_dense_h = BamComponent::INVALID;
    let mut zst_sparse_h = BamComponent::INVALID;
    let mut nonpod_h = BamComponent::INVALID;
    let mut other_h = BamComponent::INVALID;
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg, Arc::clone(&active_a), world_ptr, Some(mod_a));

        let d1 = ComponentDesc::new(
            c"mig_err::ZstDense".as_ptr(),
            ComponentUuid::from_parts("mig_err::ZstDense", 0, 1, 1),
            0,
            1,
        )
        .with_schema_fingerprint(11);
        let rc1 =
            // SAFETY: Valid test pointers.
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &d1, &mut zst_dense_h) };
        assert_eq!(rc1, FfiResult::OK);

        let d2 = ComponentDesc::new(
            c"mig_err::ZstSparse".as_ptr(),
            ComponentUuid::from_parts("mig_err::ZstSparse", 0, 1, 2),
            0,
            1,
        )
        .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET)
        .with_schema_fingerprint(22);
        let rc2 =
            // SAFETY: Valid test pointers.
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &d2, &mut zst_sparse_h) };
        assert_eq!(rc2, FfiResult::OK);

        let d3 = ComponentDesc::new(
            c"mig_err::NonPod".as_ptr(),
            ComponentUuid::from_parts("mig_err::NonPod", 4, 4, 3),
            4,
            4,
        )
        .with_drop(dummy_drop);
        let rc3 =
            // SAFETY: Valid test pointers.
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &d3, &mut nonpod_h) };
        assert_eq!(rc3, FfiResult::OK);
        load_ctx.finish_init();
    }
    {
        let reg = world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut world;
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            world_ptr,
            Some(mod_b),
        );
        let d4 = ComponentDesc::new(
            c"mig_err::OtherOwner".as_ptr(),
            ComponentUuid::from_parts("mig_err::OtherOwner", 4, 4, 4),
            4,
            4,
        );
        let rc4 =
            // SAFETY: Valid test pointers.
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &d4, &mut other_h) };
        assert_eq!(rc4, FfiResult::OK);
        load_ctx.finish_init();
    }

    let (zst_dense_cid, zst_sparse_cid) = {
        let reg = world.resource::<ModRegistry>();
        (
            reg.get_by_handle(zst_dense_h).unwrap().component_id,
            reg.get_by_handle(zst_sparse_h).unwrap().component_id,
        )
    };
    let e_dense = world.spawn_empty().id();
    let e_sparse = world.spawn_empty().id();
    // SAFETY: `zst_dense_cid` and `zst_sparse_cid` are zero-sized components.
    unsafe {
        OwningPtr::make((), |p| {
            world.entity_mut(e_dense).insert_by_id(zst_dense_cid, p);
        });
        OwningPtr::make((), |p| {
            world.entity_mut(e_sparse).insert_by_id(zst_sparse_cid, p);
        });
    }

    let reg = world.resource::<ModRegistry>().clone();
    let world_ptr = &raw mut world;
    let mut ctx = LoadContext::with_world_and_mod_id(reg, active_a, world_ptr, Some(mod_a));

    let valid_new = ComponentDesc::new(
        c"mig_err::ZstDense".as_ptr(),
        ComponentUuid::from_parts("mig_err::ZstDense", 8, 4, 10),
        8,
        4,
    )
    .with_schema_fingerprint(12);

    // Helper closure to keep every `unsafe` FFI call clearly documented right before `unsafe`.
    let call_mig = |ctx: &mut LoadContext,
                    handle: BamComponent,
                    expected_fp: u64,
                    new_desc: &ComponentDesc,
                    mig_fn: bamabi_sys::BamRowMigrateFn| {
        // SAFETY: Test passes valid `ctx`, component descriptor references, and `mig_fn`.
        unsafe {
            migrate_component_column_ffi(
                ctx,
                handle,
                expected_fp,
                new_desc,
                mig_fn,
                core::ptr::null_mut(),
            )
        }
    };

    // 1. Invalid handle -> INVALID_COMPONENT
    assert_eq!(
        call_mig(&mut ctx, BamComponent::INVALID, 0, &valid_new, noop_migrate),
        FfiResult::INVALID_COMPONENT
    );
    // 2. Wrong owner -> PERMISSION_DENIED
    assert_eq!(
        call_mig(&mut ctx, other_h, 0, &valid_new, noop_migrate),
        FfiResult::PERMISSION_DENIED
    );
    // 3. Non-POD old component -> UNSUPPORTED
    assert_eq!(
        call_mig(&mut ctx, nonpod_h, 0, &valid_new, noop_migrate),
        FfiResult::UNSUPPORTED
    );
    // 4. Invalid UTF-8 name -> INVALID_UTF8
    let bad_name: [c_char; 2] = [c_char::from_ne_bytes([0xFF]), 0];
    let bad_name_desc = ComponentDesc::new(
        bad_name.as_ptr(),
        ComponentUuid::from_parts("bad", 8, 4, 0),
        8,
        4,
    );
    assert_eq!(
        call_mig(&mut ctx, zst_dense_h, 11, &bad_name_desc, noop_migrate),
        FfiResult::INVALID_UTF8
    );
    // 5. SUMMARY_TICK | SPARSE_SET -> INVALID_ARGUMENT
    let bad_flags_desc = ComponentDesc::new(
        c"mig_err::ZstDense".as_ptr(),
        ComponentUuid::from_parts("mig_err::ZstDense", 8, 4, 11),
        8,
        4,
    )
    .with_flags(BAM_COMPONENT_FLAG_SUMMARY_TICK | BAM_COMPONENT_FLAG_SPARSE_SET);
    assert_eq!(
        call_mig(&mut ctx, zst_dense_h, 11, &bad_flags_desc, noop_migrate),
        FfiResult::INVALID_ARGUMENT
    );
    // 6. Invalid align=3 -> INVALID_ARGUMENT
    let bad_align_desc = ComponentDesc::new(
        c"mig_err::ZstDense".as_ptr(),
        ComponentUuid::from_parts("mig_err::ZstDense", 8, 3, 12),
        8,
        3,
    );
    assert_eq!(
        call_mig(&mut ctx, zst_dense_h, 11, &bad_align_desc, noop_migrate),
        FfiResult::INVALID_ARGUMENT
    );
    // 7. Invalid fields (`null` with non-zero count) -> NULL_POINTER
    let bad_fields_desc = ComponentDesc::new(
        c"mig_err::ZstDense".as_ptr(),
        ComponentUuid::from_parts("mig_err::ZstDense", 8, 4, 13),
        8,
        4,
    )
    .with_fields(core::ptr::null(), 1);
    assert_eq!(
        call_mig(&mut ctx, zst_dense_h, 11, &bad_fields_desc, noop_migrate),
        FfiResult::NULL_POINTER
    );
    // 8. UUID collision with `other_h` -> TYPE_MISMATCH
    let collide_desc = ComponentDesc::new(
        c"mig_err::ZstDense".as_ptr(),
        ComponentUuid::from_parts("mig_err::OtherOwner", 4, 4, 4),
        8,
        4,
    );
    assert_eq!(
        call_mig(&mut ctx, zst_dense_h, 11, &collide_desc, noop_migrate),
        FfiResult::TYPE_MISMATCH
    );

    // 9. Sparse `migrate_fn` failure -> INVALID_STATE
    assert_eq!(
        call_mig(&mut ctx, zst_sparse_h, 22, &valid_new, fail_sparse_migrate),
        FfiResult::INVALID_STATE
    );

    // 10. ZST 0B -> 8B Table and 8B -> 0B Table, plus ZST 0B -> 8B Sparse and 8B -> 0B Sparse!
    assert_eq!(
        call_mig(&mut ctx, zst_dense_h, 11, &valid_new, noop_migrate),
        FfiResult::OK
    );
    let back_to_zst = ComponentDesc::new(
        c"mig_err::ZstDense".as_ptr(),
        ComponentUuid::from_parts("mig_err::ZstDense", 0, 1, 14),
        0,
        1,
    )
    .with_schema_fingerprint(13);
    assert_eq!(
        call_mig(&mut ctx, zst_dense_h, 12, &back_to_zst, noop_migrate),
        FfiResult::OK
    );

    let sparse_to_8b = ComponentDesc::new(
        c"mig_err::ZstSparse".as_ptr(),
        ComponentUuid::from_parts("mig_err::ZstSparse", 8, 4, 20),
        8,
        4,
    )
    .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET)
    .with_schema_fingerprint(23);
    assert_eq!(
        call_mig(&mut ctx, zst_sparse_h, 22, &sparse_to_8b, noop_migrate),
        FfiResult::OK
    );
    let sparse_back_zst = ComponentDesc::new(
        c"mig_err::ZstSparse".as_ptr(),
        ComponentUuid::from_parts("mig_err::ZstSparse", 0, 1, 21),
        0,
        1,
    )
    .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET)
    .with_schema_fingerprint(24);
    assert_eq!(
        call_mig(&mut ctx, zst_sparse_h, 23, &sparse_back_zst, noop_migrate),
        FfiResult::OK
    );

    ctx.finish_init();
}
