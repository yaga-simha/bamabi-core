use super::*;
use bamabi_macros::ModComponent;
use bamabi_sys::{ComponentDesc, FfiChildOf, ModComponent};
use bevy_ecs::event::{EntityEvent, Event};
use bevy_ecs::message::Message;

#[repr(C)]
#[derive(Message, Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "cat_cov::ZstMsg")]
struct ZstMsg;

#[repr(C)]
#[derive(Message, Component, Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "cat_cov::PodMsg")]
struct PodMsg {
    v: u32,
}

#[derive(Message, Component, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "cat_cov::NonPodMsg")]
struct NonPodMsg {
    text: String,
    count: u32,
}

#[derive(Message, Default, Debug, PartialEq)]
struct NoImportMsg {
    val: u32,
}
impl ModComponent for NoImportMsg {
    const CANONICAL_NAME: &'static str = "cat_cov::NoImportMsg";
    const COMPONENT_CSTR: &'static core::ffi::CStr = c"cat_cov::NoImportMsg";
    const UUID: ComponentUuid = ComponentUuid::from_canonical_name("cat_cov::NoImportMsg");
    type VTable = ();
    fn vtable() -> &'static Self::VTable {
        &()
    }
    fn symbols() -> &'static [bamabi_sys::SymbolEntry] {
        &[]
    }
}

#[derive(Event, Clone, Copy, Default, ModComponent)]
#[mod_component(name = "cat_cov::ZstEvent")]
struct ZstEvent;

#[repr(C)]
#[derive(EntityEvent, Clone, Copy)]
struct TargetEv {
    entity: Entity,
}
impl ModComponent for TargetEv {
    const CANONICAL_NAME: &'static str = "cat_cov::TargetEv";
    const COMPONENT_CSTR: &'static core::ffi::CStr = c"cat_cov::TargetEv";
    const UUID: ComponentUuid = ComponentUuid::from_canonical_name("cat_cov::TargetEv");
    type VTable = ();
    fn vtable() -> &'static Self::VTable {
        &()
    }
    fn symbols() -> &'static [bamabi_sys::SymbolEntry] {
        &[]
    }
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, ModComponent)]
#[mod_component(name = "cat_cov::ZstComp")]
struct ZstComp;

#[test]
fn target_entity_options_snapshot_evict_restore_and_collisions() {
    let mut world = World::new();
    let e = world.spawn_empty().id();

    // `TargetEntity` conversions & `ContainsEntity`
    let te_default = TargetEntity::default();
    assert_eq!(te_default.to_entity(), Entity::PLACEHOLDER);
    let te = TargetEntity::from(e);
    let ffi_e: FfiEntity = te.into();
    let te2 = TargetEntity::from(ffi_e);
    assert_eq!(te2.entity(), e);
    assert_eq!(*te2, ffi_e);

    // `ComponentOptions` Clone, Debug, Default
    let opts = ComponentOptions::<ZstComp>::default().with_default();
    let opts_copy = opts;
    assert!(format!("{opts_copy:?}").contains("ComponentOptions"));

    let mut reg = ModRegistry::new();
    let z_cid = world.register_component::<ZstComp>();
    let z_h = reg.register_component::<ZstComp>(z_cid, opts).unwrap();

    // `Snapshot` / `evict_mod` / `restore_snapshot`
    let pre_snap = reg.snapshot_arc();
    let mod1 = ModId(10);
    static SYM_A_SENTINEL: u8 = 0;
    static SYM_B_SENTINEL: u8 = 0;
    // SAFETY: Static sentinel addresses are non-null and never dereferenced through the registry.
    unsafe {
        reg.register_mod_symbol(
            "cat_cov::sym_a",
            (&raw const SYM_A_SENTINEL).cast::<c_void>(),
            mod1,
        );
        reg.register_mod_symbol(
            "bam.v0.cat_cov::sym_b",
            (&raw const SYM_B_SENTINEL).cast::<c_void>(),
            mod1,
        );
    }
    assert!(reg.resolve_symbol("bam.v0.cat_cov::sym_a").is_some());
    assert!(reg.resolve_symbol("bam.v0.cat_cov::sym_b").is_some());

    // Register a dynamic component owned by `mod1`
    let dyn_desc = ComponentDesc::new(
        c"cat_cov::Dyn".as_ptr(),
        ComponentUuid::from_canonical_name("cat_cov::Dyn"),
        4,
        4,
    );
    let dyn_cid = world.register_component::<PodMsg>();
    let dyn_h = reg
        .register_dynamic_component(DynamicComponentReg {
            comp_id: dyn_cid,
            desc: &dyn_desc,
            name: "cat_cov::Dyn",
            owner: Some(mod1),
            drop_slot: None,
            required_components: Box::new([]),
            schema_fingerprint: 0,
            fields: Arc::from([]),
            owned_field_strings: None,
        })
        .unwrap();
    reg.evict_mod(mod1);
    assert!(reg.resolve_symbol("bam.v0.cat_cov::sym_a").is_none());
    assert!(reg.get_by_handle(dyn_h).unwrap().owner.is_none());

    reg.restore_snapshot(pre_snap);
    assert!(reg.get_by_handle(dyn_h).is_none());
    assert_eq!(reg.get_by_handle(z_h).unwrap().handle, z_h);

    // Collision / compatibility error checks
    let other_cid = world.register_component::<NonPodMsg>();
    assert!(matches!(
        reg.check_registration_collision(
            "cat_cov::ZstComp",
            ZstComp::UUID,
            0,
            1,
            reg.get_by_handle(z_h).unwrap().storage_flags,
            Some(other_cid),
        ),
        Err(ComponentCollisionError::MetadataMismatch { .. })
    ));
    assert!(matches!(
        reg.check_dynamic_drop_compatibility("cat_cov::ZstComp", true),
        Err(ComponentCollisionError::MetadataMismatch { .. })
    ));
    assert!(matches!(
        reg.check_dynamic_hook_compatibility("cat_cov::ZstComp", true, false),
        Err(ComponentCollisionError::MetadataMismatch { .. })
    ));
    let hook_desc = dyn_desc.with_hooks(None, None, core::ptr::null_mut());
    assert!(matches!(
        reg.check_dynamic_extended_hook_compatibility(
            "cat_cov::ZstComp",
            &hook_desc.with_clone_fn({
                unsafe extern "C" fn c(
                    _: *const c_void,
                    _: *mut c_void,
                    _: *mut c_void,
                ) -> FfiResult {
                    FfiResult::OK
                }
                c
            })
        ),
        Err(ComponentCollisionError::MetadataMismatch { .. })
    ));

    // Dynamic mismatch against existing dynamic entry
    let _ = reg
        .register_dynamic_component(DynamicComponentReg {
            comp_id: dyn_cid,
            desc: &dyn_desc,
            name: "cat_cov::Dyn2",
            owner: None,
            drop_slot: None,
            required_components: Box::new([]),
            schema_fingerprint: 0,
            fields: Arc::from([]),
            owned_field_strings: None,
        })
        .unwrap();
    assert!(
        reg.check_dynamic_drop_compatibility("cat_cov::Dyn2", true)
            .is_err()
    );
    assert!(
        reg.check_dynamic_hook_compatibility("cat_cov::Dyn2", true, false)
            .is_err()
    );
    let mut desc_with_insert = dyn_desc;
    unsafe extern "C" fn noop_hook(_: bamabi_sys::FfiEntity, _: *const c_void, _: *mut c_void) {}
    desc_with_insert = desc_with_insert.with_on_insert(noop_hook);
    assert!(
        reg.check_dynamic_extended_hook_compatibility("cat_cov::Dyn2", &desc_with_insert)
            .is_err()
    );

    // `extract_owned_dynamic_fields` error paths
    let null_fields = dyn_desc.with_fields(core::ptr::null(), 1);
    let null_res =
        // SAFETY: Null pointer check happens before slice construction.
        unsafe { extract_owned_dynamic_fields(&null_fields) };
    assert_eq!(null_res.unwrap_err().0, FfiResult::NULL_POINTER);
    let mut bad_sz_field = BamFieldDesc::new(
        bamabi_sys::BamFieldKind::U32,
        StrView::from_str("x"),
        StrView::from_str("u32"),
        0,
        4,
        4,
    );
    bad_sz_field.struct_size = 4;
    let bad_sz_desc = dyn_desc.with_fields(&raw const bad_sz_field, 1);
    let bad_sz_res =
        // SAFETY: `bad_sz_field` is a valid stack descriptor.
        unsafe { extract_owned_dynamic_fields(&bad_sz_desc) };
    assert_eq!(bad_sz_res.unwrap_err().0, FfiResult::INVALID_VERSION);
    let bad_utf8 = [0xFFu8];
    let bad_tag_field = BamFieldDesc::new(
        bamabi_sys::BamFieldKind::U32,
        StrView::from_str("ok"),
        StrView {
            ptr: bad_utf8.as_ptr(),
            len: 1,
        },
        0,
        4,
        4,
    );
    let bad_tag_desc = dyn_desc.with_fields(&raw const bad_tag_field, 1);
    let bad_tag_res =
        // SAFETY: `bad_tag_field` is a valid stack descriptor.
        unsafe { extract_owned_dynamic_fields(&bad_tag_desc) };
    assert_eq!(bad_tag_res.unwrap_err().0, FfiResult::INVALID_UTF8);

    // Relation metadata lookup helpers & collisions
    let rel_cid = world.register_component::<ZstComp>();
    let rel = reg
        .register_relation_entry(RelationRegSpec {
            comp_id: rel_cid,
            name: "cat_cov::Rel",
            uuid: ComponentUuid::from_canonical_name("cat_cov::Rel"),
            flags: 0,
            data_size: 0,
            data_align: 1,
            max_degree: 0,
            owner: Some(mod1),
        })
        .unwrap();
    assert_eq!(reg.relation_count(), 1);
    assert!(reg.is_relation(rel.handle));
    assert!(reg.get_relation(rel.handle).is_some());
    assert!(reg.get_relation_by_id(rel_cid).is_some());
    assert!(reg.get_relation_by_uuid(&rel.uuid).is_some());
    assert!(reg.required_components(rel.handle).is_empty());
    // Idempotent re-registration of identical relation
    assert!(
        reg.register_relation_entry(RelationRegSpec {
            comp_id: rel_cid,
            name: "cat_cov::Rel",
            uuid: ComponentUuid::from_canonical_name("cat_cov::Rel"),
            flags: 0,
            data_size: 0,
            data_align: 1,
            max_degree: 0,
            owner: None,
        })
        .is_ok()
    );
}

#[test]
fn registry_message_event_raw_inserter_and_hierarchy_edge_paths() {
    let mut world = World::new();
    let mut reg = ModRegistry::new();

    // Register ZST message, POD message, NonPod message (twice -> re-reg branch), NoImport message,
    // ZST event, Entity event, ZST component, and ChildOf hierarchy.
    let zmsg_cid = world.register_component::<Messages<ZstMsg>>();
    let zmsg_h = reg.register_message::<ZstMsg>(zmsg_cid).unwrap();
    let pmsg_cid = world.register_component::<Messages<PodMsg>>();
    let pmsg_h = reg.register_message::<PodMsg>(pmsg_cid).unwrap();
    let npmsg_cid = world.register_component::<Messages<NonPodMsg>>();
    let npmsg_h = reg
        .register_non_pod_message::<NonPodMsg>(npmsg_cid)
        .unwrap();
    let _npmsg_h2 = reg
        .register_non_pod_message::<NonPodMsg>(npmsg_cid)
        .unwrap();
    let noimp_cid = world.register_component::<Messages<NoImportMsg>>();
    let noimp_h = reg
        .register_non_pod_message::<NoImportMsg>(noimp_cid)
        .unwrap();

    let zev_key = world.register_event_key::<ZstEvent>();
    let zev_h = reg.register_event::<ZstEvent>(zev_key).unwrap();
    let eev_key = world.register_event_key::<TargetEv>();
    let eev_h = reg.register_entity_event::<TargetEv>(eev_key).unwrap();

    let zcomp_cid = world.register_component::<ZstComp>();
    let zcomp_h = reg
        .register_component::<ZstComp>(zcomp_cid, ComponentOptions::new())
        .unwrap();
    let child_of_cid = world.register_component::<ChildOf>();
    let child_of_h = reg.register_hierarchy(child_of_cid, true).unwrap();

    // Collision on `register_hierarchy` / `register_event` / `register_entity_event`
    assert!(reg.register_hierarchy(zcomp_cid, true).is_err());
    assert!(reg.register_event::<ZstEvent>(eev_key).is_err());
    assert!(reg.register_entity_event::<TargetEv>(zev_key).is_err());

    let e1 = world.spawn_empty().id();
    let e2 = world.spawn_empty().id();

    // Raw inserters: ZST + ChildOf edge cases
    let raw_z = reg.get_by_handle(zcomp_h).unwrap().insert_raw_fn.unwrap();
    let raw_co = reg
        .get_by_handle(child_of_h)
        .unwrap()
        .insert_raw_fn
        .unwrap();
    // SAFETY: ZST `insert_raw_fn` ignores `data`; `ChildOf` tests null, invalid bits, self-parent, dead parent.
    unsafe {
        raw_z(&mut world, e1, core::ptr::null());
        assert!(world.entity(e1).get::<ZstComp>().is_some());

        raw_co(&mut world, e1, core::ptr::null());
        let bad_bits = u64::MAX.to_ne_bytes();
        raw_co(&mut world, e1, bad_bits.as_ptr());
        let self_bits = FfiChildOf {
            parent: FfiEntity::from_bits(e1.to_bits()),
        };
        raw_co(&mut world, e1, (&raw const self_bits).cast::<u8>());
        let ok_parent = FfiChildOf {
            parent: FfiEntity::from_bits(e2.to_bits()),
        };
        raw_co(&mut world, e1, (&raw const ok_parent).cast::<u8>());
        assert_eq!(
            world.entity(e1).get::<ChildOf>().map(|c| c.parent()),
            Some(e2)
        );
    }

    // Event & EntityEvent triggers (ZST, null, dead entity)
    let trig_zev = reg.get_by_handle(zev_h).unwrap().trigger_event_fn.unwrap();
    let trig_eev = reg.get_by_handle(eev_h).unwrap().trigger_event_fn.unwrap();
    let ext_eev = reg
        .get_by_handle(eev_h)
        .unwrap()
        .extract_event_target_fn
        .unwrap();
    // SAFETY: Null pointers and valid target pointers match `trigger_event_fn` / `extract_event_target_fn`.
    unsafe {
        assert_eq!(trig_zev(&mut world, None, core::ptr::null()), Ok(()));
        assert_eq!(
            trig_eev(&mut world, None, core::ptr::null()),
            Err(FfiResult::NULL_POINTER)
        );
        let dead_e = world.spawn_empty().id();
        assert!(world.despawn(dead_e));
        let ev_dead = TargetEv { entity: dead_e };
        assert_eq!(ext_eev((&raw const ev_dead).cast::<u8>()), Some(dead_e));
        assert_eq!(
            trig_eev(&mut world, None, (&raw const ev_dead).cast::<u8>()),
            Err(FfiResult::INVALID_ENTITY)
        );
        assert_eq!(ext_eev(core::ptr::null()), None);
    }

    // Message function pointers: missing resource, wrong cursor, count=0, null buf, ZST, non-POD errors
    let z_read = reg
        .get_by_handle(zmsg_h)
        .unwrap()
        .msg_read_batch_fn
        .unwrap();
    let z_send = reg.get_by_handle(zmsg_h).unwrap().msg_send_fn.unwrap();
    let z_write = reg
        .get_by_handle(zmsg_h)
        .unwrap()
        .msg_write_batch_fn
        .unwrap();
    let p_read = reg
        .get_by_handle(pmsg_h)
        .unwrap()
        .msg_read_batch_fn
        .unwrap();
    let p_send = reg.get_by_handle(pmsg_h).unwrap().msg_send_fn.unwrap();
    let p_write = reg
        .get_by_handle(pmsg_h)
        .unwrap()
        .msg_write_batch_fn
        .unwrap();
    let np_read = reg
        .get_by_handle(npmsg_h)
        .unwrap()
        .msg_read_batch_fn
        .unwrap();
    let np_send = reg.get_by_handle(npmsg_h).unwrap().msg_send_fn.unwrap();
    let noimp_send = reg.get_by_handle(noimp_h).unwrap().msg_send_fn.unwrap();

    let mut z_cursor = (reg
        .get_by_handle(zmsg_h)
        .unwrap()
        .create_msg_cursor_fn
        .unwrap())();
    let mut p_cursor = (reg
        .get_by_handle(pmsg_h)
        .unwrap()
        .create_msg_cursor_fn
        .unwrap())();
    let mut np_cursor = (reg
        .get_by_handle(npmsg_h)
        .unwrap()
        .create_msg_cursor_fn
        .unwrap())();

    let mut out_count = 0usize;
    let tick = world.change_tick();
    {
        let cell = world.as_unsafe_world_cell();
        // SAFETY: Exclusive access to `world` during test, no conflicting borrows live.
        unsafe {
            // Wrong cursor type -> TYPE_MISMATCH
            assert_eq!(
                z_read(
                    cell,
                    zmsg_cid,
                    &mut *p_cursor,
                    core::ptr::null_mut(),
                    1,
                    &mut out_count
                ),
                FfiResult::TYPE_MISMATCH
            );
            assert_eq!(
                np_read(
                    cell,
                    npmsg_cid,
                    &mut *p_cursor,
                    core::ptr::null_mut(),
                    1,
                    &mut out_count
                ),
                FfiResult::TYPE_MISMATCH
            );
            // max_count == 0 / count == 0 -> OK
            assert_eq!(
                z_read(
                    cell,
                    zmsg_cid,
                    &mut *z_cursor,
                    core::ptr::null_mut(),
                    0,
                    &mut out_count
                ),
                FfiResult::OK
            );
            assert_eq!(
                np_read(
                    cell,
                    npmsg_cid,
                    &mut *np_cursor,
                    core::ptr::null_mut(),
                    0,
                    &mut out_count
                ),
                FfiResult::OK
            );
            assert_eq!(
                p_write(cell, tick, tick, pmsg_cid, core::ptr::null(), 0),
                FfiResult::OK
            );
            // Null buffer with count > 0 -> NULL_POINTER
            assert_eq!(
                p_read(
                    cell,
                    pmsg_cid,
                    &mut *p_cursor,
                    core::ptr::null_mut(),
                    1,
                    &mut out_count
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                np_read(
                    cell,
                    npmsg_cid,
                    &mut *np_cursor,
                    core::ptr::null_mut(),
                    1,
                    &mut out_count
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                p_write(cell, tick, tick, pmsg_cid, core::ptr::null(), 1),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                p_send(cell, tick, tick, pmsg_cid, core::ptr::null(), 4),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                p_send(cell, tick, tick, pmsg_cid, core::ptr::null(), 2),
                FfiResult::INVALID_ARGUMENT
            );
            // Resource not yet inserted -> OK on read, NOT_FOUND on send/write
            let mut dummy_out = [0u8; 16];
            assert_eq!(
                p_read(
                    cell,
                    pmsg_cid,
                    &mut *p_cursor,
                    dummy_out.as_mut_ptr(),
                    1,
                    &mut out_count
                ),
                FfiResult::OK
            );
            assert_eq!(
                np_read(
                    cell,
                    npmsg_cid,
                    &mut *np_cursor,
                    dummy_out.as_mut_ptr(),
                    1,
                    &mut out_count
                ),
                FfiResult::OK
            );
            assert_eq!(
                z_send(cell, tick, tick, zmsg_cid, core::ptr::null(), 0),
                FfiResult::NOT_FOUND
            );
            let pod_val = PodMsg { v: 1 };
            assert_eq!(
                p_write(
                    cell,
                    tick,
                    tick,
                    pmsg_cid,
                    (&raw const pod_val).cast::<u8>(),
                    1
                ),
                FfiResult::NOT_FOUND
            );
            // Non-POD packet errors
            assert_eq!(
                np_send(cell, tick, tick, npmsg_cid, core::ptr::null(), 2),
                FfiResult::INVALID_ARGUMENT
            );
            let hdr_only = 0u32.to_le_bytes();
            assert_eq!(
                np_send(cell, tick, tick, npmsg_cid, core::ptr::null(), 4),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                noimp_send(cell, tick, tick, noimp_cid, hdr_only.as_ptr(), 4),
                FfiResult::UNSUPPORTED
            );
        }
    }

    // Now insert `Messages<ZstMsg>`, `Messages<PodMsg>`, and `Messages<NonPodMsg>` and test ZST + overflow + non-POD trailing/utf8!
    world.init_resource::<Messages<ZstMsg>>();
    world.init_resource::<Messages<PodMsg>>();
    world.init_resource::<Messages<NonPodMsg>>();
    let tick = world.change_tick();
    {
        let cell = world.as_unsafe_world_cell();
        // SAFETY: Resources are initialized and exclusively accessed in this test.
        unsafe {
            assert_eq!(
                z_send(cell, tick, tick, zmsg_cid, core::ptr::null(), 0),
                FfiResult::OK
            );
            assert_eq!(
                z_write(cell, tick, tick, zmsg_cid, core::ptr::null(), 3),
                FfiResult::OK
            );
            let mut z_out = [0u8; 4];
            assert_eq!(
                z_read(
                    cell,
                    zmsg_cid,
                    &mut *z_cursor,
                    z_out.as_mut_ptr(),
                    2,
                    &mut out_count
                ),
                FfiResult::OK
            );
            assert_eq!(out_count, 2);

            let pod_arr = [PodMsg { v: 10 }, PodMsg { v: 20 }];
            assert_eq!(
                p_write(cell, tick, tick, pmsg_cid, pod_arr.as_ptr().cast::<u8>(), 2),
                FfiResult::OK
            );
            let mut p_out = [PodMsg::default(); 1];
            assert_eq!(
                p_read(
                    cell,
                    pmsg_cid,
                    &mut *p_cursor,
                    p_out.as_mut_ptr().cast::<u8>(),
                    1,
                    &mut out_count
                ),
                FfiResult::OK
            );
            assert_eq!(out_count, 1);
            assert_eq!(p_out[0], PodMsg { v: 10 });

            // Trailing byte in non-POD packet -> INVALID_ARGUMENT
            let trailing_pkt = [0u32.to_le_bytes().as_slice(), &[0xAA]].concat();
            assert_eq!(
                np_send(
                    cell,
                    tick,
                    tick,
                    npmsg_cid,
                    trailing_pkt.as_ptr(),
                    trailing_pkt.len()
                ),
                FfiResult::INVALID_ARGUMENT
            );
            // Invalid UTF-8 field name in non-POD packet -> INVALID_UTF8
            let mut bad_utf8_pkt = Vec::new();
            bad_utf8_pkt.extend_from_slice(&1u32.to_le_bytes());
            bad_utf8_pkt.extend_from_slice(&1u32.to_le_bytes());
            bad_utf8_pkt.push(0xFF);
            assert_eq!(
                np_send(
                    cell,
                    tick,
                    tick,
                    npmsg_cid,
                    bad_utf8_pkt.as_ptr(),
                    bad_utf8_pkt.len()
                ),
                FfiResult::INVALID_UTF8
            );
        }
    }
}
