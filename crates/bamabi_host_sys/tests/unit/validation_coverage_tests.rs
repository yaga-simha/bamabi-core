use super::*;
use crate::LoadedMod;
use crate::drop_trampoline::WorldDropGuard;
use crate::ffi::register::{LoadContext, host_register_component};
use crate::loader::ModHost;
use crate::registry::{
    ComponentOptions, DynamicComponentReg, HostWireBridge, extract_owned_dynamic_fields,
};
use bamabi_macros::ModComponent;
use bamabi_sys::{
    BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET, BamFieldDesc, BamFieldKind,
    BamInstance, ComponentDesc, ComponentUuid, StrView,
};
use bevy_ecs::component::Component;
use bevy_ecs::prelude::Resource;
use core::ffi::{CStr, c_char};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Component, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "val_cov::ZeroWitness")]
struct ZeroWitness;

#[derive(Resource, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "val_cov::ZeroResWitness")]
struct ZeroResWitness;

#[derive(Resource, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "val_cov::ClockRes")]
struct ClockRes {
    tick: u32,
    label: String,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "val_cov::RawOnlyPod")]
struct RawOnlyPod {
    a: u32,
    b: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "val_cov::ZeroPod")]
struct ZeroPod;

#[repr(C)]
#[derive(Resource, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "val_cov::PodClockRes")]
struct PodClockRes {
    step: u32,
    dt: f32,
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "val_cov::ZeroPodRes")]
struct ZeroPodRes;

#[derive(Component, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "val_cov::NoInserters")]
struct NoInserters {
    val: u32,
}

#[derive(Resource, Default)]
struct ImmRes(u32);

struct ImmResBridge;
impl HostWireBridge<ImmRes> for ImmResBridge {
    type Wire = u32;
    const CANONICAL_NAME: &'static str = "val_cov::ImmRes";
    const SUPPORTS_SET: bool = false;

    fn to_wire(value: &ImmRes) -> Self::Wire {
        value.0
    }
}

fn reg_dyn(
    reg: &mut ModRegistry,
    comp_id: ComponentId,
    desc: &ComponentDesc,
    name: &str,
    owner: Option<crate::loader::ModId>,
) -> BamComponent {
    // SAFETY: Test descriptor `fields` is valid when `field_count > 0`.
    let (schema_fingerprint, fields, owned_field_strings) =
        unsafe { extract_owned_dynamic_fields(desc) }
            .unwrap_or_else(|_| (0, std::sync::Arc::from([]), None));
    reg.register_dynamic_component(DynamicComponentReg {
        comp_id,
        desc,
        name,
        owner,
        drop_slot: None,
        required_components: Box::new([]),
        schema_fingerprint,
        fields,
        owned_field_strings,
    })
    .unwrap()
}

fn reg_dyn_table_comp(
    world: &mut World,
    reg: &mut ModRegistry,
    name: &'static str,
    size: usize,
    align: usize,
    flags: u32,
    drop_fn: Option<ComponentDropFn>,
    owner: Option<crate::loader::ModId>,
    fields: &[BamFieldDesc],
) -> (ComponentId, BamComponent) {
    let layout = Layout::from_size_align(size, align).unwrap();
    let storage = if (flags & BAM_COMPONENT_FLAG_SPARSE_SET) != 0 {
        StorageType::SparseSet
    } else {
        StorageType::Table
    };
    let mutable = (flags & BAM_COMPONENT_FLAG_IMMUTABLE) == 0;
    // SAFETY: `layout` is valid and `drop` is `None` for test descriptors; manual drops are
    // handled directly in tests or via `drop_fn` pointer stored in `ComponentDesc`.
    let cid = world.register_component_with_descriptor(unsafe {
        ComponentDescriptor::new_with_layout(
            name.to_owned(),
            storage,
            layout,
            None,
            mutable,
            false,
            ComponentCloneBehavior::Default,
            None,
        )
    });
    let mut desc = ComponentDesc::new(
        core::ptr::null(),
        ComponentUuid::from_parts(name, size, align, flags as u64),
        size,
        align,
    )
    .with_flags(flags);
    if let Some(df) = drop_fn {
        desc = desc.with_drop(df);
    }
    if !fields.is_empty() {
        desc = desc.with_fields(fields.as_ptr(), fields.len());
    }
    let handle = reg_dyn(reg, cid, &desc, name, owner);
    (cid, handle)
}

#[test]
fn witness_sink_resolve_field_info_and_resource_paths_cover_errors_and_zsts() {
    // 1. `collect_witness_export_sink` error branches.
    // SAFETY: Null context pointer is explicitly handled by `collect_witness_export_sink`.
    let rc_null_ctx = unsafe {
        collect_witness_export_sink(
            core::ptr::null_mut(),
            StrView::from_str("a"),
            StrView::from_str("u32"),
            FfiSlice::empty(),
        )
    };
    assert_eq!(rc_null_ctx, FfiResult::NULL_POINTER);

    let mut sink_out: Vec<ExportedFieldRecord> = Vec::new();
    let bad_utf8 = [0xFFu8];
    let bad_view = StrView {
        ptr: bad_utf8.as_ptr(),
        len: bad_utf8.len(),
    };
    // SAFETY: `sink_out` and `bad_view` point to valid stack allocations.
    let rc_bad_name = unsafe {
        collect_witness_export_sink(
            (&raw mut sink_out).cast::<c_void>(),
            bad_view,
            StrView::from_str("u32"),
            FfiSlice::empty(),
        )
    };
    assert_eq!(rc_bad_name, FfiResult::INVALID_UTF8);

    // SAFETY: `sink_out` and `bad_view` point to valid stack allocations.
    let rc_bad_tag = unsafe {
        collect_witness_export_sink(
            (&raw mut sink_out).cast::<c_void>(),
            StrView::from_str("x"),
            bad_view,
            FfiSlice::empty(),
        )
    };
    assert_eq!(rc_bad_tag, FfiResult::INVALID_UTF8);

    let null_nonempty = FfiSlice {
        ptr: core::ptr::null(),
        len: 4,
    };
    // SAFETY: `sink_out` is valid; `null_nonempty` exercises null-pointer rejection.
    let rc_null_slice = unsafe {
        collect_witness_export_sink(
            (&raw mut sink_out).cast::<c_void>(),
            StrView::from_str("x"),
            StrView::from_str("u32"),
            null_nonempty,
        )
    };
    assert_eq!(rc_null_slice, FfiResult::NULL_POINTER);

    // Empty slice succeeds.
    // SAFETY: `sink_out` is valid and `FfiSlice::empty()` has `len == 0`.
    let rc_empty = unsafe {
        collect_witness_export_sink(
            (&raw mut sink_out).cast::<c_void>(),
            StrView::from_str("empty_field"),
            StrView::from_str("()"),
            FfiSlice::empty(),
        )
    };
    assert_eq!(rc_empty, FfiResult::OK);
    assert_eq!(sink_out.len(), 1);
    assert!(sink_out[0].bytes.is_empty());

    // 2. `export_fields_from_raw_ptr` & `apply_fields_to_raw_ptr` & `query_field_info_from_fn`.
    unsafe extern "C" fn failing_export(
        _src: *const c_void,
        _sink: Option<BamExportFieldsSinkFn>,
        _ctx: *mut c_void,
    ) -> FfiResult {
        FfiResult::INVALID_STATE
    }
    assert_eq!(
        export_fields_from_raw_ptr(failing_export, core::ptr::null()).unwrap_err(),
        FfiResult::INVALID_STATE
    );

    unsafe extern "C" fn mock_import_not_found(
        _dst: *mut c_void,
        _name: StrView,
        _tag: StrView,
        payload: FfiSlice<u8>,
    ) -> FfiResult {
        if payload.len == 0 {
            FfiResult::NOT_FOUND
        } else {
            FfiResult::TYPE_MISMATCH
        }
    }
    let empty_rec = [ExportedFieldRecord {
        field_name: "a".to_owned(),
        type_tag: "()".to_owned(),
        bytes: Vec::new(),
    }];
    assert_eq!(
        apply_fields_to_raw_ptr(
            mock_import_not_found,
            core::ptr::null_mut(),
            &empty_rec,
            true
        ),
        Ok(())
    );
    assert_eq!(
        apply_fields_to_raw_ptr(
            mock_import_not_found,
            core::ptr::null_mut(),
            &empty_rec,
            false
        ),
        Err(FfiResult::NOT_FOUND)
    );
    let non_empty_rec = [ExportedFieldRecord {
        field_name: "b".to_owned(),
        type_tag: "u32".to_owned(),
        bytes: vec![1, 2, 3, 4],
    }];
    assert_eq!(
        apply_fields_to_raw_ptr(
            mock_import_not_found,
            core::ptr::null_mut(),
            &non_empty_rec,
            true
        ),
        Err(FfiResult::TYPE_MISMATCH)
    );

    unsafe extern "C" fn bad_utf8_field_info(
        index: usize,
        out_name: *mut StrView,
        out_tag: *mut StrView,
        out_offset: *mut usize,
    ) -> FfiBool {
        static BAD: [u8; 1] = [0xFF];
        if index == 0 {
            // SAFETY: Valid non-null aligned out pointers provided by `query_field_info_from_fn`.
            unsafe {
                *out_name = StrView {
                    ptr: BAD.as_ptr(),
                    len: 1,
                };
                *out_tag = StrView::from_str("u32");
                *out_offset = 0;
            }
            return FfiBool::TRUE;
        }
        FfiBool::FALSE
    }
    assert!(query_field_info_from_fn(bad_utf8_field_info).is_empty());

    unsafe extern "C" fn bad_tag_field_info(
        index: usize,
        out_name: *mut StrView,
        out_tag: *mut StrView,
        out_offset: *mut usize,
    ) -> FfiBool {
        static BAD: [u8; 1] = [0xFF];
        if index == 0 {
            // SAFETY: Valid non-null aligned out pointers provided by `query_field_info_from_fn`.
            unsafe {
                *out_name = StrView::from_str("ok");
                *out_tag = StrView {
                    ptr: BAD.as_ptr(),
                    len: 1,
                };
                *out_offset = 0;
            }
            return FfiBool::TRUE;
        }
        FfiBool::FALSE
    }
    assert!(query_field_info_from_fn(bad_tag_field_info).is_empty());

    // 3. Host witness inspection, ZST component/resource export & apply, and ClockRes resource.
    let mut world = World::new();
    let zw_cid = world.register_component::<ZeroWitness>();
    let zrw_cid = world.register_component::<ZeroResWitness>();
    let clock_cid = world.register_component::<ClockRes>();
    let mut reg = ModRegistry::new();
    let zw_h = reg
        .register_component::<ZeroWitness>(
            zw_cid,
            ComponentOptions::new().with_raw_insert().with_default(),
        )
        .unwrap();
    let zrw_h = reg.register_resource::<ZeroResWitness>(zrw_cid).unwrap();
    let clock_h = reg.register_resource::<ClockRes>(clock_cid).unwrap();
    world.insert_resource(reg);

    // `inspect_component_witness_fields_in_registry`
    let clock_fields =
        inspect_component_witness_fields_in_registry(world.resource::<ModRegistry>(), clock_h)
            .unwrap();
    assert_eq!(clock_fields.len(), 2);
    assert!(
        inspect_component_witness_fields_in_registry(
            world.resource::<ModRegistry>(),
            BamComponent::INVALID
        )
        .is_none()
    );

    // `export_entity_component_fields` & `apply_entity_component_fields` on resource -> INVALID_COMPONENT,
    // and on ZST component -> OK.
    let ent = world.spawn(ZeroWitness).id();
    assert_eq!(
        export_entity_component_fields(&world, ent, clock_h).unwrap_err(),
        FfiResult::INVALID_COMPONENT
    );
    assert_eq!(
        apply_entity_component_fields(&mut world, ent, clock_h, &[]),
        Err(FfiResult::INVALID_COMPONENT)
    );
    assert!(
        export_entity_component_fields(&world, ent, zw_h)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        apply_entity_component_fields(&mut world, ent, zw_h, &[]),
        Ok(())
    );

    // `export_resource_fields` & `apply_resource_fields`:
    assert_eq!(
        export_resource_fields(&world, zw_h).unwrap_err(),
        FfiResult::INVALID_COMPONENT
    );
    assert_eq!(
        apply_resource_fields(&mut world, zw_h, &[]),
        Err(FfiResult::INVALID_COMPONENT)
    );
    assert_eq!(
        export_resource_fields(&world, clock_h).unwrap_err(),
        FfiResult::NOT_FOUND
    );
    assert_eq!(
        apply_resource_fields(&mut world, clock_h, &[]),
        Err(FfiResult::NOT_FOUND)
    );

    world.insert_resource(ZeroResWitness);
    world.insert_resource(ClockRes {
        tick: 10,
        label: "v1".to_owned(),
    });
    assert!(export_resource_fields(&world, zrw_h).unwrap().is_empty());
    assert_eq!(apply_resource_fields(&mut world, zrw_h, &[]), Ok(()));
    let exported_clock = export_resource_fields(&world, clock_h).unwrap();
    assert_eq!(exported_clock.len(), 2);
    let patch_clock = [
        ExportedFieldRecord {
            field_name: "tick".to_owned(),
            type_tag: "u32".to_owned(),
            bytes: 77u32.to_ne_bytes().to_vec(),
        },
        ExportedFieldRecord {
            field_name: "label".to_owned(),
            type_tag: "String".to_owned(),
            bytes: b"v2".to_vec(),
        },
    ];
    apply_resource_fields(&mut world, clock_h, &patch_clock).unwrap();
    assert_eq!(
        world.resource::<ClockRes>(),
        &ClockRes {
            tick: 77,
            label: "v2".to_owned()
        }
    );
}

#[test]
fn witness_resolve_from_loaded_mod_and_insert_paths_cover_all_branches() {
    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    static DROPS: AtomicUsize = AtomicUsize::new(0);
    static INIT_FAIL: AtomicBool = AtomicBool::new(false);

    #[repr(C)]
    struct GuestDropBox {
        val: u32,
    }

    unsafe extern "C" fn guest_drop(_ptr: *mut c_void) {
        DROPS.fetch_add(1, Ordering::SeqCst);
    }
    unsafe extern "C" fn guest_init_default(dst: *mut c_void) -> FfiResult {
        if INIT_FAIL.load(Ordering::SeqCst) {
            return FfiResult::CAPACITY_EXCEEDED;
        }
        // SAFETY: `dst` points to aligned writable `GuestDropBox` storage.
        unsafe { core::ptr::write(dst.cast::<GuestDropBox>(), GuestDropBox { val: 5 }) };
        FfiResult::OK
    }
    unsafe extern "C" fn guest_import_field(
        dst: *mut c_void,
        field_name: StrView,
        _tag: StrView,
        payload: FfiSlice<u8>,
    ) -> FfiResult {
        // SAFETY: Test passes valid UTF-8 `field_name` and `payload`.
        let name = unsafe { field_name.try_as_str() }.unwrap_or("");
        if name == "bad" {
            return FfiResult::INVALID_ARGUMENT;
        }
        if name == "val" && payload.len == 4 {
            // SAFETY: `payload` holds 4 bytes and `dst` points to live `GuestDropBox`.
            let v = u32::from_ne_bytes(unsafe { payload.as_slice() }.try_into().unwrap());
            // SAFETY: `dst` is valid aligned `GuestDropBox`.
            unsafe { (*dst.cast::<GuestDropBox>()).val = v };
            return FfiResult::OK;
        }
        FfiResult::NOT_FOUND
    }
    unsafe extern "C" fn guest_export_fields(
        src: *const c_void,
        sink: Option<BamExportFieldsSinkFn>,
        ctx: *mut c_void,
    ) -> FfiResult {
        let Some(sink_fn) = sink else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `src` points to live `GuestDropBox`.
        let v = unsafe { (*src.cast::<GuestDropBox>()).val };
        let bytes = v.to_ne_bytes();
        // SAFETY: `ctx` and `bytes` live for this synchronous callback.
        unsafe {
            sink_fn(
                ctx,
                StrView::from_str("val"),
                StrView::from_str("u32"),
                FfiSlice::from_slice(&bytes),
            )
        }
    }
    unsafe extern "C" fn guest_field_info(
        index: usize,
        out_name: *mut StrView,
        out_tag: *mut StrView,
        out_offset: *mut usize,
    ) -> FfiBool {
        if index == 0 {
            // SAFETY: Valid output pointers provided by caller.
            unsafe {
                *out_name = StrView::from_str("val");
                *out_tag = StrView::from_str("u32");
                *out_offset = 0;
            }
            return FfiBool::TRUE;
        }
        FfiBool::FALSE
    }
    unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}
    unsafe extern "C" fn resolve_guest_sym(
        _inst: *const BamInstance,
        name: *const c_char,
    ) -> *const c_void {
        // SAFETY: `name` is a valid NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            return core::ptr::null();
        };
        match s {
            "bam.v0.guest_ns::DropBox.init_default" => guest_init_default as *const c_void,
            "bam.v0.guest_ns::DropBox.import_field" => guest_import_field as *const c_void,
            "bam.v0.guest_ns::DropBox.export_fields" => guest_export_fields as *const c_void,
            // Exercise short-name fallback (`"bam.v0.DropBox.field_info"`).
            "bam.v0.DropBox.field_info" => guest_field_info as *const c_void,
            _ => core::ptr::null(),
        }
    }

    DROPS.store(0, Ordering::SeqCst);
    INIT_FAIL.store(false, Ordering::SeqCst);

    let mut world = World::new();
    world.insert_resource(WorldDropGuard::new());
    world.insert_resource(ModHost::new());
    let mut reg = ModRegistry::new();

    let mod_active = Arc::new(AtomicBool::new(true));
    let mod_id = LoadedMod::register_mock_for_test(
        &mut world,
        "guest_ns",
        Arc::clone(&mod_active),
        noop_shutdown,
        Some(resolve_guest_sym),
        core::ptr::null_mut(),
    );

    let raw_cid = world.register_component::<RawOnlyPod>();
    let raw_h = reg
        .register_component::<RawOnlyPod>(raw_cid, ComponentOptions::new().with_raw_insert())
        .unwrap();

    let no_ins_cid = world.register_component::<NoInserters>();
    let no_ins_h = reg
        .register_component::<NoInserters>(no_ins_cid, ComponentOptions::new())
        .unwrap();

    let zp_cid = world.register_component::<ZeroPod>();
    let zp_h = reg
        .register_component::<ZeroPod>(zp_cid, ComponentOptions::new().opaque())
        .unwrap();

    let (_dyn_pod_cid, dyn_pod_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "guest_ns::DynPodNoWitness",
        8,
        4,
        0,
        None,
        None,
        &[],
    );

    let imm_res_cid = world.register_component::<ImmRes>();
    let imm_res_h = reg
        .register_bridged_resource::<ImmRes, ImmResBridge>(imm_res_cid)
        .unwrap();

    let (drop_cid, drop_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "guest_ns::DropBox",
        4,
        4,
        0,
        Some(guest_drop),
        Some(mod_id),
        &[],
    );
    let (_nopod_cid, nopod_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "guest_ns::NoInitNonPod",
        4,
        4,
        0,
        Some(guest_drop),
        None,
        &[],
    );
    let (_imm_cid, imm_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "guest_ns::ImmComp",
        4,
        4,
        BAM_COMPONENT_FLAG_IMMUTABLE,
        None,
        None,
        &[],
    );
    world.insert_resource(reg);

    // Resolve `field_info` through `LoadedMod` short-name fallback!
    let fields = inspect_component_witness_fields(&world, drop_h).unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, "val");

    let ent = world.spawn_empty().id();

    // Error paths on `insert_entity_component_from_fields`:
    assert_eq!(
        insert_entity_component_from_fields(&mut world, Entity::PLACEHOLDER, drop_h, &[]),
        Err(FfiResult::INVALID_ENTITY)
    );
    assert_eq!(
        insert_entity_component_from_fields(&mut world, ent, imm_res_h, &[]),
        Err(FfiResult::INVALID_COMPONENT)
    );
    assert_eq!(
        insert_entity_component_from_fields(&mut world, ent, nopod_h, &[]),
        Err(FfiResult::UNSUPPORTED)
    );
    assert_eq!(
        insert_entity_component_from_fields(&mut world, ent, no_ins_h, &[]),
        Err(FfiResult::UNSUPPORTED)
    );

    // `init_default` returning error:
    INIT_FAIL.store(true, Ordering::SeqCst);
    assert_eq!(
        insert_entity_component_from_fields(&mut world, ent, drop_h, &[]),
        Err(FfiResult::CAPACITY_EXCEEDED)
    );
    INIT_FAIL.store(false, Ordering::SeqCst);

    // `import_field` error drops initialized scratch buffer via `drop_raw_if_needed`:
    let bad_patch = [ExportedFieldRecord {
        field_name: "bad".to_owned(),
        type_tag: "u32".to_owned(),
        bytes: vec![1, 2, 3, 4],
    }];
    assert_eq!(
        insert_entity_component_from_fields(&mut world, ent, drop_h, &bad_patch),
        Err(FfiResult::INVALID_ARGUMENT)
    );
    assert_eq!(DROPS.load(Ordering::SeqCst), 1);

    // Dynamic `init_default` + `import_field` succeeds and `insert_or_apply` updates in place:
    let good_patch = [ExportedFieldRecord {
        field_name: "val".to_owned(),
        type_tag: "u32".to_owned(),
        bytes: 42u32.to_ne_bytes().to_vec(),
    }];
    insert_or_apply_entity_component_fields(&mut world, ent, drop_h, &good_patch).unwrap();
    let drop_val =
        // SAFETY: `drop_cid` stores a live `GuestDropBox`.
        unsafe {
            world
                .entity(ent)
                .get_by_id(drop_cid)
                .unwrap()
                .deref::<GuestDropBox>()
                .val
        };
    assert_eq!(drop_val, 42);
    let update_patch = [ExportedFieldRecord {
        field_name: "val".to_owned(),
        type_tag: "u32".to_owned(),
        bytes: 99u32.to_ne_bytes().to_vec(),
    }];
    insert_or_apply_entity_component_fields(&mut world, ent, drop_h, &update_patch).unwrap();
    let exported = export_entity_component_fields(&world, ent, drop_h).unwrap();
    assert_eq!(exported[0].bytes, 99u32.to_ne_bytes());

    // Host `RawOnlyPod` with `insert_raw_fn` and no `insert_default_fn`:
    insert_entity_component_from_fields(&mut world, ent, raw_h, &[]).unwrap();
    assert_eq!(
        world.entity(ent).get::<RawOnlyPod>(),
        Some(&RawOnlyPod { a: 0, b: 0 })
    );

    // Immutable component & resource errors:
    assert_eq!(
        apply_entity_component_fields(&mut world, ent, imm_h, &[]),
        Err(FfiResult::PERMISSION_DENIED)
    );
    assert_eq!(
        apply_resource_fields(&mut world, imm_res_h, &[]),
        Err(FfiResult::PERMISSION_DENIED)
    );

    // `read_entity_pod_bytes` / `write_entity_pod_bytes` errors and ZST/RawInsertable paths:
    assert_eq!(
        read_entity_pod_bytes(&world, ent, drop_h),
        Err(FfiResult::TYPE_MISMATCH)
    );
    assert_eq!(
        write_entity_pod_bytes(&mut world, ent, drop_h, &[0; 4]),
        Err(FfiResult::TYPE_MISMATCH)
    );
    assert_eq!(
        write_entity_pod_bytes(&mut world, ent, raw_h, &[1, 2]),
        Err(FfiResult::INVALID_ARGUMENT)
    );
    assert_eq!(
        write_entity_pod_bytes(&mut world, ent, zp_h, &[]),
        Err(FfiResult::PERMISSION_DENIED)
    );
    world.entity_mut(ent).insert(ZeroPod);
    assert!(read_entity_pod_bytes(&world, ent, zp_h).unwrap().is_empty());

    // Non-empty fields without `import_field` on a POD component returns `UNSUPPORTED`:
    assert_eq!(
        insert_entity_component_from_fields(&mut world, ent, dyn_pod_h, &good_patch),
        Err(FfiResult::UNSUPPORTED)
    );
}

#[test]
fn bam_field_pod_kind_classify_and_bam_field_byte_accessors_cover_all_variants() {
    let cases = [
        (BamFieldKind::BOOL, "bool", 1, BamFieldPodKind::Bool, 1, 1),
        (BamFieldKind::U8, "u8", 1, BamFieldPodKind::U8, 1, 1),
        (BamFieldKind::I8, "i8", 1, BamFieldPodKind::I8, 1, 1),
        (BamFieldKind::U16, "u16", 2, BamFieldPodKind::U16, 2, 2),
        (BamFieldKind::I16, "i16", 2, BamFieldPodKind::I16, 2, 2),
        (BamFieldKind::U32, "u32", 4, BamFieldPodKind::U32, 4, 4),
        (BamFieldKind::I32, "i32", 4, BamFieldPodKind::I32, 4, 4),
        (BamFieldKind::U64, "u64", 8, BamFieldPodKind::U64, 8, 8),
        (BamFieldKind::I64, "i64", 8, BamFieldPodKind::I64, 8, 8),
        (BamFieldKind::F32, "f32", 4, BamFieldPodKind::F32, 4, 4),
        (BamFieldKind::F64, "f64", 8, BamFieldPodKind::F64, 8, 8),
        (
            BamFieldKind::USIZE,
            "usize",
            core::mem::size_of::<usize>(),
            BamFieldPodKind::Usize,
            core::mem::size_of::<usize>(),
            core::mem::align_of::<usize>(),
        ),
        (
            BamFieldKind::ISIZE,
            "isize",
            core::mem::size_of::<isize>(),
            BamFieldPodKind::Isize,
            core::mem::size_of::<isize>(),
            core::mem::align_of::<isize>(),
        ),
        (
            BamFieldKind::U64,
            "Entity",
            8,
            BamFieldPodKind::Entity,
            8,
            8,
        ),
        (
            BamFieldKind::U32,
            "BamComponent",
            4,
            BamFieldPodKind::Component,
            4,
            4,
        ),
        (BamFieldKind::NESTED, "Vec2", 8, BamFieldPodKind::Vec2, 8, 4),
        (
            BamFieldKind::NESTED,
            "Vec3",
            12,
            BamFieldPodKind::Vec3,
            12,
            4,
        ),
        (
            BamFieldKind::NESTED,
            "Quat",
            16,
            BamFieldPodKind::Vec4,
            16,
            4,
        ),
        (
            BamFieldKind::NESTED,
            "DVec2",
            16,
            BamFieldPodKind::DVec2,
            16,
            8,
        ),
        (
            BamFieldKind::NESTED,
            "DVec3",
            24,
            BamFieldPodKind::DVec3,
            24,
            8,
        ),
        (
            BamFieldKind::NESTED,
            "DVec4",
            32,
            BamFieldPodKind::DVec4,
            32,
            8,
        ),
        (
            BamFieldKind::NESTED,
            "UVec2",
            8,
            BamFieldPodKind::UVec2,
            8,
            4,
        ),
        (
            BamFieldKind::NESTED,
            "UVec3",
            12,
            BamFieldPodKind::UVec3,
            12,
            4,
        ),
        (
            BamFieldKind::NESTED,
            "UVec4",
            16,
            BamFieldPodKind::UVec4,
            16,
            4,
        ),
        (
            BamFieldKind::NESTED,
            "IVec2",
            8,
            BamFieldPodKind::IVec2,
            8,
            4,
        ),
        (
            BamFieldKind::NESTED,
            "IVec3",
            12,
            BamFieldPodKind::IVec3,
            12,
            4,
        ),
        (
            BamFieldKind::NESTED,
            "IVec4",
            16,
            BamFieldPodKind::IVec4,
            16,
            4,
        ),
        (
            BamFieldKind::OPAQUE,
            "[u8; 2]",
            2,
            BamFieldPodKind::Bytes2,
            2,
            1,
        ),
        (
            BamFieldKind::OPAQUE,
            "[u8; 3]",
            3,
            BamFieldPodKind::Bytes3,
            3,
            1,
        ),
        (
            BamFieldKind::OPAQUE,
            "[u8; 4]",
            4,
            BamFieldPodKind::Bytes4,
            4,
            1,
        ),
        (
            BamFieldKind::OPAQUE,
            "[u8; 8]",
            8,
            BamFieldPodKind::Bytes8,
            8,
            1,
        ),
        (
            BamFieldKind::OPAQUE,
            "[u8; 16]",
            16,
            BamFieldPodKind::Bytes16,
            16,
            1,
        ),
    ];
    for (kind, tag, sz, expected, exp_sz, exp_align) in cases {
        let got = BamFieldPodKind::classify(kind, tag, sz).expect("expected variant");
        assert_eq!(got, expected);
        assert_eq!(got.byte_size(), exp_sz);
        assert_eq!(got.rust_align(), exp_align);
    }
    let unknown_tags = [
        ("bool", 1, BamFieldPodKind::Bool),
        ("u8", 1, BamFieldPodKind::U8),
        ("i8", 1, BamFieldPodKind::I8),
        ("u16", 2, BamFieldPodKind::U16),
        ("i16", 2, BamFieldPodKind::I16),
        ("u32", 4, BamFieldPodKind::U32),
        ("i32", 4, BamFieldPodKind::I32),
        ("u64", 8, BamFieldPodKind::U64),
        ("i64", 8, BamFieldPodKind::I64),
        ("f32", 4, BamFieldPodKind::F32),
        ("f64", 8, BamFieldPodKind::F64),
        (
            "usize",
            core::mem::size_of::<usize>(),
            BamFieldPodKind::Usize,
        ),
        (
            "isize",
            core::mem::size_of::<isize>(),
            BamFieldPodKind::Isize,
        ),
    ];
    for (tag, sz, expected) in unknown_tags {
        assert_eq!(
            BamFieldPodKind::classify(BamFieldKind::UNKNOWN, tag, sz),
            Some(expected)
        );
    }
    assert!(BamFieldPodKind::classify(BamFieldKind::UNKNOWN, "nope", 4).is_none());

    // World without ModRegistry -> INVALID_STATE
    let mut empty_world = World::new();
    let e0 = empty_world.spawn_empty().id();
    assert_eq!(
        inspect_component_bam_fields(&empty_world, BamComponent(1)),
        Err(FfiResult::INVALID_STATE)
    );
    assert_eq!(
        read_entity_bam_field_bytes(&empty_world, e0, BamComponent(1), 0, 4),
        Err(FfiResult::INVALID_STATE)
    );

    let mut world = World::new();
    let mut reg = ModRegistry::new();
    let raw_cid = world.register_component::<RawOnlyPod>();
    let raw_h = reg
        .register_component::<RawOnlyPod>(raw_cid, ComponentOptions::new().with_raw_insert())
        .unwrap();
    let no_ins_cid = world.register_component::<NoInserters>();
    let no_ins_h = reg
        .register_component::<NoInserters>(no_ins_cid, ComponentOptions::new())
        .unwrap();
    let zp_cid = world.register_component::<ZeroPod>();
    let zp_h = reg
        .register_component::<ZeroPod>(zp_cid, ComponentOptions::new().opaque())
        .unwrap();
    let pod_res_cid = world.register_component::<PodClockRes>();
    let pod_res_h = reg.register_resource::<PodClockRes>(pod_res_cid).unwrap();
    let z_res_cid = world.register_component::<ZeroPodRes>();
    let z_res_h = reg.register_resource::<ZeroPodRes>(z_res_cid).unwrap();
    let imm_res_cid = world.register_component::<ImmRes>();
    let imm_res_h = reg
        .register_bridged_resource::<ImmRes, ImmResBridge>(imm_res_cid)
        .unwrap();

    let (_imm_cid, imm_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "val_cov::Imm",
        8,
        4,
        BAM_COMPONENT_FLAG_IMMUTABLE,
        None,
        None,
        &[],
    );
    let (_dyn_pod_cid, dyn_pod_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "val_cov::DynPod",
        8,
        4,
        0,
        None,
        None,
        &[],
    );
    let overflow_fields = [BamFieldDesc::new(
        BamFieldKind::U32,
        StrView::from_str("ovf"),
        StrView::from_str("u32"),
        usize::MAX - 3,
        8,
        4,
    )];
    let (_ovf_cid, ovf_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "val_cov::Ovf",
        8,
        4,
        0,
        None,
        None,
        &overflow_fields,
    );
    world.insert_resource(reg);

    // `inspect_component_bam_fields` & `inspect_component_bam_fields_in_registry`
    assert_eq!(inspect_component_bam_fields(&world, dyn_pod_h), Ok(None));
    assert_eq!(
        inspect_component_bam_fields(&world, ovf_h),
        Err(FfiResult::OUT_OF_RANGE)
    );
    assert_eq!(
        inspect_component_bam_fields(&world, BamComponent::INVALID),
        Err(FfiResult::INVALID_COMPONENT)
    );

    let ent = world.spawn_empty().id();
    // `insert_entity_pod_with_field_patches` on host RawInsertable & dynamic POD & error paths
    assert_eq!(
        insert_entity_pod_with_field_patches(&mut world, ent, no_ins_h, &[]),
        Err(FfiResult::TYPE_MISMATCH)
    );
    assert_eq!(
        insert_entity_pod_with_field_patches(&mut world, ent, zp_h, &[]),
        Err(FfiResult::PERMISSION_DENIED)
    );
    assert_eq!(
        insert_entity_pod_with_field_patches(
            &mut world,
            ent,
            raw_h,
            &[BamFieldBytePatch {
                offset: usize::MAX,
                bytes: vec![1],
            }],
        ),
        Err(FfiResult::OUT_OF_RANGE)
    );
    assert_eq!(
        insert_entity_pod_with_field_patches(
            &mut world,
            ent,
            raw_h,
            &[BamFieldBytePatch {
                offset: 6,
                bytes: vec![1, 2, 3, 4],
            }],
        ),
        Err(FfiResult::OUT_OF_RANGE)
    );
    insert_entity_pod_with_field_patches(
        &mut world,
        ent,
        raw_h,
        &[BamFieldBytePatch {
            offset: 4,
            bytes: 88u32.to_ne_bytes().to_vec(),
        }],
    )
    .unwrap();
    assert_eq!(
        world.entity(ent).get::<RawOnlyPod>(),
        Some(&RawOnlyPod { a: 0, b: 88 })
    );

    insert_entity_pod_with_field_patches(
        &mut world,
        ent,
        dyn_pod_h,
        &[BamFieldBytePatch {
            offset: 0,
            bytes: 123u32.to_ne_bytes().to_vec(),
        }],
    )
    .unwrap();

    // `read_entity_bam_field_bytes` & `write_entity_bam_field_bytes`
    assert_eq!(
        read_entity_bam_field_bytes(&world, ent, pod_res_h, 0, 4),
        Err(FfiResult::INVALID_COMPONENT)
    );
    assert_eq!(
        read_entity_bam_field_bytes(&world, ent, raw_h, usize::MAX, 4),
        Err(FfiResult::OUT_OF_RANGE)
    );
    assert_eq!(
        read_entity_bam_field_bytes(&world, ent, raw_h, 6, 4),
        Err(FfiResult::OUT_OF_RANGE)
    );
    assert_eq!(
        read_entity_bam_field_bytes(&world, Entity::PLACEHOLDER, raw_h, 0, 4),
        Err(FfiResult::INVALID_ENTITY)
    );
    let empty_ent = world.spawn_empty().id();
    assert_eq!(
        read_entity_bam_field_bytes(&world, empty_ent, raw_h, 0, 4),
        Err(FfiResult::NOT_FOUND)
    );
    assert!(
        read_entity_bam_field_bytes(&world, ent, raw_h, 0, 0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        read_entity_bam_field_bytes(&world, ent, raw_h, 4, 4).unwrap(),
        88u32.to_ne_bytes()
    );

    assert_eq!(
        write_entity_bam_field_bytes(&mut world, ent, pod_res_h, &[]),
        Err(FfiResult::INVALID_COMPONENT)
    );
    assert_eq!(
        write_entity_bam_field_bytes(&mut world, ent, imm_h, &[]),
        Err(FfiResult::PERMISSION_DENIED)
    );
    assert_eq!(
        write_entity_bam_field_bytes(
            &mut world,
            ent,
            raw_h,
            &[BamFieldBytePatch {
                offset: usize::MAX,
                bytes: vec![1],
            }],
        ),
        Err(FfiResult::OUT_OF_RANGE)
    );
    assert_eq!(
        write_entity_bam_field_bytes(
            &mut world,
            ent,
            raw_h,
            &[BamFieldBytePatch {
                offset: 6,
                bytes: vec![1, 2, 3, 4],
            }],
        ),
        Err(FfiResult::OUT_OF_RANGE)
    );
    assert_eq!(
        write_entity_bam_field_bytes(&mut world, Entity::PLACEHOLDER, raw_h, &[]),
        Err(FfiResult::INVALID_ENTITY)
    );
    assert_eq!(
        write_entity_bam_field_bytes(&mut world, empty_ent, raw_h, &[]),
        Err(FfiResult::NOT_FOUND)
    );
    write_entity_bam_field_bytes(
        &mut world,
        ent,
        raw_h,
        &[BamFieldBytePatch {
            offset: 0,
            bytes: 55u32.to_ne_bytes().to_vec(),
        }],
    )
    .unwrap();
    assert_eq!(
        world.entity(ent).get::<RawOnlyPod>(),
        Some(&RawOnlyPod { a: 55, b: 88 })
    );

    // `read_resource_bam_field_bytes` & `write_resource_bam_field_bytes`
    assert_eq!(
        read_resource_bam_field_bytes(&world, raw_h, 0, 4),
        Err(FfiResult::INVALID_COMPONENT)
    );
    assert_eq!(
        read_resource_bam_field_bytes(&world, pod_res_h, usize::MAX, 4),
        Err(FfiResult::OUT_OF_RANGE)
    );
    assert_eq!(
        read_resource_bam_field_bytes(&world, pod_res_h, 6, 4),
        Err(FfiResult::OUT_OF_RANGE)
    );
    assert_eq!(
        read_resource_bam_field_bytes(&world, pod_res_h, 0, 4),
        Err(FfiResult::NOT_FOUND)
    );
    assert_eq!(
        write_resource_bam_field_bytes(&mut world, raw_h, &[]),
        Err(FfiResult::INVALID_COMPONENT)
    );
    assert_eq!(
        write_resource_bam_field_bytes(&mut world, imm_res_h, &[]),
        Err(FfiResult::PERMISSION_DENIED)
    );
    assert_eq!(
        write_resource_bam_field_bytes(
            &mut world,
            pod_res_h,
            &[BamFieldBytePatch {
                offset: usize::MAX,
                bytes: vec![1],
            }],
        ),
        Err(FfiResult::OUT_OF_RANGE)
    );
    assert_eq!(
        write_resource_bam_field_bytes(
            &mut world,
            pod_res_h,
            &[BamFieldBytePatch {
                offset: 6,
                bytes: vec![1, 2, 3, 4],
            }],
        ),
        Err(FfiResult::OUT_OF_RANGE)
    );
    assert_eq!(
        write_resource_bam_field_bytes(&mut world, pod_res_h, &[]),
        Err(FfiResult::NOT_FOUND)
    );

    world.insert_resource(PodClockRes { step: 3, dt: 0.5 });
    world.insert_resource(ZeroPodRes);
    assert!(
        read_resource_bam_field_bytes(&world, z_res_h, 0, 0)
            .unwrap()
            .is_empty()
    );
    write_resource_bam_field_bytes(
        &mut world,
        pod_res_h,
        &[BamFieldBytePatch {
            offset: 0,
            bytes: 99u32.to_ne_bytes().to_vec(),
        }],
    )
    .unwrap();
    assert_eq!(world.resource::<PodClockRes>().step, 99);
}

#[test]
fn cross_world_extract_component_and_resource_cover_all_paths() {
    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    unsafe extern "C" fn clone_ok(
        src: *const c_void,
        dst: *mut c_void,
        _user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: Both pointers point to valid 4-byte aligned `u32` values.
        unsafe { core::ptr::write(dst.cast::<u32>(), *src.cast::<u32>() + 1) };
        FfiResult::OK
    }
    unsafe extern "C" fn clone_err(
        _src: *const c_void,
        _dst: *mut c_void,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::INVALID_STATE
    }
    unsafe extern "C" fn dummy_drop(_ptr: *mut c_void) {}

    let mut src_world = World::new();
    src_world.insert_resource(WorldDropGuard::new());
    let mut dst_world = World::new();

    // Missing ModRegistry -> INVALID_STATE
    assert_eq!(
        extract_resource_between_worlds(&src_world, &mut dst_world, BamComponent(1)),
        Err(FfiResult::INVALID_STATE)
    );

    let mut reg = ModRegistry::new();
    let zp_cid = src_world.register_component::<ZeroPod>();
    let zp_h = reg
        .register_component::<ZeroPod>(zp_cid, ComponentOptions::new())
        .unwrap();
    let raw_cid = src_world.register_component::<RawOnlyPod>();
    let raw_h = reg
        .register_component::<RawOnlyPod>(raw_cid, ComponentOptions::new().with_raw_insert())
        .unwrap();
    let no_ins_cid = src_world.register_component::<NoInserters>();
    let no_ins_h = reg
        .register_component::<NoInserters>(no_ins_cid, ComponentOptions::new().opaque())
        .unwrap();
    let pod_res_cid = src_world.register_component::<PodClockRes>();
    let pod_res_h = reg.register_resource::<PodClockRes>(pod_res_cid).unwrap();
    let clock_cid = src_world.register_component::<ClockRes>();
    let clock_h = reg.register_resource::<ClockRes>(clock_cid).unwrap();
    let zres_cid = src_world.register_component::<ZeroPodRes>();
    let zres_h = reg.register_resource::<ZeroPodRes>(zres_cid).unwrap();

    let (sparse_cid, sparse_h) = reg_dyn_table_comp(
        &mut src_world,
        &mut reg,
        "extract_cov::SparsePod",
        4,
        4,
        BAM_COMPONENT_FLAG_SPARSE_SET,
        None,
        None,
        &[],
    );
    let (_drop_cid, drop_h) = reg_dyn_table_comp(
        &mut src_world,
        &mut reg,
        "extract_cov::DropComp",
        4,
        4,
        0,
        Some(dummy_drop),
        None,
        &[],
    );
    let (_drop2_cid, drop2_h) = reg_dyn_table_comp(
        &mut src_world,
        &mut reg,
        "extract_cov::DropComp2",
        4,
        4,
        0,
        Some(dummy_drop),
        None,
        &[],
    );
    src_world.insert_resource(reg);

    // Register dynamic components with `clone_fn` via `host_register_component` so
    // `DynamicComponentHookMap` binds their active clone functions.
    let active = Arc::new(AtomicBool::new(true));
    let mut clone_ok_h = BamComponent::INVALID;
    let mut clone_err_h = BamComponent::INVALID;
    {
        let reg_clone = src_world.resource::<ModRegistry>().clone();
        let world_ptr = &raw mut src_world;
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(reg_clone, Arc::clone(&active), world_ptr, None);

        let desc_ok = ComponentDesc::new(
            c"extract_cov::CloneOk".as_ptr(),
            ComponentUuid::from_parts("extract_cov::CloneOk", 4, 4, 0),
            4,
            4,
        )
        .with_clone_fn(clone_ok);
        let reg_ok_res =
            // SAFETY: Valid `load_ctx`, `desc_ok`, and `&mut clone_ok_h`.
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc_ok, &mut clone_ok_h) };
        assert_eq!(reg_ok_res, FfiResult::OK);

        let desc_err = ComponentDesc::new(
            c"extract_cov::CloneErr".as_ptr(),
            ComponentUuid::from_parts("extract_cov::CloneErr", 4, 4, 0),
            4,
            4,
        )
        .with_clone_fn(clone_err);
        let reg_err_res =
            // SAFETY: Valid `load_ctx`, `desc_err`, and `&mut clone_err_h`.
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc_err, &mut clone_err_h) };
        assert_eq!(reg_err_res, FfiResult::OK);
        load_ctx.finish_init();
    }

    let (clone_ok_cid, clone_err_cid) = {
        let r = src_world.resource::<ModRegistry>();
        (
            r.get_by_handle(clone_ok_h).unwrap().component_id,
            r.get_by_handle(clone_err_h).unwrap().component_id,
        )
    };

    // Pre-register `ZeroPod` natively in `dst_world` to exercise `TypeId` reuse (`2730-2732`).
    let _ = dst_world.register_component::<ZeroPod>();
    // Pre-register mismatched layout name in `dst_world` to exercise `TYPE_MISMATCH` (`2720-2723`).
    let mut dst_reg = ModRegistry::new();
    let (_m_cid, _m_h) = reg_dyn_table_comp(
        &mut dst_world,
        &mut dst_reg,
        "extract_cov::DropComp",
        8,
        8,
        0,
        None,
        None,
        &[],
    );
    dst_world.insert_resource(dst_reg);

    // `ensure_component_in_target_world` branches
    assert_eq!(
        ensure_component_in_target_world(&src_world, &mut dst_world, sparse_h).unwrap(),
        ensure_component_in_target_world(&src_world, &mut dst_world, sparse_h).unwrap()
    );
    assert!(ensure_component_in_target_world(&src_world, &mut dst_world, zp_h).is_ok());
    assert_eq!(
        ensure_component_in_target_world(&src_world, &mut dst_world, drop_h),
        Err(FfiResult::TYPE_MISMATCH)
    );
    assert_eq!(
        ensure_component_in_target_world(&src_world, &mut dst_world, drop2_h),
        Err(FfiResult::UNSUPPORTED)
    );

    // `extract_component_between_worlds` branches
    let src_e = src_world.spawn((ZeroPod, NoInserters { val: 30 })).id();
    let dst_e = dst_world.spawn_empty().id();
    assert_eq!(
        extract_component_between_worlds(&src_world, &mut dst_world, src_e, dst_e, pod_res_h),
        Err(FfiResult::INVALID_COMPONENT)
    );
    assert_eq!(
        extract_component_between_worlds(
            &src_world,
            &mut dst_world,
            Entity::PLACEHOLDER,
            dst_e,
            raw_h
        ),
        Ok(false)
    );
    assert_eq!(
        extract_component_between_worlds(&src_world, &mut dst_world, src_e, dst_e, raw_h),
        Ok(false)
    );
    assert_eq!(
        extract_component_between_worlds(
            &src_world,
            &mut dst_world,
            src_e,
            Entity::PLACEHOLDER,
            zp_h
        ),
        Err(FfiResult::INVALID_ENTITY)
    );
    assert_eq!(
        extract_component_between_worlds(&src_world, &mut dst_world, src_e, dst_e, zp_h),
        Ok(true)
    );

    // Clone hook & non-POD error branches
    // SAFETY: `4`-byte aligned `u32` values match `clone_ok_cid`, `clone_err_cid`, `sparse_cid`.
    unsafe {
        OwningPtr::make(10u32, |p| {
            src_world.entity_mut(src_e).insert_by_id(clone_ok_cid, p);
        });
        OwningPtr::make(20u32, |p| {
            src_world.entity_mut(src_e).insert_by_id(clone_err_cid, p);
        });
        OwningPtr::make(40u32, |p| {
            src_world.entity_mut(src_e).insert_by_id(sparse_cid, p);
        });
    }
    assert_eq!(
        extract_component_between_worlds(&src_world, &mut dst_world, src_e, dst_e, clone_ok_h),
        Ok(true)
    );
    assert_eq!(
        extract_component_between_worlds(&src_world, &mut dst_world, src_e, dst_e, clone_err_h),
        Err(FfiResult::INVALID_STATE)
    );
    assert_eq!(
        extract_component_between_worlds(&src_world, &mut dst_world, src_e, dst_e, no_ins_h),
        Err(FfiResult::UNSUPPORTED)
    );
    assert_eq!(
        extract_component_between_worlds(&src_world, &mut dst_world, src_e, dst_e, sparse_h),
        Ok(true)
    );

    // `extract_resource_between_worlds` branches
    assert_eq!(
        extract_resource_between_worlds(&src_world, &mut dst_world, raw_h),
        Err(FfiResult::INVALID_COMPONENT)
    );
    assert_eq!(
        extract_resource_between_worlds(&src_world, &mut dst_world, clock_h),
        Err(FfiResult::UNSUPPORTED)
    );
    assert_eq!(
        extract_resource_between_worlds(&src_world, &mut dst_world, pod_res_h),
        Ok(false)
    );

    src_world.insert_resource(PodClockRes { step: 7, dt: 0.25 });
    src_world.insert_resource(ZeroPodRes);
    assert_eq!(
        extract_resource_between_worlds(&src_world, &mut dst_world, pod_res_h),
        Ok(true)
    );
    assert_eq!(
        extract_resource_between_worlds(&src_world, &mut dst_world, zres_h),
        Ok(true)
    );
    // Overwrite existing resource in `dst_world`
    src_world.insert_resource(PodClockRes { step: 99, dt: 1.5 });
    assert_eq!(
        extract_resource_between_worlds(&src_world, &mut dst_world, pod_res_h),
        Ok(true)
    );
    let dst_pod_res_cid = dst_world
        .resource::<ModRegistry>()
        .get_by_name("val_cov::PodClockRes")
        .unwrap()
        .component_id;
    // SAFETY: `dst_pod_res_cid` holds initialized `PodClockRes`.
    let dst_val = unsafe {
        *dst_world
            .get_resource_by_id(dst_pod_res_cid)
            .unwrap()
            .deref::<PodClockRes>()
    };
    assert_eq!(dst_val, PodClockRes { step: 99, dt: 1.5 });
}

#[cfg(feature = "bridge-reflect")]
#[test]
fn bridge_reflect_pool_covers_struct_reflect_traits_and_component_fns() {
    use bevy_ecs::entity::{EntityHashMap, SceneEntityMapper};
    use bevy_ecs::reflect::{AppTypeRegistry, ReflectComponent};
    use bevy_ecs::relationship::RelationshipHookMode;
    use bevy_reflect::ReflectFromPtr;
    use bevy_reflect::structs::DynamicStruct;

    let mut world = World::new();
    world.insert_resource(AppTypeRegistry::default());
    let mut reg = ModRegistry::new();

    // 1. Non-inplace-reflectable misaligned u32 (offset 1, align 4) -> `Ok(None)`
    let misalign_fields = [BamFieldDesc::new(
        BamFieldKind::U32,
        StrView::from_str("bad"),
        StrView::from_str("u32"),
        1,
        4,
        1,
    )];
    let (_misaligned_cid, misaligned_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "refl_cov::Misaligned",
        8,
        1,
        0,
        None,
        None,
        &misalign_fields,
    );

    // 2. Single-segment name (`size 16, align 8` valid Bevy layout for u32 + Entity)
    let flat_fields = [
        BamFieldDesc::new(
            BamFieldKind::U32,
            StrView::from_str("id"),
            StrView::from_str("u32"),
            0,
            4,
            4,
        ),
        BamFieldDesc::new(
            BamFieldKind::U64,
            StrView::from_str("owner"),
            StrView::from_str("Entity"),
            8,
            8,
            8,
        ),
    ];
    let (flat_cid, flat_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "FlatReflectComp",
        16,
        8,
        0,
        None,
        None,
        &flat_fields,
    );

    // 2b. Non-exact size `65, align 1` -> exercises fallback slot `64`
    let fallback_fields = [BamFieldDesc::new(
        BamFieldKind::U8,
        StrView::from_str("tag"),
        StrView::from_str("u8"),
        0,
        1,
        1,
    )];
    let (_fb_cid, fb_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "refl_cov::Fallback65",
        65,
        1,
        0,
        None,
        None,
        &fallback_fields,
    );

    // 3. Immutable component & empty-fields component
    let imm_fields = [BamFieldDesc::new(
        BamFieldKind::U32,
        StrView::from_str("v"),
        StrView::from_str("u32"),
        0,
        4,
        4,
    )];
    let (imm_cid, imm_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "refl_cov::ImmComp",
        4,
        4,
        BAM_COMPONENT_FLAG_IMMUTABLE,
        None,
        None,
        &imm_fields,
    );
    let (_empty_cid, empty_h) = reg_dyn_table_comp(
        &mut world,
        &mut reg,
        "refl_cov::EmptyFields",
        4,
        4,
        0,
        None,
        None,
        &[],
    );

    world.insert_resource(reg);

    assert_eq!(
        register_bam_field_reflect_type(&mut world, empty_h),
        Ok(None)
    );
    assert_eq!(
        register_bam_field_reflect_type(&mut world, misaligned_h),
        Ok(None)
    );
    assert!(
        register_bam_field_reflect_type(&mut world, fb_h)
            .unwrap()
            .is_some()
    );

    let flat_tid = register_bam_field_reflect_type(&mut world, flat_h)
        .unwrap()
        .expect("flat 16B struct fits 16B slot");
    // Second call hits `guard.by_key` cache.
    assert_eq!(
        register_bam_field_reflect_type(&mut world, flat_h).unwrap(),
        Some(flat_tid)
    );
    let imm_tid = register_bam_field_reflect_type(&mut world, imm_h)
        .unwrap()
        .unwrap();

    let (flat_rfp, flat_rc, imm_rfp) = {
        let app_types = world.resource::<AppTypeRegistry>().read();
        let flat_reg = app_types.get(flat_tid).unwrap();
        let imm_reg = app_types.get(imm_tid).unwrap();
        (
            flat_reg.data::<ReflectFromPtr>().unwrap().clone(),
            flat_reg.data::<ReflectComponent>().unwrap().clone(),
            imm_reg.data::<ReflectFromPtr>().unwrap().clone(),
        )
    };

    // Exercise `ReflectComponent` Fns (`insert`, `apply`, `apply_or_insert_mapped`, `contains`,
    // `reflect`, `reflect_mut`, `map_entities`, `reflect_unchecked_mut`, `copy`, `remove`,
    // `register_component`) + `Struct` / `PartialReflect` / `Reflect` / `OwnedBamFieldStruct`.
    let target_ent = world.spawn_empty().id();
    let mut dyn_init = DynamicStruct::default();
    dyn_init.insert("id", 7u32);
    dyn_init.insert("owner", target_ent); // tests `Entity -> u64` conversion in `apply_struct_to_slot`
    dyn_init.insert("ignored_extra", 123u32);

    let e1 = world.spawn_empty().id();
    {
        let app_types = world.resource::<AppTypeRegistry>().clone();
        let guard = app_types.read();
        let mut entity_map = EntityHashMap::default();
        SceneEntityMapper::world_scope(&mut entity_map, &mut world, |w, mapper| {
            let mut e_mut = w.entity_mut(e1);
            flat_rc.apply_or_insert_mapped(
                &mut e_mut,
                &dyn_init,
                &guard,
                mapper,
                RelationshipHookMode::Run,
            );
        });
    }
    assert!(flat_rc.contains(world.entity(e1)));

    let mut dyn_patch = DynamicStruct::default();
    dyn_patch.insert("id", 42u32);
    {
        let app_types = world.resource::<AppTypeRegistry>().clone();
        let guard = app_types.read();
        let mut entity_map = EntityHashMap::default();
        SceneEntityMapper::world_scope(&mut entity_map, &mut world, |w, mapper| {
            let mut e_mut = w.entity_mut(e1);
            flat_rc.apply(&mut e_mut, &dyn_patch);
            flat_rc.apply_or_insert_mapped(
                &mut e_mut,
                &dyn_patch,
                &guard,
                mapper,
                RelationshipHookMode::Run,
            );
        });
    }

    // Read & mutate via `ReflectComponent::reflect` / `reflect_mut` / `reflect_unchecked_mut`
    {
        let e_ref = world.entity(e1);
        let r = flat_rc.reflect(e_ref).unwrap();
        let s = r.reflect_ref().as_struct().unwrap();
        assert_eq!(s.field_len(), 2);
        assert_eq!(s.name_at(0), Some("id"));
        assert_eq!(s.index_of_name("owner"), Some(1));
        assert!(s.field("missing").is_none());
        assert_eq!(
            *s.field_at(0).unwrap().try_downcast_ref::<u32>().unwrap(),
            42
        );
        assert_eq!(
            *s.field("owner").unwrap().try_downcast_ref::<u64>().unwrap(),
            target_ent.to_bits()
        );
        assert_eq!(s.iter_fields().count(), 2);
        assert!(r.get_represented_type_info().is_some());
        assert_eq!(r.reflect_kind(), bevy_reflect::ReflectKind::Struct);
        assert!(r.try_as_reflect().is_some());
        assert!(r.reflect_partial_eq(r.as_partial_reflect()).is_some());
        let _ = r.reflect_partial_cmp(r.as_partial_reflect());
        assert!(!format!("{r:?}").is_empty());

        // Clone into `OwnedBamFieldStruct` and exercise all trait methods on it too!
        let mut cloned = r.reflect_clone().unwrap();
        assert!(cloned.get_represented_type_info().is_some());
        assert!(cloned.try_as_reflect().is_some());
        assert!(cloned.try_as_reflect_mut().is_some());
        assert!(cloned.as_reflect().as_any().type_id() == cloned.type_id());
        assert!(cloned.as_reflect_mut().as_any_mut().type_id() == cloned.type_id());
        assert!(cloned.reflect_partial_eq(r.as_partial_reflect()).is_some());
        let _ = cloned.reflect_partial_cmp(r.as_partial_reflect());
        assert!(!format!("{cloned:?}").is_empty());
        let cloned2 = cloned.reflect_clone().unwrap();
        {
            let cs = cloned.reflect_mut().as_struct().unwrap();
            assert_eq!(cs.field_len(), 2);
            assert_eq!(cs.name_at(1), Some("owner"));
            assert_eq!(cs.index_of_name("id"), Some(0));
            assert!(cs.field("missing").is_none());
            *cs.field_mut("id")
                .unwrap()
                .try_downcast_mut::<u32>()
                .unwrap() = 55;
            *cs.field_at_mut(1)
                .unwrap()
                .try_downcast_mut::<u64>()
                .unwrap() = 999;
        }
        assert!(cloned.set(cloned2).is_ok());
        assert!(cloned.set(Box::new(123u32)).is_err());
        let _ = cloned.into_reflect().into_any();
    }

    {
        #[derive(bevy_reflect::Reflect)]
        struct PatchStruct {
            id: u32,
        }
        let mut e_mut = world.entity_mut(e1);
        let mut r_mut = flat_rc.reflect_mut(&mut e_mut).unwrap();
        let _ = r_mut.as_partial_reflect_mut();
        assert!(r_mut.try_as_reflect_mut().is_some());
        assert!(r_mut.set(Box::new(PatchStruct { id: 42 })).is_ok());
        assert!(r_mut.set(Box::new(123u32)).is_err());
        let s_mut = r_mut.reflect_mut().as_struct().unwrap();
        *s_mut
            .field_mut("id")
            .unwrap()
            .try_downcast_mut::<u32>()
            .unwrap() = 88;
        let _ = s_mut.field_at_mut(0);
    }

    // `reflect_unchecked_mut` and `map_entities`
    {
        let mut entity_map = EntityHashMap::default();
        SceneEntityMapper::world_scope(&mut entity_map, &mut world, |w, mapper| {
            let cell = w.as_unsafe_world_cell().get_entity(e1).unwrap();
            // SAFETY: Exclusive borrow of `w` guarantees unique mutable access to `e1`.
            let mut unchecked = unsafe { flat_rc.reflect_unchecked_mut(cell) }.unwrap();
            flat_rc.map_entities(&mut *unchecked, mapper);
        });
    }

    // `copy` between worlds
    let mut dst_world = World::new();
    let _dst_cid = dst_world.register_component_with_descriptor(
        // SAFETY: Valid 16B POD layout matching `flat_cid`.
        unsafe {
            ComponentDescriptor::new_with_layout(
                "FlatReflectComp".to_owned(),
                StorageType::Table,
                Layout::from_size_align(16, 8).unwrap(),
                None,
                true,
                false,
                ComponentCloneBehavior::Default,
                None,
            )
        },
    );
    let dst_e = dst_world.spawn_empty().id();
    let types_clone = world.resource::<AppTypeRegistry>().clone();
    {
        let guard = types_clone.read();
        flat_rc.copy(&world, &mut dst_world, e1, dst_e, &guard);
    }
    let reg_cid = flat_rc.register_component(&mut world);
    assert_eq!(reg_cid, flat_cid);

    // `remove` and `take`
    let taken = flat_rc.take(&mut world.entity_mut(e1)).unwrap();
    let _ = taken.into_partial_reflect();
    assert!(!flat_rc.contains(world.entity(e1)));
    flat_rc.insert(&mut world.entity_mut(e1), &dyn_init, &types_clone.read());
    flat_rc.remove(&mut world.entity_mut(e1));
    assert!(!flat_rc.contains(world.entity(e1)));

    // Mismatched/immutable checks on `reflect_entity_component*` & `reflect_resource_component*`
    assert!(reflect_entity_component(&world, world.entity(e1), flat_cid, &imm_rfp).is_none());
    assert!(reflect_entity_component_mut(&mut world.entity_mut(e1), imm_cid, &imm_rfp).is_none());
    assert!(reflect_entity_component_mut(&mut world.entity_mut(e1), flat_cid, &imm_rfp).is_none());
    assert!(reflect_resource_component(&world, flat_cid, &imm_rfp).is_none());
    assert!(reflect_resource_component_mut(&mut world, imm_cid, &imm_rfp).is_none());
    assert!(reflect_resource_component_mut(&mut world, flat_cid, &imm_rfp).is_none());
    let _ = flat_rfp;
}
