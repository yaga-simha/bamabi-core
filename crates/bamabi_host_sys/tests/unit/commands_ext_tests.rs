use super::*;
use crate::HostCommands;
use crate::ffi::commands::HostCommandsStorage;
use crate::loader::ModId;
use crate::registry::{ComponentOptions, ModRegistry};
use bamabi_macros::ModComponent;
use bamabi_sys::AccessMode;
use bevy_ecs::component::Component;
use bevy_ecs::world::World;
use core::ptr::{null, null_mut};

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, ModComponent)]
#[mod_component(name = "commands_ext_test::Val")]
struct CmdExtVal(i32);

#[test]
fn extended_commands_guards_and_execution() {
    let mut world = World::new();
    let cid = world.register_component::<CmdExtVal>();
    let mut reg = ModRegistry::new();
    let val_h = reg
        .register_component::<CmdExtVal>(cid, ComponentOptions::new().with_raw_insert())
        .unwrap();
    let snap = reg.snapshot_arc();
    let bare = world.spawn_empty().id();
    let bare_ffi = FfiEntity::from_bits(bare.to_bits());
    let mut storage = HostCommandsStorage::new();
    let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
        .with_mod_context(Some(ModId(1)), None);
    let mut out_ent = FfiEntity::NULL;

    // Guard branches: null `out_entity` and null/invalid `commands`.
    let bad_cmds = (&raw const snap).cast_mut().cast::<BamCommands>();
    // SAFETY: Intentional null/invalid handles are validated before dereferencing.
    unsafe {
        assert_eq!(
            host_cmd_spawn_persistent(cmds.as_ffi_ptr(), null(), null(), 0, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_persistent(null_mut(), null(), null(), 0, &raw mut out_ent),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_persistent(bad_cmds, null(), null(), 0, &raw mut out_ent),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_spawn_batch(null_mut(), null(), null(), 0, 0, null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_spawn_batch(bad_cmds, null(), null(), 0, 0, null_mut()),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_despawn_all(null_mut(), null(), 0),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_despawn_all(bad_cmds, null(), 0),
            FfiResult::INVALID_STATE
        );
        assert_eq!(
            host_cmd_insert_if_new(null_mut(), FfiEntity::NULL, val_h, null(), 0),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_cmd_insert_if_new(bad_cmds, FfiEntity::NULL, val_h, null(), 0),
            FfiResult::INVALID_STATE
        );
    }

    // Functional path for all 4 entrypoints:
    let init_val = CmdExtVal(10);
    let comps = [val_h];
    let ptrs = [(&raw const init_val).cast::<u8>()];
    let soa_vals = [CmdExtVal(20), CmdExtVal(30)];
    let soa_ptrs = [soa_vals.as_ptr().cast::<u8>()];
    let mut soa_ents = [FfiEntity::NULL; 2];
    let keep_val = CmdExtVal(99);
    let fresh_val = CmdExtVal(77);

    // SAFETY: All pointers reference valid stack arrays matching `CmdExtVal`'s layout.
    unsafe {
        assert_eq!(
            host_cmd_spawn_persistent(
                cmds.as_ffi_ptr(),
                comps.as_ptr(),
                ptrs.as_ptr(),
                1,
                &raw mut out_ent,
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_spawn_batch(
                cmds.as_ffi_ptr(),
                comps.as_ptr(),
                soa_ptrs.as_ptr(),
                1,
                2,
                soa_ents.as_mut_ptr(),
            ),
            FfiResult::OK
        );
        // Keep existing `10` on `out_ent`, insert `77` on `bare`.
        assert_eq!(
            host_cmd_insert_if_new(
                cmds.as_ffi_ptr(),
                out_ent,
                val_h,
                (&raw const keep_val).cast::<u8>(),
                core::mem::size_of::<CmdExtVal>(),
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_insert_if_new(
                cmds.as_ffi_ptr(),
                bare_ffi,
                val_h,
                (&raw const fresh_val).cast::<u8>(),
                core::mem::size_of::<CmdExtVal>(),
            ),
            FfiResult::OK
        );
    }
    drop(cmds);
    storage.apply_to_world(&mut world, &snap);

    let persist_e = bevy_ecs::entity::Entity::try_from_bits(out_ent.to_bits()).unwrap();
    let batch_e0 = bevy_ecs::entity::Entity::try_from_bits(soa_ents[0].to_bits()).unwrap();
    let batch_e1 = bevy_ecs::entity::Entity::try_from_bits(soa_ents[1].to_bits()).unwrap();
    assert_eq!(
        world.entity(persist_e).get::<CmdExtVal>(),
        Some(&CmdExtVal(10))
    );
    assert_eq!(world.entity(bare).get::<CmdExtVal>(), Some(&CmdExtVal(77)));
    assert_eq!(
        world.entity(batch_e0).get::<CmdExtVal>(),
        Some(&CmdExtVal(20))
    );
    assert_eq!(
        world.entity(batch_e1).get::<CmdExtVal>(),
        Some(&CmdExtVal(30))
    );

    // `host_cmd_despawn_all` removes all entities matching `With<CmdExtVal>`.
    let mut cmds2 = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
    let term = [BamTermDesc::new(val_h, AccessMode::FILTER_ONLY)];
    // SAFETY: `cmds2` and `term` are valid on stack.
    unsafe {
        assert_eq!(
            host_cmd_despawn_all(cmds2.as_ffi_ptr(), term.as_ptr(), 1),
            FfiResult::OK
        );
    }
    drop(cmds2);
    storage.apply_to_world(&mut world, &snap);
    assert!(world.get_entity(persist_e).is_err());
    assert!(world.get_entity(bare).is_err());
    assert!(world.get_entity(batch_e0).is_err());
    assert!(world.get_entity(batch_e1).is_err());
}

#[test]
fn clone_entity_clones_host_pod_and_custom_clone_fn_respecting_mod_scoped_and_rollback() {
    use crate::ModScoped;
    use crate::ffi::register::{LoadContext, host_register_component};
    use crate::ffi::v03_ext::{host_cmd_checkpoint, host_cmd_rollback};
    use bamabi_sys::{ComponentDesc, ComponentUuid};
    use bevy_ecs::ptr::OwningPtr;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    unsafe extern "C" fn custom_clone_doubled(
        src: *const c_void,
        dst: *mut c_void,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `src` and `dst` point to aligned `u32` payloads; `user_data` points to `AtomicUsize`.
        unsafe {
            *dst.cast::<u32>() = (*src.cast::<u32>()).saturating_mul(2);
            (*user_data.cast::<AtomicUsize>()).fetch_add(1, Ordering::Relaxed);
        }
        FfiResult::OK
    }

    unsafe extern "C" fn non_clone_drop(ptr: *mut c_void) {
        // SAFETY: Payload holds `*const AtomicUsize`.
        unsafe {
            let p = core::ptr::read_unaligned(ptr.cast::<*const AtomicUsize>());
            if !p.is_null() {
                (*p).fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let mut world = World::new();
    let host_cid = world.register_component::<CmdExtVal>();
    let mut reg = ModRegistry::new();
    let _host_h = reg
        .register_component::<CmdExtVal>(host_cid, ComponentOptions::new().with_raw_insert())
        .unwrap();

    let clone_calls = Box::new(AtomicUsize::new(0));
    let drop_calls = Box::new(AtomicUsize::new(0));
    let clone_ud = (&*clone_calls as *const AtomicUsize)
        .cast_mut()
        .cast::<c_void>();
    let drop_ptr = &*drop_calls as *const AtomicUsize;

    let active = Arc::new(AtomicBool::new(true));
    let mut dyn_pod_h = BamComponent::INVALID;
    let mut dyn_custom_h = BamComponent::INVALID;
    let mut dyn_nonclone_h = BamComponent::INVALID;

    let reg = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(1)),
        );
        let d_pod = ComponentDesc::new(
            c"clone_ext::DynPod".as_ptr(),
            ComponentUuid::from_parts("clone_ext::DynPod", 4, 4, 0),
            4,
            4,
        );
        let d_custom = ComponentDesc::new(
            c"clone_ext::DynCustom".as_ptr(),
            ComponentUuid::from_parts("clone_ext::DynCustom", 4, 4, 0),
            4,
            4,
        )
        .with_clone_fn(custom_clone_doubled)
        .with_hook_user_data(clone_ud);
        let d_nonclone = ComponentDesc::new(
            c"clone_ext::DynNonClone".as_ptr(),
            ComponentUuid::from_parts(
                "clone_ext::DynNonClone",
                core::mem::size_of::<*const AtomicUsize>(),
                core::mem::align_of::<*const AtomicUsize>(),
                0,
            ),
            core::mem::size_of::<*const AtomicUsize>(),
            core::mem::align_of::<*const AtomicUsize>(),
        )
        .with_drop(non_clone_drop);
        // SAFETY: Valid `LoadContext` and `ComponentDesc` pointers.
        unsafe {
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &d_pod, &mut dyn_pod_h),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &d_custom, &mut dyn_custom_h),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &d_nonclone, &mut dyn_nonclone_h),
                FfiResult::OK
            );
        }
        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    let dyn_pod_cid = reg.get_by_handle(dyn_pod_h).unwrap().component_id;
    let dyn_custom_cid = reg.get_by_handle(dyn_custom_h).unwrap().component_id;
    let dyn_nonclone_cid = reg.get_by_handle(dyn_nonclone_h).unwrap().component_id;

    let src = world.spawn((CmdExtVal(123), ModScoped(ModId(1)))).id();
    let dst = world.spawn(ModScoped(ModId(2))).id();
    let dst_rb = world.spawn(ModScoped(ModId(3))).id();
    let mut pod_val: u32 = 55;
    let mut custom_val: u32 = 21;
    let mut drop_payload = drop_ptr;
    // SAFETY: Payloads match exact registered sizes/alignments.
    unsafe {
        world.entity_mut(src).insert_by_id(
            dyn_pod_cid,
            OwningPtr::new(core::ptr::NonNull::from(&mut pod_val).cast()),
        );
        world.entity_mut(src).insert_by_id(
            dyn_custom_cid,
            OwningPtr::new(core::ptr::NonNull::from(&mut custom_val).cast()),
        );
        world.entity_mut(src).insert_by_id(
            dyn_nonclone_cid,
            OwningPtr::new(core::ptr::NonNull::from(&mut drop_payload).cast()),
        );
    }

    let snap = reg.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    let src_ffi = FfiEntity::from_bits(src.to_bits());
    let dst_ffi = FfiEntity::from_bits(dst.to_bits());
    let dst_rb_ffi = FfiEntity::from_bits(dst_rb.to_bits());

    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let mut cp = 0usize;
        // SAFETY: Valid `HostCommands` and entities; invalid checks verify guards.
        unsafe {
            assert_eq!(
                host_cmd_clone_entity(null_mut(), src_ffi, dst_ffi),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_clone_entity(cmds.as_ffi_ptr(), src_ffi, src_ffi),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_clone_entity(cmds.as_ffi_ptr(), FfiEntity::NULL, dst_ffi),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_cmd_clone_entity(cmds.as_ffi_ptr(), src_ffi, dst_ffi),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_checkpoint(cmds.as_ffi_ptr(), &raw mut cp),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_clone_entity(cmds.as_ffi_ptr(), src_ffi, dst_rb_ffi),
                FfiResult::OK
            );
            assert_eq!(host_cmd_rollback(cmds.as_ffi_ptr(), cp), FfiResult::OK);
        }
    }

    storage.apply_to_world(&mut world, &snap);

    assert_eq!(world.entity(dst).get::<CmdExtVal>(), Some(&CmdExtVal(123)));
    assert_eq!(
        world.entity(dst).get::<ModScoped>(),
        Some(&ModScoped(ModId(2)))
    );
    // SAFETY: `dyn_pod_cid` stores a `u32` and `dyn_custom_cid` stores a `u32`.
    unsafe {
        let p = world.entity(dst).get_by_id(dyn_pod_cid).unwrap().as_ptr();
        assert_eq!(core::ptr::read_unaligned(p.cast::<u32>()), 55);
        let c = world
            .entity(dst)
            .get_by_id(dyn_custom_cid)
            .unwrap()
            .as_ptr();
        assert_eq!(core::ptr::read_unaligned(c.cast::<u32>()), 42);
    }
    assert!(
        world.entity(dst).get_by_id(dyn_nonclone_cid).is_err(),
        "non-cloneable drop_fn-only dynamic component must NOT be bitwise copied"
    );
    assert!(
        world.entity(dst_rb).get::<CmdExtVal>().is_none(),
        "rolled-back clone_entity must not execute"
    );
    assert_eq!(clone_calls.load(Ordering::Relaxed), 1);

    world.despawn(src);
    world.despawn(dst);
    assert_eq!(drop_calls.load(Ordering::Relaxed), 1);

    crate::drop_trampoline::prune_drop_trampolines_for_guard(&active);
    let _ = world
        .resource_mut::<crate::WorldDropGuard>()
        .recycle_slots_for_guard(&active);
}

#[test]
fn insert_and_remove_resource_supports_pod_host_and_enforces_safety() {
    use crate::ffi::v03_ext::{host_cmd_checkpoint, host_cmd_rollback};
    use bamabi_sys::{BAM_COMPONENT_FLAG_IMMUTABLE, ComponentUuid, SymbolEntry, VTableHeader};
    use bevy_ecs::resource::Resource;
    use core::ffi::CStr;

    #[repr(C)]
    #[derive(Resource, Clone, Copy, Debug, PartialEq, Eq, ModComponent)]
    #[mod_component(name = "cmd_res_test::PodRes")]
    struct PodRes(u64);

    #[repr(C)]
    #[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
    struct ImmPodRes(u64);

    impl bamabi_sys::ModComponent for ImmPodRes {
        const CANONICAL_NAME: &'static str = "cmd_res_test::ImmPodRes";
        const COMPONENT_CSTR: &'static CStr = c"cmd_res_test::ImmPodRes";
        const UUID: ComponentUuid = ComponentUuid::from_canonical_name("cmd_res_test::ImmPodRes");
        const IS_ABI_POD: bool = true;
        const IS_RAW_INSERTABLE: bool = true;
        const COMPONENT_FLAGS: u32 = BAM_COMPONENT_FLAG_IMMUTABLE;
        type VTable = VTableHeader;
        fn vtable() -> &'static Self::VTable {
            static VT: VTableHeader = VTableHeader::new::<VTableHeader, ImmPodRes>(ImmPodRes::UUID);
            &VT
        }
        fn symbols() -> &'static [SymbolEntry] {
            &[]
        }
    }

    #[derive(Resource, Debug)]
    struct OpaqueRes(#[allow(dead_code)] String);

    impl bamabi_sys::ModComponent for OpaqueRes {
        const CANONICAL_NAME: &'static str = "cmd_res_test::OpaqueRes";
        const COMPONENT_CSTR: &'static CStr = c"cmd_res_test::OpaqueRes";
        const UUID: ComponentUuid = ComponentUuid::from_canonical_name("cmd_res_test::OpaqueRes");
        const IS_ABI_POD: bool = false;
        const IS_RAW_INSERTABLE: bool = false;
        type VTable = VTableHeader;
        fn vtable() -> &'static Self::VTable {
            static VT: VTableHeader = VTableHeader::new::<VTableHeader, OpaqueRes>(OpaqueRes::UUID);
            &VT
        }
        fn symbols() -> &'static [SymbolEntry] {
            &[]
        }
    }

    let mut world = World::new();
    world.insert_resource(PodRes(0));
    let pod_cid = world.components().component_id::<PodRes>().unwrap();
    world.remove_resource::<PodRes>();

    world.insert_resource(ImmPodRes(1));
    let imm_cid = world.components().component_id::<ImmPodRes>().unwrap();

    world.insert_resource(OpaqueRes(String::from("init")));
    let opaque_cid = world.components().component_id::<OpaqueRes>().unwrap();
    world.remove_resource::<OpaqueRes>();

    let comp_cid = world.register_component::<CmdExtVal>();

    let mut reg = ModRegistry::new();
    let pod_h = reg.register_resource::<PodRes>(pod_cid).unwrap();
    let imm_h = reg.register_resource::<ImmPodRes>(imm_cid).unwrap();
    let opaque_h = reg.register_resource::<OpaqueRes>(opaque_cid).unwrap();
    let comp_h = reg
        .register_component::<CmdExtVal>(comp_cid, ComponentOptions::new())
        .unwrap();

    let snap = reg.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    let v1 = PodRes(777);
    let v_rb = PodRes(999);
    let v_imm = ImmPodRes(2);
    let dummy_opaque = [0u8; core::mem::size_of::<OpaqueRes>()];

    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let mut cp = 0usize;
        // SAFETY: Valid pointers and handles for guard + execution tests.
        unsafe {
            assert_eq!(
                host_cmd_insert_resource(null_mut(), pod_h, (&raw const v1).cast(), 8),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_insert_resource(cmds.as_ffi_ptr(), pod_h, null(), 8),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_insert_resource(cmds.as_ffi_ptr(), comp_h, (&raw const v1).cast(), 4),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_insert_resource(cmds.as_ffi_ptr(), imm_h, (&raw const v_imm).cast(), 8),
                FfiResult::PERMISSION_DENIED
            );
            assert_eq!(
                host_cmd_insert_resource(
                    cmds.as_ffi_ptr(),
                    opaque_h,
                    dummy_opaque.as_ptr(),
                    dummy_opaque.len(),
                ),
                FfiResult::PERMISSION_DENIED
            );
            assert_eq!(
                host_cmd_insert_resource(cmds.as_ffi_ptr(), pod_h, (&raw const v1).cast(), 4),
                FfiResult::INVALID_ARGUMENT
            );

            assert_eq!(
                host_cmd_insert_resource(cmds.as_ffi_ptr(), pod_h, (&raw const v1).cast(), 8),
                FfiResult::OK
            );
            // Queuing + rolling back `InsertResource` must leave `PodRes(777)` intact.
            assert_eq!(
                host_cmd_checkpoint(cmds.as_ffi_ptr(), &raw mut cp),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_insert_resource(cmds.as_ffi_ptr(), pod_h, (&raw const v_rb).cast(), 8),
                FfiResult::OK
            );
            assert_eq!(host_cmd_rollback(cmds.as_ffi_ptr(), cp), FfiResult::OK);
        }
    }

    storage.apply_to_world(&mut world, &snap);
    assert_eq!(world.get_resource::<PodRes>(), Some(&PodRes(777)));

    // Replace existing POD resource and then remove it via `host_cmd_remove_resource`.
    let v2 = PodRes(888);
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        // SAFETY: Valid `HostCommands` and handles.
        unsafe {
            assert_eq!(
                host_cmd_insert_resource(cmds.as_ffi_ptr(), pod_h, (&raw const v2).cast(), 8),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(world.get_resource::<PodRes>(), Some(&PodRes(888)));

    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        // SAFETY: Valid `HostCommands` and handles.
        unsafe {
            assert_eq!(
                host_cmd_remove_resource(null_mut(), pod_h),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_remove_resource(cmds.as_ffi_ptr(), comp_h),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_remove_resource(cmds.as_ffi_ptr(), imm_h),
                FfiResult::PERMISSION_DENIED
            );
            assert_eq!(
                host_cmd_remove_resource(cmds.as_ffi_ptr(), pod_h),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert!(world.get_resource::<PodRes>().is_none());
}
