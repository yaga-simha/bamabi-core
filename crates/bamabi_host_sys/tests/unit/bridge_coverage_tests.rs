use super::*;
use bamabi_sys::{ComponentDesc, MappedStringVTable, MappedWireVTable};
use bevy_ecs::event::EntityEvent;

#[repr(align(4))]
#[derive(Component, Default)]
struct BridgedPos {
    x: f32,
    y: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct WirePos {
    x: f32,
    y: f32,
}
// SAFETY: `WirePos` is `#[repr(C)]` with no padding and only `f32` fields.
unsafe impl bamabi_sys::RawInsertable for WirePos {}
// SAFETY: `WirePos` is `#[repr(C)]` with no padding and only `f32` fields.
unsafe impl AbiPod for WirePos {}

struct ReadOnlyPosBridge;
impl HostWireBridge<BridgedPos> for ReadOnlyPosBridge {
    type Wire = WirePos;
    const CANONICAL_NAME: &'static str = "bridge_cov::ReadOnlyPos";
    const ALIAS: Option<&'static str> = Some("ReadOnlyPos");
    const SUPPORTS_SET: bool = false;

    fn to_wire(value: &BridgedPos) -> Self::Wire {
        WirePos {
            x: value.x,
            y: value.y,
        }
    }
}

#[derive(Resource, Default)]
struct BridgedConfigRes {
    speed: f32,
    gravity: f32,
}

struct ConfigResBridge;
impl HostWireBridge<BridgedConfigRes> for ConfigResBridge {
    type Wire = WirePos;
    const CANONICAL_NAME: &'static str = "bridge_cov::ConfigRes";
    const ALIAS: Option<&'static str> = Some("ConfigRes");
    const SUPPORTS_SET: bool = true;

    fn to_wire(value: &BridgedConfigRes) -> Self::Wire {
        WirePos {
            x: value.speed,
            y: value.gravity,
        }
    }

    fn apply_wire(value: &mut BridgedConfigRes, wire: Self::Wire) -> FfiResult {
        value.speed = wire.x;
        value.gravity = wire.y;
        FfiResult::OK
    }
}

#[derive(Resource, Default)]
struct ReadOnlyConfigRes(f32);

struct ReadOnlyConfigResBridge;
impl HostWireBridge<ReadOnlyConfigRes> for ReadOnlyConfigResBridge {
    type Wire = WirePos;
    const CANONICAL_NAME: &'static str = "bridge_cov::ReadOnlyConfigRes";
    const SUPPORTS_SET: bool = false;

    fn to_wire(value: &ReadOnlyConfigRes) -> Self::Wire {
        WirePos { x: value.0, y: 0.0 }
    }
}

#[derive(Component, Default)]
struct BridgedLabel(String);

struct ReadOnlyLabelBridge;
impl HostStringBridge<BridgedLabel> for ReadOnlyLabelBridge {
    const CANONICAL_NAME: &'static str = "bridge_cov::ReadOnlyLabel";
    const ALIAS: Option<&'static str> = Some("ReadOnlyLabel");
    const SUPPORTS_SET: bool = false;

    fn as_str(value: &BridgedLabel) -> &str {
        &value.0
    }
}

struct MutableLabelBridge;
impl HostStringBridge<BridgedLabel> for MutableLabelBridge {
    const CANONICAL_NAME: &'static str = "bridge_cov::MutableLabel";
    const SUPPORTS_SET: bool = true;

    fn as_str(value: &BridgedLabel) -> &str {
        &value.0
    }

    fn set_str(value: &mut BridgedLabel, text: &str) -> FfiResult {
        value.0.clear();
        value.0.push_str(text);
        FfiResult::OK
    }
}

#[derive(EntityEvent, Clone, Copy)]
struct SampleEntityEvent {
    entity: Entity,
}

#[test]
fn bridge_wire_string_resource_event_relationship_shims_and_errors() {
    let mut world = World::new();
    let mut reg = ModRegistry::new();

    // 1. Default `apply_wire` / `set_str` return PERMISSION_DENIED
    let mut tmp_pos = BridgedPos::default();
    assert_eq!(
        ReadOnlyPosBridge::apply_wire(&mut tmp_pos, WirePos { x: 1.0, y: 2.0 }),
        FfiResult::PERMISSION_DENIED
    );
    let mut tmp_lbl = BridgedLabel::default();
    assert_eq!(
        ReadOnlyLabelBridge::set_str(&mut tmp_lbl, "abc"),
        FfiResult::PERMISSION_DENIED
    );

    // 2. Wire bridge component (`SUPPORTS_SET = false`) + `default_inserter_for`
    let pos_cid = world.register_component::<BridgedPos>();
    let inserter = ModRegistry::default_inserter_for::<BridgedPos>();
    let pos_h = reg
        .register_bridged_component::<BridgedPos, ReadOnlyPosBridge>(pos_cid, 0, Some(inserter))
        .unwrap();
    let ent = world.spawn_empty().id();
    inserter(&mut world, ent);
    assert!(world.entity(ent).get::<BridgedPos>().is_some());

    // Wire VTable null/unaligned checks (using ConfigResBridge which enables `set_shim`)
    let cfg_cid = world.register_component::<BridgedConfigRes>();
    let cfg_h = reg
        .register_bridged_resource::<BridgedConfigRes, ConfigResBridge>(cfg_cid)
        .unwrap();
    let ro_cfg_cid = world.register_component::<ReadOnlyConfigRes>();
    let ro_cfg_h = reg
        .register_bridged_resource::<ReadOnlyConfigRes, ReadOnlyConfigResBridge>(ro_cfg_cid)
        .unwrap();
    assert!(reg.get_by_handle(ro_cfg_h).unwrap().is_immutable());

    let wire_vt_ptr = reg.get_by_handle(cfg_h).unwrap().vtable_ptr.as_ptr();
    // SAFETY: `ConfigResBridge` registered a static `MappedWireVTable<WirePos>`.
    let wire_vt = unsafe { &*wire_vt_ptr.cast::<MappedWireVTable<WirePos>>() };
    let get_fn = wire_vt.get.unwrap();
    let set_fn = wire_vt.set.unwrap();
    let mut out_wire = WirePos { x: 0.0, y: 0.0 };
    let mut cfg_val = BridgedConfigRes {
        speed: 3.0,
        gravity: 9.8,
    };
    #[repr(C, align(8))]
    struct AlignedBuf([u8; 32]);
    let unaligned_buf = AlignedBuf([0u8; 32]);
    // SAFETY: `unaligned_buf.0` is 8-byte aligned, so offset `+1` is guaranteed misaligned.
    let unaligned_ptr = unsafe { unaligned_buf.0.as_ptr().add(1).cast::<c_void>() };
    // SAFETY: Null and misaligned pointers are rejected by `get_shim` / `set_shim`.
    unsafe {
        assert_eq!(
            get_fn(core::ptr::null(), &raw mut out_wire),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            get_fn(unaligned_ptr, &raw mut out_wire),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            get_fn((&raw const cfg_val).cast::<c_void>(), &raw mut out_wire),
            FfiResult::OK
        );
        assert_eq!(out_wire, WirePos { x: 3.0, y: 9.8 });

        assert_eq!(
            set_fn(core::ptr::null_mut(), &raw const out_wire),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            set_fn(unaligned_ptr.cast_mut(), &raw const out_wire),
            FfiResult::INVALID_ARGUMENT
        );
        let new_wire = WirePos { x: 6.0, y: 1.5 };
        assert_eq!(
            set_fn((&raw mut cfg_val).cast::<c_void>(), &raw const new_wire),
            FfiResult::OK
        );
        assert_eq!(cfg_val.speed, 6.0);
    }

    // 3. String bridge component (`SUPPORTS_SET = false` & `true`) + null/unaligned/UTF-8 shims
    let lbl_cid = world.register_component::<BridgedLabel>();
    let _ro_lbl_h = reg
        .register_bridged_string_component::<BridgedLabel, ReadOnlyLabelBridge>(lbl_cid, 0, None)
        .unwrap();
    let mut_lbl_h = reg
        .register_bridged_string_component::<BridgedLabel, MutableLabelBridge>(lbl_cid, 0, None)
        .unwrap();
    let str_vt_ptr = reg.get_by_handle(mut_lbl_h).unwrap().vtable_ptr.as_ptr();
    // SAFETY: `MutableLabelBridge` registered a static `MappedStringVTable`.
    let str_vt = unsafe { &*str_vt_ptr.cast::<MappedStringVTable>() };
    let str_get = str_vt.get.unwrap();
    let str_set = str_vt.set.unwrap();
    let mut lbl = BridgedLabel("hello".to_owned());
    let bad_utf8 = [0xFFu8];
    let bad_view = StrView {
        ptr: bad_utf8.as_ptr(),
        len: 1,
    };
    // SAFETY: Null, unaligned, and invalid UTF-8 inputs are rejected by `StringVTableFactory`.
    unsafe {
        let mut out_view = StrView::empty();
        assert_eq!(
            str_get(core::ptr::null(), &raw mut out_view),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            str_get(unaligned_ptr, &raw mut out_view),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            str_get((&raw const lbl).cast::<c_void>(), &raw mut out_view),
            FfiResult::OK
        );
        assert_eq!(out_view.try_as_str().unwrap(), "hello");
        let x_view = StrView::from_str("x");
        assert_eq!(
            str_set(core::ptr::null_mut(), &raw const x_view),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            str_set(unaligned_ptr.cast_mut(), &raw const x_view),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            str_set((&raw mut lbl).cast::<c_void>(), &raw const bad_view),
            FfiResult::INVALID_UTF8
        );
        let world_view = StrView::from_str("world");
        assert_eq!(
            str_set((&raw mut lbl).cast::<c_void>(), &raw const world_view),
            FfiResult::OK
        );
        assert_eq!(lbl.0, "world");
    }

    // 4. Bridged entity event (`extract_entity_only_target_impl` null data + valid data)
    let ev_h = reg
        .register_bridged_entity_event_with::<SampleEntityEvent>(
            &mut world,
            "bridge_cov::SampleEv",
            Some("SampleEv"),
            |_w, _e| {},
        )
        .unwrap();
    let extract_ev_fn = reg
        .get_by_handle(ev_h)
        .unwrap()
        .extract_event_target_fn
        .unwrap();
    let sample_ev = SampleEntityEvent { entity: ent };
    // SAFETY: Null pointer returns `None`; valid `SampleEntityEvent` pointer extracts `Some(ent)`.
    unsafe {
        assert_eq!(extract_ev_fn(core::ptr::null()), None);
        assert_eq!(
            extract_ev_fn((&raw const sample_ev).cast::<u8>()),
            Some(ent)
        );
    }

    // 5. Relationship target (`Children`) null/unaligned checks
    let children_cid = world.register_component::<Children>();
    let ch_h = reg.register_children_hierarchy(children_cid).unwrap();
    let extract_rel_fn = reg
        .get_by_handle(ch_h)
        .unwrap()
        .extract_relationship_targets_fn
        .unwrap();
    // SAFETY: Null and misaligned pointers are safely rejected by `extract_relationship_targets_impl`.
    unsafe {
        assert_eq!(extract_rel_fn(core::ptr::null()), (core::ptr::null(), 0));
        assert_eq!(
            extract_rel_fn(unaligned_ptr.cast::<u8>()),
            (core::ptr::null(), 0)
        );
    }

    // 6. Collision errors on bridged registrations
    assert!(
        reg.register_asset_type("bridge_cov::ReadOnlyPos", None, pos_cid)
            .is_err()
    );
    assert!(
        reg.register_bridged_component::<BridgedPos, ReadOnlyPosBridge>(
            cfg_cid, // mismatched ComponentId -> collision
            0, None,
        )
        .is_err()
    );
    assert!(
        reg.register_bridged_resource::<BridgedConfigRes, ConfigResBridge>(pos_cid)
            .is_err()
    );
    assert!(
        reg.register_bridged_string_component::<BridgedLabel, ReadOnlyLabelBridge>(
            pos_cid, 0, None,
        )
        .is_err()
    );
    assert!(
        reg.register_bridged_entity_event_with::<SampleEntityEvent>(
            &mut world,
            "bridge_cov::ReadOnlyPos",
            None,
            |_w, _e| {},
        )
        .is_err()
    );
    assert!(
        reg.register_relationship_target::<Children>("bridge_cov::ReadOnlyPos", None, children_cid)
            .is_err()
    );

    // 7. Re-registering an existing dynamic entry via `insert_bridged_entry` upgrades it to static
    let dyn_cid = world.register_component::<BridgedPos>();
    let dyn_desc = ComponentDesc::new(
        c"bridge_cov::UpgradedComp".as_ptr(),
        ComponentUuid::from_canonical_name("bridge_cov::UpgradedComp"),
        core::mem::size_of::<BridgedPos>(),
        core::mem::align_of::<BridgedPos>(),
    );
    let _ = reg
        .register_dynamic_component(crate::registry::DynamicComponentReg {
            comp_id: dyn_cid,
            desc: &dyn_desc,
            name: "bridge_cov::UpgradedComp",
            owner: Some(ModId(1)),
            drop_slot: None,
            required_components: Box::new([]),
            schema_fingerprint: 0,
            fields: Arc::from([]),
            owned_field_strings: None,
        })
        .unwrap();
    struct UpgradedBridge;
    impl HostWireBridge<BridgedPos> for UpgradedBridge {
        type Wire = WirePos;
        const CANONICAL_NAME: &'static str = "bridge_cov::UpgradedComp";
        fn to_wire(v: &BridgedPos) -> Self::Wire {
            WirePos { x: v.x, y: v.y }
        }
    }
    let upg_h = reg
        .register_bridged_component::<BridgedPos, UpgradedBridge>(dyn_cid, 0, None)
        .unwrap();
    assert!(!reg.get_by_handle(upg_h).unwrap().is_dynamic);

    // 8. `remap_migrated_component` error branches & vtable removal
    let bogus_spec = MigratedComponentSpec {
        handle: BamComponent::INVALID,
        new_comp_id: pos_cid,
        new_uuid: ComponentUuid::from_canonical_name("x"),
        new_size: 4,
        new_align: 4,
        new_storage_flags: 0,
        new_schema_fingerprint: 1,
        new_fields: Arc::from([]),
        new_owned_field_strings: None,
        new_vtable_ptr: core::ptr::null(),
        owner: None,
    };
    assert!(matches!(
        reg.remap_migrated_component(bogus_spec),
        Err(ComponentCollisionError::MetadataMismatch { .. })
    ));

    let static_spec = MigratedComponentSpec {
        handle: pos_h,
        new_comp_id: pos_cid,
        new_uuid: ComponentUuid::from_canonical_name("x"),
        new_size: 8,
        new_align: 4,
        new_storage_flags: 0,
        new_schema_fingerprint: 1,
        new_fields: Arc::from([]),
        new_owned_field_strings: None,
        new_vtable_ptr: core::ptr::null(),
        owner: None,
    };
    assert!(matches!(
        reg.remap_migrated_component(static_spec),
        Err(ComponentCollisionError::MetadataMismatch { .. })
    ));

    let dyn2_cid = world.register_component::<BridgedPos>();
    let dyn2_desc = ComponentDesc::new(
        c"bridge_cov::DynMig".as_ptr(),
        ComponentUuid::from_canonical_name("bridge_cov::DynMig"),
        8,
        4,
    )
    .with_vtable(wire_vt_ptr);
    let dyn2_h = reg
        .register_dynamic_component(crate::registry::DynamicComponentReg {
            comp_id: dyn2_cid,
            desc: &dyn2_desc,
            name: "bridge_cov::DynMig",
            owner: Some(ModId(2)),
            drop_slot: None,
            required_components: Box::new([]),
            schema_fingerprint: 0,
            fields: Arc::from([]),
            owned_field_strings: None,
        })
        .unwrap();

    // UUID collision with `pos_h`
    let collide_spec = MigratedComponentSpec {
        handle: dyn2_h,
        new_comp_id: dyn2_cid,
        new_uuid: ComponentUuid::from_canonical_name("bridge_cov::ReadOnlyPos"),
        new_size: 8,
        new_align: 4,
        new_storage_flags: 0,
        new_schema_fingerprint: 2,
        new_fields: Arc::from([]),
        new_owned_field_strings: None,
        new_vtable_ptr: core::ptr::null(),
        owner: None,
    };
    assert!(matches!(
        reg.remap_migrated_component(collide_spec),
        Err(ComponentCollisionError::UuidCollision { .. })
    ));

    // Valid remap removing previous owner vtable symbol (`new_vtable_ptr = null`)
    let clear_vtable_spec = MigratedComponentSpec {
        handle: dyn2_h,
        new_comp_id: cfg_cid,
        new_uuid: ComponentUuid::from_canonical_name("bridge_cov::DynMigV2"),
        new_size: 8,
        new_align: 4,
        new_storage_flags: 0,
        new_schema_fingerprint: 3,
        new_fields: Arc::from([]),
        new_owned_field_strings: None,
        new_vtable_ptr: core::ptr::null(),
        owner: Some(ModId(3)),
    };
    reg.remap_migrated_component(clear_vtable_spec).unwrap();
    assert!(
        reg.resolve_symbol("bam.v0.bridge_cov::DynMig.vtable")
            .is_none()
    );
}
