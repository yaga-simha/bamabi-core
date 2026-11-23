use super::*;
use crate::ffi::register::{LoadContext, host_register_component, host_register_observer};
use bamabi_sys::{BamComponent, ComponentDesc, ComponentUuid};
use bevy_ecs::event::EntityEvent;
use core::ptr::NonNull;
use std::sync::Mutex;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DamagePayload {
    amount: u32,
}

#[derive(EntityEvent, Clone, Copy, Debug)]
struct TestReady {
    entity: Entity,
}

impl From<Entity> for TestReady {
    fn from(entity: Entity) -> Self {
        Self { entity }
    }
}

struct LogCtx {
    events: Mutex<Vec<(&'static str, FfiEntity)>>,
}

struct MutObsCtx {
    delta: u32,
    stop_propagate: bool,
    seen: Mutex<Vec<(FfiEntity, u32)>>,
}

unsafe extern "C" fn record_add(
    entity: FfiEntity,
    data: *const c_void,
    _cmds: *mut BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `data` points to synthesized `FfiEntity` and `user_data` points to `LogCtx`.
    let wire = unsafe { *data.cast::<FfiEntity>() };
    assert_eq!(wire, entity);
    // SAFETY: `user_data` points to `LogCtx`.
    let log = unsafe { &*user_data.cast::<LogCtx>() };
    log.events.lock().unwrap().push(("add", entity));
    FfiResult::OK
}

unsafe extern "C" fn record_insert(
    entity: FfiEntity,
    data: *const c_void,
    _cmds: *mut BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `data` points to synthesized `FfiEntity`.
    let wire = unsafe { *data.cast::<FfiEntity>() };
    assert_eq!(wire, entity);
    // SAFETY: `user_data` points to `LogCtx`.
    let log = unsafe { &*user_data.cast::<LogCtx>() };
    log.events.lock().unwrap().push(("insert", entity));
    FfiResult::OK
}

unsafe extern "C" fn record_discard(
    entity: FfiEntity,
    data: *const c_void,
    _cmds: *mut BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `data` points to synthesized `FfiEntity`.
    let wire = unsafe { *data.cast::<FfiEntity>() };
    assert_eq!(wire, entity);
    // SAFETY: `user_data` points to `LogCtx`.
    let log = unsafe { &*user_data.cast::<LogCtx>() };
    log.events.lock().unwrap().push(("discard", entity));
    FfiResult::OK
}

unsafe extern "C" fn record_remove(
    entity: FfiEntity,
    data: *const c_void,
    _cmds: *mut BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `data` points to synthesized `FfiEntity`.
    let wire = unsafe { *data.cast::<FfiEntity>() };
    assert_eq!(wire, entity);
    // SAFETY: `user_data` points to `LogCtx`.
    let log = unsafe { &*user_data.cast::<LogCtx>() };
    log.events.lock().unwrap().push(("remove", entity));
    FfiResult::OK
}

unsafe extern "C" fn record_despawn(
    entity: FfiEntity,
    data: *const c_void,
    _cmds: *mut BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `data` points to synthesized `FfiEntity`.
    let wire = unsafe { *data.cast::<FfiEntity>() };
    assert_eq!(wire, entity);
    // SAFETY: `user_data` points to `LogCtx`.
    let log = unsafe { &*user_data.cast::<LogCtx>() };
    log.events.lock().unwrap().push(("despawn", entity));
    FfiResult::OK
}

unsafe extern "C" fn record_ready(
    entity: FfiEntity,
    data: *const c_void,
    _cmds: *mut BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `data` points to synthesized `FfiEntity`.
    let wire = unsafe { *data.cast::<FfiEntity>() };
    assert_eq!(wire, entity);
    // SAFETY: `user_data` points to `LogCtx`.
    let log = unsafe { &*user_data.cast::<LogCtx>() };
    log.events.lock().unwrap().push(("ready", entity));
    FfiResult::OK
}

unsafe extern "C" fn mut_damage_cb(
    entity: FfiEntity,
    event_data: *mut u8,
    propagate: *mut FfiBool,
    _cmds: *mut BamCommands,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `MutObsCtx` and `event_data` points to `DamagePayload`.
    let ctx = unsafe { &*user_data.cast::<MutObsCtx>() };
    // SAFETY: `event_data` is readable/writable for `DamagePayload`.
    let mut dmg = unsafe { core::ptr::read_unaligned(event_data.cast::<DamagePayload>()) };
    dmg.amount += ctx.delta;
    // SAFETY: `event_data` is writable for `DamagePayload`.
    unsafe { core::ptr::write_unaligned(event_data.cast::<DamagePayload>(), dmg) };
    ctx.seen.lock().unwrap().push((entity, dmg.amount));
    if ctx.stop_propagate {
        // SAFETY: `propagate` is writable `FfiBool`.
        unsafe {
            *propagate = FfiBool::FALSE;
        }
    }
    FfiResult::OK
}

#[test]
fn lifecycle_observers_add_insert_discard_remove_despawn_and_ready() {
    let mut world = World::new();
    let mut reg = ModRegistry::new();
    reg.register_bridged_entity_event::<TestReady>(&mut world, "bevy_scene::Ready", Some("Ready"))
        .unwrap();
    let active = Arc::new(AtomicBool::new(true));
    let log = Box::new(LogCtx {
        events: Mutex::new(Vec::new()),
    });
    let log_ptr = (&*log as *const LogCtx).cast_mut().cast::<c_void>();

    let mut comp_handle = BamComponent::INVALID;
    let registered_observers = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg.clone(),
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(1)),
        );
        let comp_desc = ComponentDesc::new(
            c"obs_test::Health".as_ptr(),
            ComponentUuid::from_parts("obs_test::Health", 4, 4, 0),
            4,
            4,
        );
        // SAFETY: Valid `LoadContext` and `ComponentDesc`.
        let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &comp_desc, &mut comp_handle) };
        assert_eq!(rc, FfiResult::OK);

        for (kind, cb, name) in [
            (BamEventKind::ADD, record_add as _, c"on_add"),
            (BamEventKind::INSERT, record_insert as _, c"on_insert"),
            (BamEventKind::DISCARD, record_discard as _, c"on_discard"),
            (BamEventKind::REMOVE, record_remove as _, c"on_remove"),
            (BamEventKind::DESPAWN, record_despawn as _, c"on_despawn"),
        ] {
            let desc = BamObserverDesc::new_lifecycle(kind, &comp_handle, 1, cb, log_ptr)
                .with_name(name.as_ptr());
            // SAFETY: Valid `LoadContext` and `BamObserverDesc`.
            let ro = unsafe { host_register_observer(ctx.as_ffi_ptr(), &desc) };
            assert_eq!(ro, FfiResult::OK);
        }

        let ready_desc = BamObserverDesc::new_lifecycle(
            BamEventKind::READY,
            core::ptr::null(),
            0,
            record_ready,
            log_ptr,
        )
        .with_name(c"on_ready".as_ptr());
        // SAFETY: Valid `LoadContext` and `BamObserverDesc`.
        let rr = unsafe { host_register_observer(ctx.as_ffi_ptr(), &ready_desc) };
        assert_eq!(rr, FfiResult::OK);

        reg = ctx.registry.clone();
        ctx.finish_init();
        core::mem::take(&mut ctx.registered_observers)
    };

    let _obs =
        spawn_registered_observers_in_world(&mut world, registered_observers, &reg, ModId(1));

    let e = world.spawn_empty().id();
    let e_ffi = FfiEntity::from_bits(e.to_bits());
    let comp_id = reg.get_by_handle(comp_handle).unwrap().component_id;

    let mut val: u32 = 10;
    let nn = NonNull::from(&mut val).cast::<u8>();
    // SAFETY: `nn` points to 4 bytes matching `comp_id`.
    unsafe {
        world
            .entity_mut(e)
            .insert_by_id(comp_id, bevy_ecs::ptr::OwningPtr::new(nn));
    }

    let mut val2: u32 = 20;
    let nn2 = NonNull::from(&mut val2).cast::<u8>();
    // SAFETY: `nn2` points to 4 bytes matching `comp_id`.
    unsafe {
        world
            .entity_mut(e)
            .insert_by_id(comp_id, bevy_ecs::ptr::OwningPtr::new(nn2));
    }

    world.entity_mut(e).remove_by_id(comp_id);

    let mut val3: u32 = 30;
    let nn3 = NonNull::from(&mut val3).cast::<u8>();
    // SAFETY: `nn3` points to 4 bytes matching `comp_id`.
    unsafe {
        world
            .entity_mut(e)
            .insert_by_id(comp_id, bevy_ecs::ptr::OwningPtr::new(nn3));
    }
    world.entity_mut(e).despawn();

    // Trigger synthetic READY via `host_cmd_trigger_ex` on a new entity.
    let e2 = world.spawn_empty().id();
    let e2_ffi = FfiEntity::from_bits(e2.to_bits());
    let snap = reg.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(1)), Some(active))
            .with_registry(&reg);
        let trig = BamTriggerDesc::new(BamComponent::INVALID, core::ptr::null_mut(), 0)
            .with_event_kind(BamEventKind::READY)
            .with_entity(e2_ffi);
        // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
        let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
        assert_eq!(rt, FfiResult::OK);
    }
    storage.apply_to_world(&mut world, &snap);

    let recorded = log.events.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![
            ("add", e_ffi),
            ("insert", e_ffi),
            ("discard", e_ffi),
            ("insert", e_ffi),
            ("discard", e_ffi),
            ("remove", e_ffi),
            ("add", e_ffi),
            ("insert", e_ffi),
            ("despawn", e_ffi),
            ("discard", e_ffi),
            ("remove", e_ffi),
            ("ready", e2_ffi),
        ]
    );
}

#[test]
fn mutable_observer_and_child_of_propagation_with_stop_and_full_bubble() {
    let mut world = World::new();
    let reg = ModRegistry::new();
    let active = Arc::new(AtomicBool::new(true));

    let grandparent = world.spawn_empty().id();
    let parent = world.spawn(ChildOf(grandparent)).id();
    let child = world.spawn(ChildOf(parent)).id();

    let child_ctx = Box::new(MutObsCtx {
        delta: 5,
        stop_propagate: false,
        seen: Mutex::new(Vec::new()),
    });
    let parent_stop_ctx = Box::new(MutObsCtx {
        delta: 1,
        stop_propagate: true,
        seen: Mutex::new(Vec::new()),
    });
    let gp_ctx = Box::new(MutObsCtx {
        delta: 100,
        stop_propagate: false,
        seen: Mutex::new(Vec::new()),
    });

    let mut evt_handle = BamComponent::INVALID;
    let (reg, registered_observers) = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(2)),
        );
        let evt_desc = ComponentDesc::new(
            c"obs_test::DamagePayload".as_ptr(),
            ComponentUuid::from_parts("obs_test::DamagePayload", 4, 4, 0),
            4,
            4,
        );
        // SAFETY: Valid `LoadContext` and `ComponentDesc`.
        let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &evt_desc, &mut evt_handle) };
        assert_eq!(rc, FfiResult::OK);

        for (target, ptr, name) in [
            (
                child,
                (&*child_ctx as *const MutObsCtx)
                    .cast_mut()
                    .cast::<c_void>(),
                c"child_obs",
            ),
            (
                parent,
                (&*parent_stop_ctx as *const MutObsCtx)
                    .cast_mut()
                    .cast::<c_void>(),
                c"parent_obs",
            ),
            (
                grandparent,
                (&*gp_ctx as *const MutObsCtx).cast_mut().cast::<c_void>(),
                c"gp_obs",
            ),
        ] {
            let obs = BamObserverDesc::new_mut(evt_handle, mut_damage_cb, ptr)
                .with_name(name.as_ptr())
                .with_entity(FfiEntity::from_bits(target.to_bits()))
                .with_propagate(BamComponent::INVALID);
            // SAFETY: Valid `LoadContext` and `BamObserverDesc`.
            let ro = unsafe { host_register_observer(ctx.as_ffi_ptr(), &obs) };
            assert_eq!(ro, FfiResult::OK);
        }

        let updated_reg = ctx.registry.clone();
        ctx.finish_init();
        (updated_reg, core::mem::take(&mut ctx.registered_observers))
    };

    let _obs =
        spawn_registered_observers_in_world(&mut world, registered_observers, &reg, ModId(2));

    let snap = reg.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(2)), Some(Arc::clone(&active)))
            .with_registry(&reg);
        let mut payload = DamagePayload { amount: 10 };
        let trig = BamTriggerDesc::new(
            evt_handle,
            (&raw mut payload).cast::<u8>(),
            core::mem::size_of::<DamagePayload>(),
        )
        .with_entity(FfiEntity::from_bits(child.to_bits()))
        .with_propagate(BamComponent::INVALID);
        // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
        let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
        assert_eq!(rt, FfiResult::OK);
    }
    storage.apply_to_world(&mut world, &snap);

    assert_eq!(
        *child_ctx.seen.lock().unwrap(),
        vec![(FfiEntity::from_bits(child.to_bits()), 15)]
    );
    assert_eq!(
        *parent_stop_ctx.seen.lock().unwrap(),
        vec![(FfiEntity::from_bits(parent.to_bits()), 16)]
    );
    // Grandparent must NOT have executed because parent set `*propagate = FfiBool::FALSE`.
    assert!(gp_ctx.seen.lock().unwrap().is_empty());

    // Now remove `parent` from hierarchy propagation by triggering directly on `parent` after
    // adding a second non-stopping parent? Better: detach `parent`'s observer and re-attach a
    // non-stopping parent observer via `host_cmd_observe`, verifying full bubble to `grandparent`!
    let parent_continue_ctx = Box::new(MutObsCtx {
        delta: 2,
        stop_propagate: false,
        seen: Mutex::new(Vec::new()),
    });
    let world_obs_entities: Vec<Entity> = world
        .query::<(Entity, &ModObserverState)>()
        .iter(&world)
        .filter(|(_, st)| st.binding.name.as_ref() == "parent_obs")
        .map(|(e, _)| e)
        .collect();
    for e in world_obs_entities {
        world.entity_mut(e).despawn();
    }
    let mut storage2 = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage2, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(2)), Some(active))
            .with_registry(&reg);
        let obs = BamObserverDesc::new_mut(
            evt_handle,
            mut_damage_cb,
            (&*parent_continue_ctx as *const MutObsCtx)
                .cast_mut()
                .cast::<c_void>(),
        )
        .with_entity(FfiEntity::from_bits(parent.to_bits()))
        .with_propagate(BamComponent::INVALID);
        // SAFETY: Valid `HostCommands` and `BamObserverDesc`.
        let ro = unsafe { host_cmd_observe(cmds.as_ffi_ptr(), &obs, core::ptr::null_mut()) };
        assert_eq!(ro, FfiResult::OK);
        let mut payload = DamagePayload { amount: 20 };
        let trig = BamTriggerDesc::new(
            evt_handle,
            (&raw mut payload).cast::<u8>(),
            core::mem::size_of::<DamagePayload>(),
        )
        .with_entity(FfiEntity::from_bits(child.to_bits()))
        .with_propagate(BamComponent::INVALID);
        // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
        let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
        assert_eq!(rt, FfiResult::OK);
    }
    storage2.apply_to_world(&mut world, &snap);

    assert_eq!(
        *gp_ctx.seen.lock().unwrap(),
        vec![(FfiEntity::from_bits(grandparent.to_bits()), 127)]
    );
}

#[test]
fn custom_relationship_propagation_cycle_guard_and_runtime_observe_disarm() {
    let mut world = World::new();
    let reg = ModRegistry::new();
    let active = Arc::new(AtomicBool::new(true));

    let mut evt_handle = BamComponent::INVALID;
    let mut rel_handle = BamComponent::INVALID;
    let reg = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(3)),
        );
        let evt_desc = ComponentDesc::new(
            c"obs_test::Ping".as_ptr(),
            ComponentUuid::from_parts("obs_test::Ping", 4, 4, 0),
            4,
            4,
        );
        let rel_desc = ComponentDesc::new(
            c"obs_test::CustomLink".as_ptr(),
            ComponentUuid::from_parts("obs_test::CustomLink", 8, 8, 0),
            8,
            8,
        );
        // SAFETY: Valid `LoadContext` and descriptors.
        unsafe {
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &evt_desc, &mut evt_handle),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &rel_desc, &mut rel_handle),
                FfiResult::OK
            );
        }
        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let c_unwatched = world.spawn_empty().id();
    let rel_cid = reg.get_by_handle(rel_handle).unwrap().component_id;
    // Create a cycle: a -> b -> a
    let mut link_to_b = FfiEntity::from_bits(b.to_bits());
    let mut link_to_a = FfiEntity::from_bits(a.to_bits());
    // SAFETY: `rel_cid` has 8-byte `FfiEntity` layout.
    unsafe {
        world.entity_mut(a).insert_by_id(
            rel_cid,
            bevy_ecs::ptr::OwningPtr::new(NonNull::from(&mut link_to_b).cast()),
        );
        world.entity_mut(b).insert_by_id(
            rel_cid,
            bevy_ecs::ptr::OwningPtr::new(NonNull::from(&mut link_to_a).cast()),
        );
    }

    let obs_ctx = Box::new(MutObsCtx {
        delta: 1,
        stop_propagate: false,
        seen: Mutex::new(Vec::new()),
    });
    let obs_ptr = (&*obs_ctx as *const MutObsCtx).cast_mut().cast::<c_void>();

    let snap = reg.snapshot_arc();
    let mut obs_a_out = FfiEntity::NULL;
    let mut obs_b_out = FfiEntity::NULL;
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(3)), Some(Arc::clone(&active)))
            .with_registry(&reg);
        let desc_a = BamObserverDesc::new_mut(evt_handle, mut_damage_cb, obs_ptr)
            .with_entity(FfiEntity::from_bits(a.to_bits()));
        let desc_b = BamObserverDesc::new_mut(evt_handle, mut_damage_cb, obs_ptr)
            .with_entity(FfiEntity::from_bits(b.to_bits()));
        // SAFETY: Valid `HostCommands` and `BamObserverDesc`.
        unsafe {
            assert_eq!(
                host_cmd_observe(cmds.as_ffi_ptr(), &desc_a, &mut obs_a_out),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_observe(cmds.as_ffi_ptr(), &desc_b, &mut obs_b_out),
                FfiResult::OK
            );
        }
        assert!(!obs_a_out.is_null());
        assert!(!obs_b_out.is_null());

        let mut payload = DamagePayload { amount: 0 };
        let trig = BamTriggerDesc::new(
            evt_handle,
            (&raw mut payload).cast::<u8>(),
            core::mem::size_of::<DamagePayload>(),
        )
        .with_entity(FfiEntity::from_bits(a.to_bits()))
        .with_propagate(rel_handle);
        // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
        let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
        assert_eq!(rt, FfiResult::OK);
    }
    storage.apply_to_world(&mut world, &snap);

    // Must visit `a` then `b` once and break the cycle cleanly.
    assert_eq!(
        *obs_ctx.seen.lock().unwrap(),
        vec![
            (FfiEntity::from_bits(a.to_bits()), 1),
            (FfiEntity::from_bits(b.to_bits()), 2),
        ]
    );

    // Triggering on unwatched entity `c_unwatched` must not invoke `a`/`b` observers.
    let mut storage2 = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage2, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(3)), Some(Arc::clone(&active)))
            .with_registry(&reg);
        let mut payload = DamagePayload { amount: 5 };
        let trig = BamTriggerDesc::new(
            evt_handle,
            (&raw mut payload).cast::<u8>(),
            core::mem::size_of::<DamagePayload>(),
        )
        .with_entity(FfiEntity::from_bits(c_unwatched.to_bits()));
        // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
        let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
        assert_eq!(rt, FfiResult::OK);
    }
    storage2.apply_to_world(&mut world, &snap);
    assert_eq!(obs_ctx.seen.lock().unwrap().len(), 2);

    // Disarming `is_active` (as during mod unload) prevents runtime observers from executing.
    active.store(false, Ordering::Release);
    let mut storage3 = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage3, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(3)), Some(active))
            .with_registry(&reg);
        let mut payload = DamagePayload { amount: 5 };
        let trig = BamTriggerDesc::new(
            evt_handle,
            (&raw mut payload).cast::<u8>(),
            core::mem::size_of::<DamagePayload>(),
        )
        .with_entity(FfiEntity::from_bits(a.to_bits()));
        // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
        let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
        assert_eq!(rt, FfiResult::OK);
    }
    storage3.apply_to_world(&mut world, &snap);
    assert_eq!(obs_ctx.seen.lock().unwrap().len(), 2);
}

#[derive(bevy_ecs::event::EntityEvent, Clone, Copy, Debug)]
struct NonFromReady {
    #[event_target]
    entity: Entity,
    extra_rust_tag: u128,
}

#[test]
fn non_from_ready_bridged_entity_event_with_supports_ready_trigger_and_propagation() {
    let mut world = World::new();
    let child_of_id = world.register_component::<ChildOf>();
    let mut reg = ModRegistry::new();
    reg.register_hierarchy(child_of_id, false).unwrap();
    let ready_handle = reg
        .register_bridged_entity_event_with::<NonFromReady>(
            &mut world,
            "bevy_scene::Ready",
            Some("Ready"),
            |w, entity| {
                w.trigger(NonFromReady {
                    entity,
                    extra_rust_tag: 0xCAFE_BABE_1234_5678,
                });
            },
        )
        .unwrap();

    let active = Arc::new(AtomicBool::new(true));
    let log = Box::new(LogCtx {
        events: Mutex::new(Vec::new()),
    });
    let log_ptr = (&*log as *const LogCtx).cast_mut().cast::<c_void>();

    let registered_observers = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg.clone(),
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(7)),
        );
        let ready_desc = BamObserverDesc::new_lifecycle(
            BamEventKind::READY,
            core::ptr::null(),
            0,
            record_ready,
            log_ptr,
        )
        .with_name(c"on_non_from_ready".as_ptr());
        // SAFETY: Valid `LoadContext` and `BamObserverDesc`.
        let rr = unsafe { host_register_observer(ctx.as_ffi_ptr(), &ready_desc) };
        assert_eq!(rr, FfiResult::OK);
        reg = ctx.registry.clone();
        ctx.finish_init();
        core::mem::take(&mut ctx.registered_observers)
    };

    let _obs =
        spawn_registered_observers_in_world(&mut world, registered_observers, &reg, ModId(7));
    world.add_observer(|ev: bevy_ecs::observer::On<NonFromReady>| {
        assert_ne!(ev.extra_rust_tag, 0);
    });

    let parent = world.spawn_empty().id();
    let child = world.spawn(ChildOf(parent)).id();
    let parent_ffi = FfiEntity::from_bits(parent.to_bits());
    let child_ffi = FfiEntity::from_bits(child.to_bits());

    // 1. Direct host `world.trigger(NonFromReady { ... })` delivers synthesized 8-byte `FfiEntity`.
    world.trigger(NonFromReady {
        entity: child,
        extra_rust_tag: 99,
    });
    assert_eq!(*log.events.lock().unwrap(), vec![("ready", child_ffi)]);
    log.events.lock().unwrap().clear();

    // 2. `host_cmd_trigger` with `target == FfiEntity::NULL` + 8-byte `FfiEntity` wire payload
    //    must NOT read `size_of::<NonFromReady>()` (24 bytes) out of bounds.
    let snap = reg.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(7)), Some(Arc::clone(&active)))
            .with_registry(&reg);
        let wire_bits = child_ffi.to_bits();
        // SAFETY: `cmds` is valid and `wire_bits` is an initialized 8-byte `FfiEntity` payload.
        let rc = unsafe {
            crate::ffi::commands::host_cmd_trigger(
                cmds.as_ffi_ptr(),
                ready_handle,
                FfiEntity::NULL,
                (&raw const wire_bits).cast::<u8>(),
                core::mem::size_of::<FfiEntity>(),
            )
        };
        assert_eq!(rc, FfiResult::OK);
    }
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(*log.events.lock().unwrap(), vec![("ready", child_ffi)]);
    log.events.lock().unwrap().clear();

    // 3. `host_cmd_trigger_ex` with `BamEventKind::READY` + upward `ChildOf` propagation
    //    fires `NonFromReady` on `child` and then `parent`.
    let mut storage2 = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage2, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(7)), Some(active))
            .with_registry(&reg);
        let trig = BamTriggerDesc::new(BamComponent::INVALID, core::ptr::null_mut(), 0)
            .with_event_kind(BamEventKind::READY)
            .with_entity(child_ffi)
            .with_propagate(BamComponent::INVALID);
        // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
        let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
        assert_eq!(rt, FfiResult::OK);
    }
    storage2.apply_to_world(&mut world, &snap);
    assert_eq!(
        *log.events.lock().unwrap(),
        vec![("ready", child_ffi), ("ready", parent_ffi)]
    );
}

#[repr(C)]
#[derive(bevy_ecs::prelude::Resource, Clone, Copy, Debug, bamabi_macros::ModComponent)]
struct ObsTestRes;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, bamabi_macros::ModComponent)]
struct DamageVal;

#[repr(C)]
#[derive(bevy_ecs::event::Event, Clone, Copy, Debug, bamabi_macros::ModComponent)]
#[mod_component(name = "obs_cov::ZstHostEvt")]
struct ZstHostEvt;

#[repr(C)]
#[derive(
    bevy_ecs::event::EntityEvent, Clone, Copy, Debug, Default, bamabi_macros::ModComponent,
)]
#[mod_component(name = "obs_cov::HostEntEvt")]
struct HostEntEvt {
    #[event_target]
    target: crate::TargetEntity,
}

unsafe extern "C" fn obs_noop_cb(
    _entity: FfiEntity,
    _data: *const c_void,
    _cmds: *mut BamCommands,
    _ud: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

#[test]
#[allow(clippy::cast_ptr_alignment)]
fn observers_ext_all_guard_branches_and_payload_edge_cases() {
    use crate::TargetEntity;
    use core::ptr::{null, null_mut};

    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mut world = World::new();
    let res_cid = world.register_component::<ObsTestRes>();
    let comp_cid = world.register_component::<DamageVal>();
    let zst_evt_key = world.register_event_key::<ZstHostEvt>();
    let ent_evt_key = world.register_event_key::<HostEntEvt>();

    let mut reg = ModRegistry::new();
    let res_h = reg.register_resource::<ObsTestRes>(res_cid).unwrap();
    let comp_h = reg
        .register_component::<DamageVal>(comp_cid, Default::default())
        .unwrap();
    let zst_h = reg.register_event::<ZstHostEvt>(zst_evt_key).unwrap();
    let ent_evt_h = reg
        .register_entity_event::<HostEntEvt>(ent_evt_key)
        .unwrap();

    let active = Arc::new(AtomicBool::new(true));
    let mut dyn_evt_h = BamComponent::INVALID;
    let mut bad_prop_u32_h = BamComponent::INVALID;
    let mut drop_comp_h = BamComponent::INVALID;

    unsafe extern "C" fn dummy_drop(_p: *mut c_void) {}
    let reg = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(10)),
        );
        let d_evt = ComponentDesc::new(
            c"obs_cov::DynEvt".as_ptr(),
            ComponentUuid::from_parts("obs_cov::DynEvt", 4, 4, 0),
            4,
            4,
        );
        let d_u32 = ComponentDesc::new(
            c"obs_cov::U32Rel".as_ptr(),
            ComponentUuid::from_parts("obs_cov::U32Rel", 4, 4, 0),
            4,
            4,
        );
        let d_drop = ComponentDesc::new(
            c"obs_cov::DropComp".as_ptr(),
            ComponentUuid::from_parts("obs_cov::DropComp", 4, 4, 0),
            4,
            4,
        )
        .with_drop(dummy_drop);
        // SAFETY: Valid `LoadContext` and descriptors.
        unsafe {
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &d_evt, &mut dyn_evt_h),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &d_u32, &mut bad_prop_u32_h),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &d_drop, &mut drop_comp_h),
                FfiResult::OK
            );
        }
        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    // Direct `AlignedBuf` bad alignment & null src checks.
    // SAFETY: Intentional invalid align / null src rejected before allocation or copy.
    unsafe {
        assert!(AlignedBuf::copy_from(null(), 4, 3).is_err());
        assert!(AlignedBuf::copy_from(null(), 4, 4).is_err());
        let zst = AlignedBuf::copy_from(null(), 0, 1).unwrap();
        assert!(!zst.as_ptr().is_null());
    }

    let live_e = world.spawn_empty().id();
    let live_ffi = FfiEntity::from_bits(live_e.to_bits());
    let dead_e = world.spawn_empty().id();
    assert!(world.despawn(dead_e));
    let dead_ffi = FfiEntity::from_bits(dead_e.to_bits());
    let mut bad_bits_ffi = FfiEntity::from_bits(1u64 << 32);
    let mut placeholder_ffi = FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits());
    let mut unaligned_buf = [0u64; 2];
    let unaligned_ent = (&raw mut unaligned_buf)
        .cast::<u8>()
        .wrapping_add(1)
        .cast::<FfiEntity>();

    let snap = reg.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(10)), Some(Arc::clone(&active)))
            .with_registry(&reg);
        let cp = cmds.as_ffi_ptr();
        let bad_cmds = (&raw const snap).cast_mut().cast::<BamCommands>();
        let mut val4 = 123u32;
        let val4_ptr = (&raw mut val4).cast::<u8>();

        // SAFETY: Valid `cmds` and stack buffers; null/invalid/misaligned arguments test FFI guards.
        unsafe {
            // `host_cmd_observe` guards
            assert_eq!(
                host_cmd_observe(
                    cp,
                    &BamObserverDesc::new(dyn_evt_h, obs_noop_cb, null_mut()),
                    unaligned_ent
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_cmd_observe(
                    null_mut(),
                    &BamObserverDesc::new(dyn_evt_h, obs_noop_cb, null_mut()),
                    null_mut()
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_observe(
                    bad_cmds,
                    &BamObserverDesc::new(dyn_evt_h, obs_noop_cb, null_mut()),
                    null_mut()
                ),
                FfiResult::INVALID_STATE
            );
            assert_eq!(
                host_cmd_observe(cp, null(), null_mut()),
                FfiResult::NULL_POINTER
            );
            let mut short_obs = BamObserverDesc::new(dyn_evt_h, obs_noop_cb, null_mut());
            short_obs.struct_size = 4;
            assert_eq!(
                host_cmd_observe(cp, &short_obs, null_mut()),
                FfiResult::INVALID_VERSION
            );
            let bad_obs_flags =
                BamObserverDesc::new(dyn_evt_h, obs_noop_cb, null_mut()).with_flags(1 << 30);
            assert_eq!(
                host_cmd_observe(cp, &bad_obs_flags, null_mut()),
                FfiResult::UNSUPPORTED
            );
            let bad_obs_evt =
                BamObserverDesc::new(BamComponent::from_raw(999), obs_noop_cb, null_mut());
            assert_eq!(
                host_cmd_observe(cp, &bad_obs_evt, null_mut()),
                FfiResult::INVALID_COMPONENT
            );

            // `host_cmd_trigger_ex` guards
            let ok_trig = BamTriggerDesc::new(dyn_evt_h, val4_ptr, 4);
            assert_eq!(
                host_cmd_trigger_ex(null_mut(), &ok_trig),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_cmd_trigger_ex(bad_cmds, &ok_trig),
                FfiResult::INVALID_STATE
            );
            assert_eq!(host_cmd_trigger_ex(cp, null()), FfiResult::NULL_POINTER);
            let mut short_trig = ok_trig;
            short_trig.struct_size = 4;
            assert_eq!(
                host_cmd_trigger_ex(cp, &short_trig),
                FfiResult::INVALID_VERSION
            );
            let bad_trig_flags = ok_trig.with_flags(1 << 30);
            assert_eq!(
                host_cmd_trigger_ex(cp, &bad_trig_flags),
                FfiResult::UNSUPPORTED
            );
            assert_eq!(
                host_cmd_trigger_ex(cp, &ok_trig.with_entity(bad_bits_ffi)),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_cmd_trigger_ex(cp, &ok_trig.with_entity(placeholder_ffi)),
                FfiResult::INVALID_ENTITY
            );

            // `propagate_relationship` guards
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &ok_trig
                        .with_entity(live_ffi)
                        .with_propagate(BamComponent::from_raw(999)),
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_trigger_ex(cp, &ok_trig.with_entity(live_ffi).with_propagate(res_h),),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &ok_trig.with_entity(live_ffi).with_propagate(bad_prop_u32_h),
                ),
                FfiResult::INVALID_COMPONENT
            );

            // `watched_components` guards
            let bad_w = [BamComponent::from_raw(999)];
            assert_eq!(
                host_cmd_trigger_ex(cp, &ok_trig.with_watched_components(bad_w.as_ptr(), 1),),
                FfiResult::INVALID_COMPONENT
            );
            let res_w = [res_h];
            assert_eq!(
                host_cmd_trigger_ex(cp, &ok_trig.with_watched_components(res_w.as_ptr(), 1),),
                FfiResult::INVALID_COMPONENT
            );

            // Custom event guards
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(BamComponent::from_raw(999), null_mut(), 0),
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_trigger_ex(cp, &BamTriggerDesc::new(drop_comp_h, val4_ptr, 4)),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_trigger_ex(cp, &BamTriggerDesc::new(dyn_evt_h, val4_ptr, 8)),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_cmd_trigger_ex(cp, &ok_trig.with_propagate(BamComponent::INVALID),),
                FfiResult::INVALID_ENTITY
            );

            // Entity event (`HostEntEvt`) missing target / `extract_event_target_fn` returning `None`
            let mut placeholder_evt = HostEntEvt {
                target: TargetEntity::from_entity(Entity::PLACEHOLDER),
            };
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(
                        ent_evt_h,
                        (&raw mut placeholder_evt).cast::<u8>(),
                        core::mem::size_of::<HostEntEvt>(),
                    ),
                ),
                FfiResult::INVALID_ENTITY
            );

            // Lifecycle event guards
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(comp_h, null_mut(), 0).with_event_kind(BamEventKind::ADD),
                ),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(comp_h, val4_ptr, 4)
                        .with_event_kind(BamEventKind::ADD)
                        .with_entity(live_ffi),
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(BamComponent::from_raw(999), null_mut(), 0)
                        .with_event_kind(BamEventKind::ADD)
                        .with_entity(live_ffi),
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(res_h, null_mut(), 0)
                        .with_event_kind(BamEventKind::ADD)
                        .with_entity(live_ffi),
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(BamComponent::INVALID, null_mut(), 0)
                        .with_event_kind(BamEventKind::ADD)
                        .with_entity(live_ffi),
                ),
                FfiResult::INVALID_COMPONENT
            );

            // `READY` guards (no `Ready` registered in `reg`!)
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(BamComponent::from_raw(999), null_mut(), 0)
                        .with_event_kind(BamEventKind::READY)
                        .with_entity(live_ffi),
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(BamComponent::INVALID, null_mut(), 0)
                        .with_event_kind(BamEventKind::READY)
                        .with_entity(live_ffi),
                ),
                FfiResult::NOT_FOUND
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(res_h, null_mut(), 0)
                        .with_event_kind(BamEventKind::READY)
                        .with_entity(live_ffi),
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(dyn_evt_h, (&raw mut bad_bits_ffi).cast::<u8>(), 8)
                        .with_event_kind(BamEventKind::READY),
                ),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(dyn_evt_h, (&raw mut placeholder_ffi).cast::<u8>(), 8)
                        .with_event_kind(BamEventKind::READY),
                ),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(dyn_evt_h, val4_ptr, 2)
                        .with_event_kind(BamEventKind::READY)
                        .with_entity(live_ffi),
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(dyn_evt_h, null_mut(), 0)
                        .with_event_kind(BamEventKind::READY),
                ),
                FfiResult::INVALID_ENTITY
            );

            // Dead targets skipped cleanly during flush + `ZstHostEvt` trigger + `HostEntEvt`
            // with `Entity::PLACEHOLDER` direct `world.trigger` (`extract_target_fn` -> `None`)
            assert_eq!(
                host_cmd_trigger_ex(cp, &ok_trig.with_entity(dead_ffi)),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(comp_h, null_mut(), 0)
                        .with_event_kind(BamEventKind::ADD)
                        .with_entity(dead_ffi),
                ),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(dyn_evt_h, val4_ptr, 4)
                        .with_event_kind(BamEventKind::READY)
                        .with_entity(dead_ffi),
                ),
                FfiResult::OK
            );
            // Propagating lifecycle event stopping cleanly at root `live_e`.
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(comp_h, null_mut(), 0)
                        .with_event_kind(BamEventKind::ADD)
                        .with_entity(live_ffi)
                        .with_propagate(BamComponent::INVALID),
                ),
                FfiResult::OK
            );
            // ZST custom event with `TriggerEventFn` (exercises `AlignedPayload::as_ptr`).
            assert_eq!(
                host_cmd_trigger_ex(cp, &BamTriggerDesc::new(zst_h, null_mut(), 0)),
                FfiResult::OK
            );
        }
    }

    // Pre-insert `DynamicTriggerState` so `DynamicTriggerGuard::drop` restores `previous` (`line 78`).
    world.insert_resource(DynamicTriggerState {
        current_target: None,
        propagate: true,
        watched_ids: Vec::new(),
    });
    storage.apply_to_world(&mut world, &snap);
    assert!(world.get_resource::<DynamicTriggerState>().is_some());
    world.remove_resource::<DynamicTriggerState>();

    // Observer entity without `ModObserverState` (`line 176`), `extract_target_fn -> None` (`209`),
    // and `binding.target_entity` fallback without `DynamicTriggerState` (`217`).
    let mut bare_obs = Observer::with_dynamic_runner(guest_observer_runner);
    // SAFETY: `zst_evt_key` matches `ZstHostEvt`.
    bare_obs = unsafe { bare_obs.with_event_key(zst_evt_key) };
    world.spawn(bare_obs);
    world.trigger(ZstHostEvt);

    let mut dyn_obs = Observer::with_dynamic_runner(guest_observer_runner);
    let dyn_evt_key = snap.get_by_handle(dyn_evt_h).unwrap().event_key.unwrap();
    // SAFETY: `dyn_evt_key` matches 4-byte dynamic `DynEvt`.
    dyn_obs = unsafe { dyn_obs.with_event_key(dyn_evt_key) };
    dyn_obs.watch_entity(live_e);
    world.spawn((
        dyn_obs,
        ModObserverState::new(GuestObserverBinding {
            name: Arc::from("watched_fallback"),
            event: dyn_evt_h,
            event_kind: BamEventKind::CUSTOM,
            event_id: Some(snap.get_by_handle(dyn_evt_h).unwrap().component_id),
            event_key: dyn_evt_key,
            event_size: 4,
            is_propagating: false,
            propagate_relationship: None,
            synthesizes_ffi_entity_payload: false,
            target_entity: Some(live_e),
            watched_components: Box::new([]),
            callback: Some(GuestFnPtr(obs_noop_cb)),
            mut_callback: None,
            ctx_callback: None,
            query_specs: Vec::new(),
            user_data: crate::sync_ptr::GuestUserData::new(null_mut()),
            is_active: Arc::clone(&active),
            registry: reg.clone(),
            mod_id: Some(ModId(10)),
            extract_target_fn: None,
        }),
    ));
    let mut trig = EntityTrigger;
    let mut payload_val: u32 = 42;
    // SAFETY: `dyn_evt_key` matches `u32` payload and `EntityTrigger`.
    unsafe {
        world.trigger_dynamic_targets(
            dyn_evt_key,
            live_e,
            PtrMut::from(&mut payload_val),
            PtrMut::from(&mut trig),
        );
    }

    let mut ent_obs = Observer::with_dynamic_runner(guest_observer_runner);
    // SAFETY: `ent_evt_key` matches `HostEntEvt`.
    ent_obs = unsafe { ent_obs.with_event_key(ent_evt_key) };
    world.spawn((
        ent_obs,
        ModObserverState::new(GuestObserverBinding {
            name: Arc::from("ent_placeholder_obs"),
            event: ent_evt_h,
            event_kind: BamEventKind::CUSTOM,
            event_id: Some(snap.get_by_handle(ent_evt_h).unwrap().component_id),
            event_key: ent_evt_key,
            event_size: core::mem::size_of::<HostEntEvt>(),
            is_propagating: false,
            propagate_relationship: None,
            synthesizes_ffi_entity_payload: false,
            target_entity: None,
            watched_components: Box::new([]),
            callback: Some(GuestFnPtr(obs_noop_cb)),
            mut_callback: None,
            ctx_callback: None,
            query_specs: Vec::new(),
            user_data: crate::sync_ptr::GuestUserData::new(null_mut()),
            is_active: Arc::clone(&active),
            registry: reg,
            mod_id: Some(ModId(10)),
            extract_target_fn: snap
                .get_by_handle(ent_evt_h)
                .unwrap()
                .extract_event_target_fn,
        }),
    ));
    world.trigger(HostEntEvt {
        target: TargetEntity::from_entity(Entity::PLACEHOLDER),
    });
}

#[test]
fn guest_observer_panic_and_error_route_to_fallback_handler_with_observer_context() {
    use crate::ffi::commands::host_cmd_spawn_empty;
    use crate::loader::LoadedMod;
    use bamabi_macros::ModComponent;
    use bamabi_sys::BamInstance;
    use bevy_ecs::error::{BevyError, ErrorContext, FallbackErrorHandler, Severity};
    use bevy_ecs::event::Event;
    use core::ptr::null_mut;
    use core::sync::atomic::AtomicUsize;
    use std::sync::Mutex;

    #[derive(Event, Clone, Copy, Debug, ModComponent)]
    #[mod_component(name = "obs_test::FaultEvt")]
    #[repr(C)]
    struct FaultEvt(u32);

    unsafe extern "C" fn mock_shutdown(_inst: *mut BamInstance) {}

    #[derive(Debug)]
    struct CapturedObserverFault {
        ctx_name: String,
        is_observer_ctx: bool,
        severity: Severity,
        rendered: String,
        mod_err: Option<ModSystemError>,
    }

    static CAPTURED: Mutex<Vec<CapturedObserverFault>> = Mutex::new(Vec::new());
    static TEST_CAPTURE_LOCK: Mutex<()> = Mutex::new(());

    fn capture_handler(err: BevyError, ctx: ErrorContext) {
        let (is_observer_ctx, ctx_name) = match &ctx {
            ErrorContext::Observer { name, .. } => (true, name.to_string()),
            other => (false, other.name().to_string()),
        };
        let rec = CapturedObserverFault {
            ctx_name,
            is_observer_ctx,
            severity: err.severity(),
            rendered: format!("{err}"),
            mod_err: err.downcast_ref::<ModSystemError>().cloned(),
        };
        CAPTURED.lock().unwrap_or_else(|e| e.into_inner()).push(rec);
    }

    unsafe extern "C" fn panicking_obs(
        _target: FfiEntity,
        _data: *const c_void,
        cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `AtomicUsize` living across triggers.
        unsafe {
            (*(user_data.cast::<AtomicUsize>())).fetch_add(1, Ordering::SeqCst);
        }
        // Queue a spawn that MUST be discarded because this observer faults with `PANIC_CAUGHT`.
        let mut spawned = FfiEntity::NULL;
        // SAFETY: `cmds` is active for this observer invocation and `spawned` is writable.
        let _ = unsafe { host_cmd_spawn_empty(cmds, &mut spawned) };
        FfiResult::PANIC_CAUGHT
    }

    unsafe extern "C" fn healthy_sibling_obs(
        _target: FfiEntity,
        _data: *const c_void,
        _cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `AtomicUsize` living across triggers.
        unsafe {
            (*(user_data.cast::<AtomicUsize>())).fetch_add(1, Ordering::SeqCst);
        }
        FfiResult::OK
    }

    unsafe extern "C" fn mut_error_obs(
        _target: FfiEntity,
        _data: *mut u8,
        _propagate: *mut FfiBool,
        _cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `AtomicUsize` living across triggers.
        unsafe {
            (*(user_data.cast::<AtomicUsize>())).fetch_add(1, Ordering::SeqCst);
        }
        FfiResult::INTERNAL_ERROR
    }

    let _serial = TEST_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    CAPTURED.lock().unwrap_or_else(|e| e.into_inner()).clear();

    let mut world = World::new();
    world.insert_resource(FallbackErrorHandler(capture_handler));
    world.init_resource::<Messages<ModSystemFaulted>>();
    let mut reg = ModRegistry::new();
    let evt_key = world.register_event_key::<FaultEvt>();
    let evt_h = reg.register_event::<FaultEvt>(evt_key).unwrap();
    world.insert_resource(reg.clone());

    let active = Arc::new(AtomicBool::new(true));
    let mock_mod_id = LoadedMod::register_mock_for_test(
        &mut world,
        "obs_fault_mod",
        Arc::clone(&active),
        mock_shutdown,
        None,
        null_mut(),
    );

    let mut panic_hits = AtomicUsize::new(0);
    let mut sibling_hits = AtomicUsize::new(0);

    let bindings = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg.clone(),
            Arc::clone(&active),
            &mut world as *mut World,
            Some(mock_mod_id),
        );
        let panic_desc =
            BamObserverDesc::new(evt_h, panicking_obs, (&raw mut panic_hits).cast::<c_void>())
                .with_name(c"faulting_panic_obs".as_ptr());
        let sibling_desc = BamObserverDesc::new(
            evt_h,
            healthy_sibling_obs,
            (&raw mut sibling_hits).cast::<c_void>(),
        )
        .with_name(c"healthy_sibling_obs".as_ptr());
        // SAFETY: Valid `LoadContext` and stack descriptors.
        unsafe {
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &panic_desc),
                FfiResult::OK
            );
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &sibling_desc),
                FfiResult::OK
            );
        }
        ctx.finish_init();
        core::mem::take(&mut ctx.registered_observers)
    };
    let _spawned = spawn_registered_observers_in_world(&mut world, bindings, &reg, mock_mod_id);
    let entities_before = world.entities().len();

    // First trigger: panicking observer faults (commands discarded + per-observer quarantine),
    // sibling observer still executes.
    world.trigger(FaultEvt(1));
    assert_eq!(panic_hits.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        world.entities().len(),
        entities_before,
        "uncommitted commands from panicking observer must be discarded"
    );

    {
        let cap = CAPTURED.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(cap.len(), 1);
        assert!(cap[0].is_observer_ctx);
        assert!(
            cap[0].ctx_name == "faulting_panic_obs" || cap[0].ctx_name.contains("debug feature"),
            "unexpected ErrorContext::Observer name: {}",
            cap[0].ctx_name
        );
        assert_eq!(cap[0].severity, Severity::Error);
        assert!(cap[0].rendered.contains("faulting_panic_obs"));
        assert!(cap[0].rendered.contains("obs_fault_mod"));
        let mod_err = cap[0].mod_err.as_ref().expect("downcast ModSystemError");
        assert_eq!(mod_err.mod_id, Some(mock_mod_id));
        assert_eq!(mod_err.mod_namespace.as_deref(), Some("obs_fault_mod"));
        assert_eq!(mod_err.system_name, "faulting_panic_obs");
        assert_eq!(mod_err.status, FfiResult::PANIC_CAUGHT);
        assert!(mod_err.panicked);
    }

    let faults: Vec<ModSystemFaulted> = world
        .resource::<Messages<ModSystemFaulted>>()
        .iter_current_update_messages()
        .cloned()
        .collect();
    assert_eq!(faults.len(), 1);
    assert_eq!(faults[0].system_name, "faulting_panic_obs");
    assert_eq!(faults[0].status, FfiResult::PANIC_CAUGHT);
    assert!(faults[0].panicked);
    assert_eq!(faults[0].mod_id, Some(mock_mod_id));
    assert_eq!(
        world
            .resource::<ModHost>()
            .get_mod(mock_mod_id)
            .unwrap()
            .fault_count,
        1
    );

    // Second trigger: quarantined panicking observer does NOT run again, while sibling stays alive.
    world.trigger(FaultEvt(2));
    assert_eq!(panic_hits.load(Ordering::SeqCst), 1);
    assert_eq!(sibling_hits.load(Ordering::SeqCst), 2);
    assert_eq!(CAPTURED.lock().unwrap_or_else(|e| e.into_inner()).len(), 1);

    // DelegateToBevyFallback policy: non-panic mutable observer error routes repeatedly without quarantine.
    CAPTURED.lock().unwrap_or_else(|e| e.into_inner()).clear();
    world.insert_resource(ModErrorPolicyConfig(ModErrorPolicy::DelegateToBevyFallback));
    let mut mut_hits = AtomicUsize::new(0);
    let mut_bindings = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg.clone(),
            Arc::clone(&active),
            &mut world as *mut World,
            Some(mock_mod_id),
        );
        let desc =
            BamObserverDesc::new_mut(evt_h, mut_error_obs, (&raw mut mut_hits).cast::<c_void>())
                .with_name(c"mut_err_obs".as_ptr());
        // SAFETY: Valid `LoadContext` and stack descriptor.
        unsafe {
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &desc),
                FfiResult::OK
            );
        }
        ctx.finish_init();
        core::mem::take(&mut ctx.registered_observers)
    };
    let _mut_spawned =
        spawn_registered_observers_in_world(&mut world, mut_bindings, &reg, mock_mod_id);

    world.trigger(FaultEvt(3));
    world.trigger(FaultEvt(4));
    assert_eq!(mut_hits.load(Ordering::SeqCst), 2);
    let cap = CAPTURED.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(cap.len(), 2);
    for rec in cap.iter() {
        assert!(rec.is_observer_ctx);
        assert!(
            rec.ctx_name == "mut_err_obs" || rec.ctx_name.contains("debug feature"),
            "unexpected ErrorContext::Observer name: {}",
            rec.ctx_name
        );
        assert_eq!(rec.severity, Severity::Error);
        let me = rec.mod_err.as_ref().expect("downcast ModSystemError");
        assert_eq!(me.status, FfiResult::INTERNAL_ERROR);
        assert!(!me.panicked);
    }
    assert_eq!(
        world
            .resource::<ModHost>()
            .get_mod(mock_mod_id)
            .unwrap()
            .fault_count,
        3
    );
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Default, bamabi_macros::ModComponent)]
#[mod_component(name = "obs_v03_unit::TagA")]
struct ObsTagA;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Default, bamabi_macros::ModComponent)]
#[mod_component(name = "obs_v03_unit::TagB")]
struct ObsTagB;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Default, bamabi_macros::ModComponent)]
#[mod_component(name = "obs_v03_unit::Score")]
struct ObsScore {
    value: i32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq, Default, bamabi_macros::ModComponent)]
#[mod_component(name = "obs_v03_unit::Delta")]
struct ObsDelta {
    step: i32,
}

#[repr(C)]
#[derive(
    bevy_ecs::prelude::Resource,
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Default,
    bamabi_macros::ModComponent,
)]
#[mod_component(name = "obs_v03_unit::TelemetryRes")]
struct ObsTelemetryRes {
    hits: u32,
    chained_hits: u32,
    total_score: i32,
    random_before: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChainedEvtPayload {
    bonus: i32,
}

#[test]
fn slot37_cmd_trigger_and_custom_watched_components_fire_once_without_double_dispatch() {
    use crate::ffi::commands::host_cmd_trigger;
    use core::sync::atomic::AtomicUsize;

    struct CustomObsState {
        direct_hits: Mutex<Vec<(FfiEntity, u32)>>,
        watched_hits: AtomicUsize,
    }

    unsafe extern "C" fn record_direct(
        entity: FfiEntity,
        data: *const c_void,
        _cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `data` points to initialized `u32` payload for `DynEvt`.
        let payload = unsafe { core::ptr::read_unaligned(data.cast::<u32>()) };
        // SAFETY: `user_data` points to live `CustomObsState`.
        let st = unsafe { &*user_data.cast::<CustomObsState>() };
        st.direct_hits.lock().unwrap().push((entity, payload));
        FfiResult::OK
    }

    unsafe extern "C" fn record_watched(
        _entity: FfiEntity,
        _data: *const c_void,
        _cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to live `CustomObsState`.
        let st = unsafe { &*user_data.cast::<CustomObsState>() };
        st.watched_hits.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    }

    let mut world = World::new();
    let tag_a_cid = world.register_component::<ObsTagA>();
    let tag_b_cid = world.register_component::<ObsTagB>();
    let mut reg = ModRegistry::new();
    let tag_a_h = reg
        .register_component::<ObsTagA>(tag_a_cid, Default::default())
        .unwrap();
    let tag_b_h = reg
        .register_component::<ObsTagB>(tag_b_cid, Default::default())
        .unwrap();
    let active = Arc::new(AtomicBool::new(true));
    let state = Box::new(CustomObsState {
        direct_hits: Mutex::new(Vec::new()),
        watched_hits: AtomicUsize::new(0),
    });
    let state_ptr = core::ptr::from_ref(&*state).cast_mut().cast::<c_void>();

    let mut dyn_evt_h = BamComponent::INVALID;
    let (reg, bindings) = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(71)),
        );
        let d_evt = ComponentDesc::new(
            c"obs_v03_unit::Slot37DynEvt".as_ptr(),
            ComponentUuid::from_parts("obs_v03_unit::Slot37DynEvt", 4, 4, 0),
            4,
            4,
        );
        // SAFETY: Valid `LoadContext` and descriptors.
        unsafe {
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &d_evt, &mut dyn_evt_h),
                FfiResult::OK
            );
            let direct_desc = BamObserverDesc::new(dyn_evt_h, record_direct, state_ptr)
                .with_name(c"slot37_direct".as_ptr());
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &direct_desc),
                FfiResult::OK
            );
            let watched_desc = BamObserverDesc::new(dyn_evt_h, record_watched, state_ptr)
                .with_name(c"custom_watched_a".as_ptr())
                .with_watched_components(&tag_a_h, 1);
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &watched_desc),
                FfiResult::OK
            );
        }
        let r = ctx.registry.clone();
        ctx.finish_init();
        (r, core::mem::take(&mut ctx.registered_observers))
    };
    let _spawned = spawn_registered_observers_in_world(&mut world, bindings, &reg, ModId(71));

    let with_a = world.spawn(ObsTagA).id();
    let without_a = world.spawn(ObsTagB).id();
    let with_a_ffi = FfiEntity::from_bits(with_a.to_bits());
    let without_a_ffi = FfiEntity::from_bits(without_a.to_bits());
    let snap = reg.snapshot_arc();
    let mut storage = HostCommandsStorage::new();

    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(71)), Some(active))
            .with_registry(&reg);
        let cp = cmds.as_ffi_ptr();
        let p1: u32 = 11;
        let p2: u32 = 22;
        let p3: u32 = 33;
        let mut p4: u32 = 44;
        let mut p5: u32 = 55;
        // SAFETY: Valid `HostCommands`, payload pointers, and `BamTriggerDesc`s.
        unsafe {
            // 1. Slot-37 trigger on entity WITH TagA: direct sees `with_a_ffi`, watched fires once.
            assert_eq!(
                host_cmd_trigger(cp, dyn_evt_h, with_a_ffi, (&raw const p1).cast::<u8>(), 4),
                FfiResult::OK
            );
            // 2. Slot-37 trigger on entity WITHOUT TagA: direct sees `without_a_ffi`, watched skipped.
            assert_eq!(
                host_cmd_trigger(
                    cp,
                    dyn_evt_h,
                    without_a_ffi,
                    (&raw const p2).cast::<u8>(),
                    4
                ),
                FfiResult::OK
            );
            // 3. Global Slot-37 trigger (`FfiEntity::NULL`): direct sees `NULL`, watched skipped.
            assert_eq!(
                host_cmd_trigger(
                    cp,
                    dyn_evt_h,
                    FfiEntity::NULL,
                    (&raw const p3).cast::<u8>(),
                    4
                ),
                FfiResult::OK
            );
            // 4. `cmd_trigger_ex` with matching `watched_components = [TagA]`: watched fires ONCE (no double fire!).
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(dyn_evt_h, (&raw mut p4).cast::<u8>(), 4)
                        .with_entity(without_a_ffi)
                        .with_watched_components(&tag_a_h, 1),
                ),
                FfiResult::OK
            );
            // 5. `cmd_trigger_ex` with mismatched `watched_components = [TagB]`: watched skipped.
            assert_eq!(
                host_cmd_trigger_ex(
                    cp,
                    &BamTriggerDesc::new(dyn_evt_h, (&raw mut p5).cast::<u8>(), 4)
                        .with_entity(with_a_ffi)
                        .with_watched_components(&tag_b_h, 1),
                ),
                FfiResult::OK
            );
        }
    }

    storage.apply_to_world(&mut world, &snap);
    assert_eq!(
        *state.direct_hits.lock().unwrap(),
        vec![
            (with_a_ffi, 11),
            (without_a_ffi, 22),
            (FfiEntity::NULL, 33),
            (without_a_ffi, 44),
            (with_a_ffi, 55),
        ]
    );
    assert_eq!(state.watched_hits.load(Ordering::SeqCst), 2);
}

#[test]
fn ctx_observer_executes_queries_random_access_resources_commands_chained_triggers_and_runtime_observe()
 {
    use crate::ffi::commands::host_cmd_trigger;
    use crate::ffi::iter::{host_iter_column_readonly, host_iter_next_chunk};
    use crate::system::{
        host_sys_commands, host_sys_entity_get, host_sys_entity_get_mut, host_sys_query,
        host_sys_res_mut_ptr, host_sys_res_ptr,
    };
    use bamabi_sys::{AccessMode, BamChangeTicks, BamQueryDesc, BamSystemCtx, BamTermDesc};
    use core::ptr::null_mut;

    struct CtxChainState {
        chained_evt_h: BamComponent,
        score_h: BamComponent,
        res_h: BamComponent,
        runtime_hits: core::sync::atomic::AtomicUsize,
    }

    unsafe extern "C" fn primary_ctx_obs(
        entity: FfiEntity,
        data: *const c_void,
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to live `CtxChainState` and `data` to `i32`.
        let st = unsafe { &*user_data.cast::<CtxChainState>() };
        // SAFETY: `data` points to initialized `i32` event payload.
        let inc = unsafe { core::ptr::read_unaligned(data.cast::<i32>()) };

        // Read via `host_sys_entity_get`, then mutate via `host_sys_entity_get_mut`.
        // SAFETY: `sys_ctx` is active for this observer invocation.
        let prev_ptr = unsafe { host_sys_entity_get(sys_ctx, 0, entity, 0) };
        assert!(!prev_ptr.is_null());
        // SAFETY: `prev_ptr` points to readable `ObsScore` component bytes.
        let prev_score = unsafe { core::ptr::read_unaligned(prev_ptr.cast::<ObsScore>()) }.value;

        // SAFETY: `sys_ctx` is active with declared WRITE access on `ObsScore`.
        let mut_ptr = unsafe { host_sys_entity_get_mut(sys_ctx, 0, entity, 0) };
        assert!(!mut_ptr.is_null());
        // SAFETY: `mut_ptr` points to writable `ObsScore` component storage.
        unsafe {
            core::ptr::write_unaligned(
                mut_ptr.cast::<ObsScore>(),
                ObsScore {
                    value: prev_score + inc,
                },
            );
        }

        // Read and mutate resource via `host_sys_res_ptr` and `host_sys_res_mut_ptr`.
        let mut ticks = BamChangeTicks::default();
        // SAFETY: `sys_ctx` is active and `ticks` is stack-allocated.
        let res_ro = unsafe { host_sys_res_ptr(sys_ctx, st.res_h, &mut ticks) };
        assert!(!res_ro.is_null());
        // SAFETY: `sys_ctx` is active with declared `RESOURCE_WRITE` access.
        let res_mut = unsafe { host_sys_res_mut_ptr(sys_ctx, st.res_h, null_mut()) };
        assert!(!res_mut.is_null());
        // SAFETY: `res_mut` points to `ObsTelemetryRes` exclusive resource storage.
        let mut telem = unsafe { core::ptr::read_unaligned(res_mut.cast::<ObsTelemetryRes>()) };
        telem.hits += 1;
        telem.random_before = prev_score;
        // SAFETY: `res_mut` is exclusively writable for `ObsTelemetryRes`.
        unsafe { core::ptr::write_unaligned(res_mut.cast::<ObsTelemetryRes>(), telem) };

        // Queue chained trigger via `host_sys_commands` (`obs1 -> Ev2 -> obs2`).
        let mut cmds_ptr: *mut BamCommands = null_mut();
        // SAFETY: `sys_ctx` is active and `cmds_ptr` is stack-writable.
        let rc_cmds = unsafe { host_sys_commands(sys_ctx, &mut cmds_ptr) };
        assert_eq!(rc_cmds, FfiResult::OK);
        let chained = ChainedEvtPayload { bonus: inc * 2 };
        // SAFETY: `cmds_ptr` is active and `chained` is valid for `ChainedEvtPayload`.
        unsafe {
            host_cmd_trigger(
                cmds_ptr,
                st.chained_evt_h,
                FfiEntity::NULL,
                core::ptr::from_ref(&chained).cast::<u8>(),
                core::mem::size_of::<ChainedEvtPayload>(),
            )
        }
    }

    unsafe extern "C" fn chained_ctx_obs(
        entity: FfiEntity,
        data: *const c_void,
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        assert_eq!(entity, FfiEntity::NULL);
        // SAFETY: `user_data` points to `CtxChainState`.
        let st = unsafe { &*user_data.cast::<CtxChainState>() };
        // SAFETY: `data` points to initialized `ChainedEvtPayload`.
        let payload = unsafe { core::ptr::read_unaligned(data.cast::<ChainedEvtPayload>()) };

        let mut iter_ptr: *mut BamIter = null_mut();
        // SAFETY: `sys_ctx` is active with declared query slot 0.
        let rc_q = unsafe { host_sys_query(sys_ctx, 0, &mut iter_ptr) };
        assert_eq!(rc_q, FfiResult::OK);
        let mut sum = 0i32;
        let mut chunk_len = 0usize;
        // SAFETY: Valid `BamIter` returned by `host_sys_query`.
        while unsafe { host_iter_next_chunk(iter_ptr, &mut chunk_len) }.get() {
            // SAFETY: `iter_ptr` is active at current chunk with term 0 `ObsScore`.
            let raw_col = unsafe { host_iter_column_readonly(iter_ptr, 0) };
            if !raw_col.is_null() {
                for idx in 0..chunk_len {
                    // SAFETY: `raw_col` holds `chunk_len` contiguous `ObsScore` values.
                    let elem = unsafe {
                        raw_col
                            .add(idx * core::mem::size_of::<ObsScore>())
                            .cast::<ObsScore>()
                            .read_unaligned()
                    };
                    sum += elem.value;
                }
            }
        }
        // SAFETY: `sys_ctx` is active with declared `RESOURCE_WRITE` access.
        let res_mut = unsafe { host_sys_res_mut_ptr(sys_ctx, st.res_h, null_mut()) };
        assert!(!res_mut.is_null());
        // SAFETY: Valid `ObsTelemetryRes` pointer.
        let mut telem = unsafe { core::ptr::read_unaligned(res_mut.cast::<ObsTelemetryRes>()) };
        telem.chained_hits += 1;
        telem.total_score = sum + payload.bonus;
        // SAFETY: `res_mut` is exclusively writable for `ObsTelemetryRes`.
        unsafe { core::ptr::write_unaligned(res_mut.cast::<ObsTelemetryRes>(), telem) };
        FfiResult::OK
    }

    unsafe extern "C" fn runtime_ctx_obs(
        _entity: FfiEntity,
        _data: *const c_void,
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `CtxChainState`.
        let st = unsafe { &*user_data.cast::<CtxChainState>() };
        let mut iter_ptr: *mut BamIter = null_mut();
        // SAFETY: `sys_ctx` is active with declared query slot 0.
        let rc_q = unsafe { host_sys_query(sys_ctx, 0, &mut iter_ptr) };
        assert_eq!(rc_q, FfiResult::OK);
        let mut count = 0usize;
        let mut chunk_len = 0usize;
        // SAFETY: Valid `BamIter`.
        while unsafe { host_iter_next_chunk(iter_ptr, &mut chunk_len) }.get() {
            count += chunk_len;
        }
        assert!(st.score_h.is_valid());
        st.runtime_hits.fetch_add(count, Ordering::SeqCst);
        FfiResult::OK
    }

    let mut world = World::new();
    let score_cid = world.register_component::<ObsScore>();
    let delta_cid = world.register_component::<ObsDelta>();
    let res_cid = world.register_component::<ObsTelemetryRes>();
    world.insert_resource(ObsTelemetryRes::default());

    let mut reg = ModRegistry::new();
    let score_h = reg
        .register_component::<ObsScore>(score_cid, Default::default())
        .unwrap();
    let delta_h = reg
        .register_component::<ObsDelta>(delta_cid, Default::default())
        .unwrap();
    let res_h = reg.register_resource::<ObsTelemetryRes>(res_cid).unwrap();
    world.insert_resource(reg.clone());

    let active = Arc::new(AtomicBool::new(true));
    let mut state = Box::new(CtxChainState {
        chained_evt_h: BamComponent::INVALID,
        score_h,
        res_h,
        runtime_hits: core::sync::atomic::AtomicUsize::new(0),
    });

    let mut primary_evt_h = BamComponent::INVALID;
    let mut runtime_evt_h = BamComponent::INVALID;
    let (reg, bindings) = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::clone(&active),
            &mut world as *mut World,
            Some(ModId(72)),
        );
        let d_primary = ComponentDesc::new(
            c"obs_v03_unit::PrimaryCtxEvt".as_ptr(),
            ComponentUuid::from_parts("obs_v03_unit::PrimaryCtxEvt", 4, 4, 0),
            4,
            4,
        );
        let d_chained = ComponentDesc::new(
            c"obs_v03_unit::ChainedCtxEvt".as_ptr(),
            ComponentUuid::from_parts("obs_v03_unit::ChainedCtxEvt", 4, 4, 0),
            4,
            4,
        );
        let d_runtime = ComponentDesc::new(
            c"obs_v03_unit::RuntimeCtxEvt".as_ptr(),
            ComponentUuid::from_parts("obs_v03_unit::RuntimeCtxEvt", 4, 4, 0),
            4,
            4,
        );
        // SAFETY: Valid descriptors and stack pointers.
        unsafe {
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &d_primary, &mut primary_evt_h),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &d_chained, &mut state.chained_evt_h),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &d_runtime, &mut runtime_evt_h),
                FfiResult::OK
            );

            let state_ptr = core::ptr::from_ref(&*state).cast_mut().cast::<c_void>();
            let p_q0 = [BamTermDesc::new(score_h, AccessMode::WRITE)];
            let p_q1 = [BamTermDesc::new(res_h, AccessMode::RESOURCE_WRITE)];
            let primary_queries = [
                BamQueryDesc::new(p_q0.as_ptr(), p_q0.len()),
                BamQueryDesc::new(p_q1.as_ptr(), p_q1.len()),
            ];
            let primary_desc = BamObserverDesc::new_with_ctx(
                primary_evt_h,
                primary_ctx_obs,
                primary_queries.as_ptr(),
                primary_queries.len(),
                state_ptr,
            )
            .with_name(c"primary_ctx_obs".as_ptr());
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &primary_desc),
                FfiResult::OK
            );

            let c_q0 = [BamTermDesc::new(score_h, AccessMode::READ)];
            let c_q1 = [BamTermDesc::new(res_h, AccessMode::RESOURCE_WRITE)];
            let chained_queries = [
                BamQueryDesc::new(c_q0.as_ptr(), c_q0.len()),
                BamQueryDesc::new(c_q1.as_ptr(), c_q1.len()),
            ];
            let chained_desc = BamObserverDesc::new_with_ctx(
                state.chained_evt_h,
                chained_ctx_obs,
                chained_queries.as_ptr(),
                chained_queries.len(),
                state_ptr,
            )
            .with_name(c"chained_ctx_obs".as_ptr());
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &chained_desc),
                FfiResult::OK
            );
        }
        let r = ctx.registry.clone();
        ctx.finish_init();
        (r, core::mem::take(&mut ctx.registered_observers))
    };
    world.insert_resource(reg.clone());
    let _spawned = spawn_registered_observers_in_world(&mut world, bindings, &reg, ModId(72));

    // Spawn entities AFTER observer init so `group.update_archetypes(cell)` is exercised.
    let target_ent = world
        .spawn((ObsScore { value: 10 }, ObsDelta { step: 1 }))
        .id();
    let _other_ent = world.spawn(ObsScore { value: 50 }).id();
    let target_ffi = FfiEntity::from_bits(target_ent.to_bits());
    let snap = reg.snapshot_arc();
    let mut storage = HostCommandsStorage::new();

    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(72)), Some(Arc::clone(&active)))
            .with_registry(&reg);
        let inc: i32 = 7;
        // SAFETY: Valid `HostCommands` and payload pointer.
        let trig_rc = unsafe {
            host_cmd_trigger(
                cmds.as_ffi_ptr(),
                primary_evt_h,
                target_ffi,
                core::ptr::from_ref(&inc).cast::<u8>(),
                4,
            )
        };
        assert_eq!(trig_rc, FfiResult::OK);

        // Also register a runtime ctx observer via `host_cmd_observe`.
        let state_ptr = core::ptr::from_ref(&*state).cast_mut().cast::<c_void>();
        let rt_terms = [
            BamTermDesc::new(score_h, AccessMode::READ),
            BamTermDesc::new(delta_h, AccessMode::FILTER_ONLY),
        ];
        let rt_queries = [BamQueryDesc::new(rt_terms.as_ptr(), rt_terms.len())];
        let rt_desc = BamObserverDesc::new_with_ctx(
            runtime_evt_h,
            runtime_ctx_obs,
            rt_queries.as_ptr(),
            rt_queries.len(),
            state_ptr,
        )
        .with_name(c"runtime_ctx_obs".as_ptr());
        let mut rt_obs_ent = FfiEntity::NULL;
        // SAFETY: Valid `HostCommands`, `BamObserverDesc`, and `out_entity`.
        let obs_rc = unsafe { host_cmd_observe(cmds.as_ffi_ptr(), &rt_desc, &mut rt_obs_ent) };
        assert_eq!(obs_rc, FfiResult::OK);
        assert_ne!(rt_obs_ent, FfiEntity::NULL);
    }

    // One synchronous `apply_to_world` must run `primary_ctx_obs -> cmd_trigger(ChainedCtxEvt) -> chained_ctx_obs`.
    storage.apply_to_world(&mut world, &snap);

    assert_eq!(world.get::<ObsScore>(target_ent).unwrap().value, 17);
    let telem = *world.resource::<ObsTelemetryRes>();
    assert_eq!(
        telem,
        ObsTelemetryRes {
            hits: 1,
            chained_hits: 1,
            total_score: 17 + 50 + 14,
            random_before: 10,
        }
    );

    // Trigger runtime-spawned `runtime_ctx_obs` (`query: Score with FILTER_ONLY Delta` -> matches 1 entity).
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
            .with_mod_context(Some(ModId(72)), Some(active))
            .with_registry(&reg);
        let dummy: u32 = 1;
        // SAFETY: Valid `HostCommands` and payload pointer.
        let rt_rc = unsafe {
            host_cmd_trigger(
                cmds.as_ffi_ptr(),
                runtime_evt_h,
                FfiEntity::NULL,
                core::ptr::from_ref(&dummy).cast::<u8>(),
                4,
            )
        };
        assert_eq!(rt_rc, FfiResult::OK);
    }
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(state.runtime_hits.load(Ordering::SeqCst), 1);
}

#[test]
fn ctx_observer_validation_quarantine_reentrancy_rollback_and_truncated_compat() {
    use crate::ffi::commands::host_cmd_spawn_empty;
    use crate::ffi::v03_ext::host_report_guest_fault;
    use crate::loader::LoadedMod;
    use crate::system::{host_sys_commands, host_sys_host_ctx};
    use bamabi_sys::{
        AccessMode, BamInstance, BamQueryDesc, BamSystemCtx, BamTermDesc, MAX_QUERIES_PER_SYSTEM,
        StrView,
    };
    use bevy_ecs::error::{BevyError, ErrorContext, FallbackErrorHandler, Severity};
    use bevy_ecs::event::Event;
    use core::mem::{offset_of, size_of};
    use core::ptr::{null, null_mut};
    use core::sync::atomic::AtomicUsize;

    #[derive(Event, Clone, Copy, Debug, bamabi_macros::ModComponent)]
    #[mod_component(name = "obs_v03_unit::FaultEvt")]
    #[repr(C)]
    struct FaultEvt(u32);

    unsafe extern "C" fn mock_shutdown(_inst: *mut BamInstance) {}

    #[derive(Debug)]
    struct CapturedFault {
        is_observer_ctx: bool,
        severity: Severity,
    }

    static CAPTURED: Mutex<Vec<CapturedFault>> = Mutex::new(Vec::new());
    static TEST_CAPTURE_LOCK: Mutex<()> = Mutex::new(());

    fn capture_handler(err: BevyError, ctx: ErrorContext) {
        let is_observer_ctx = matches!(ctx, ErrorContext::Observer { .. });
        CAPTURED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(CapturedFault {
                is_observer_ctx,
                severity: err.severity(),
            });
    }

    unsafe extern "C" fn dummy_ctx_cb(
        _e: FfiEntity,
        _d: *const c_void,
        _sys: *mut BamSystemCtx,
        _ud: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    unsafe extern "C" fn faulting_ctx_obs(
        _e: FfiEntity,
        _d: *const c_void,
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `AtomicUsize`.
        unsafe {
            (*(user_data.cast::<AtomicUsize>())).fetch_add(1, Ordering::SeqCst);
        }
        let mut cmds_ptr: *mut BamCommands = null_mut();
        // SAFETY: `sys_ctx` is active and `cmds_ptr` is stack-writable.
        let rc_cmds = unsafe { host_sys_commands(sys_ctx, &mut cmds_ptr) };
        assert_eq!(rc_cmds, FfiResult::OK);
        let mut spawned = FfiEntity::NULL;
        // SAFETY: `cmds_ptr` is active and `spawned` is stack-writable.
        let rc_spawn = unsafe { host_cmd_spawn_empty(cmds_ptr, &mut spawned) };
        assert_eq!(rc_spawn, FfiResult::OK);
        // SAFETY: `sys_ctx` is active for this invocation.
        let host_ctx = unsafe { host_sys_host_ctx(sys_ctx) };
        // SAFETY: `host_ctx` is valid and `StrView` points to static UTF-8 bytes.
        unsafe {
            host_report_guest_fault(host_ctx, StrView::from_str("ctx observer explicit fault"))
        };
        FfiResult::INTERNAL_ERROR
    }

    let _serial = TEST_CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    CAPTURED.lock().unwrap_or_else(|e| e.into_inner()).clear();

    let mut world = World::new();
    world.insert_resource(FallbackErrorHandler(capture_handler));
    world.init_resource::<Messages<ModSystemFaulted>>();
    let score_cid = world.register_component::<ObsScore>();
    let mut reg = ModRegistry::new();
    let score_h = reg
        .register_component::<ObsScore>(score_cid, Default::default())
        .unwrap();
    let evt_key = world.register_event_key::<FaultEvt>();
    let evt_h = reg.register_event::<FaultEvt>(evt_key).unwrap();
    world.insert_resource(reg.clone());

    let active = Arc::new(AtomicBool::new(true));
    let mock_mod_id = LoadedMod::register_mock_for_test(
        &mut world,
        "obs_ctx_guard_mod",
        Arc::clone(&active),
        mock_shutdown,
        None,
        null_mut(),
    );

    // 1. Descriptor validation guards + backward-compatible truncated v0.2 & pre-ctx descriptors.
    {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg.clone(),
            Arc::clone(&active),
            &mut world as *mut World,
            Some(mock_mod_id),
        );
        let terms = [BamTermDesc::new(score_h, AccessMode::READ)];
        let one_q = [BamQueryDesc::new(terms.as_ptr(), terms.len())];
        let mut too_many = Vec::with_capacity(MAX_QUERIES_PER_SYSTEM + 1);
        for _ in 0..=MAX_QUERIES_PER_SYSTEM {
            too_many.push(BamQueryDesc::new(terms.as_ptr(), terms.len()));
        }

        // `query_count > MAX_QUERIES_PER_SYSTEM` -> CAPACITY_EXCEEDED
        let over_cap = BamObserverDesc::new_with_ctx(
            evt_h,
            dummy_ctx_cb,
            too_many.as_ptr(),
            too_many.len(),
            null_mut(),
        );
        // `query_count > 0` with null `queries` -> NULL_POINTER
        let null_q = BamObserverDesc::new_with_ctx(evt_h, dummy_ctx_cb, null(), 1, null_mut());
        // `queries` with `callback` (no `ctx_callback`) -> INVALID_ARGUMENT
        let mut q_no_ctx = BamObserverDesc::new(evt_h, obs_noop_cb, null_mut());
        q_no_ctx.queries = one_q.as_ptr();
        q_no_ctx.query_count = 1;
        // All callbacks null -> NULL_POINTER
        let mut all_null_cb = BamObserverDesc::new(evt_h, obs_noop_cb, null_mut());
        all_null_cb.callback = None;

        // SAFETY: Valid `LoadContext` and descriptors.
        unsafe {
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &over_cap),
                FfiResult::CAPACITY_EXCEEDED
            );
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &null_q),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &q_no_ctx),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &all_null_cb),
                FfiResult::NULL_POINTER
            );
        }

        // Backward-compat truncated v0.2 (`MIN_V0_2_SIZE`) and pre-ctx (`offset_of!(ctx_callback)`).
        struct AlignedRaw([u128; size_of::<BamObserverDesc>().div_ceil(16)]);

        for prefix_len in [
            BamObserverDesc::MIN_V0_2_SIZE,
            offset_of!(BamObserverDesc, ctx_callback),
        ] {
            let mut full = BamObserverDesc::new(evt_h, obs_noop_cb, null_mut());
            full.struct_size = prefix_len as u32;
            let mut raw = AlignedRaw([0u128; size_of::<BamObserverDesc>().div_ceil(16)]);
            // SAFETY: `prefix_len <= size_of::<BamObserverDesc>()` and `raw.0` is 16-byte aligned.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    core::ptr::from_ref(&full).cast::<u8>(),
                    raw.0.as_mut_ptr().cast::<u8>(),
                    prefix_len,
                );
                assert_eq!(
                    host_register_observer(
                        ctx.as_ffi_ptr(),
                        raw.0.as_ptr().cast::<BamObserverDesc>(),
                    ),
                    FfiResult::OK
                );
            }
        }
        ctx.finish_init();
    }

    // 2. Conflicting queries rejected at registration AND quarantined cleanly if encountered at init.
    let conflict_binding = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg.clone(),
            Arc::clone(&active),
            &mut world as *mut World,
            Some(mock_mod_id),
        );
        let w_term = [BamTermDesc::new(score_h, AccessMode::WRITE)];
        let dup_writes = [
            BamQueryDesc::new(w_term.as_ptr(), 1),
            BamQueryDesc::new(w_term.as_ptr(), 1),
        ];
        let desc = BamObserverDesc::new_with_ctx(
            evt_h,
            dummy_ctx_cb,
            dup_writes.as_ptr(),
            dup_writes.len(),
            null_mut(),
        )
        .with_name(c"self_conflict_ctx_obs".as_ptr());
        // SAFETY: Valid `LoadContext` and `BamObserverDesc`.
        unsafe {
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &desc),
                FfiResult::ACCESS_CONFLICT
            );
        }
        let one_q = [BamQueryDesc::new(w_term.as_ptr(), 1)];
        let single_desc =
            BamObserverDesc::new_with_ctx(evt_h, dummy_ctx_cb, one_q.as_ptr(), 1, null_mut())
                .with_name(c"self_conflict_init_obs".as_ptr());
        // SAFETY: Valid stack descriptor.
        let parsed = unsafe {
            bamabi_sys::read_descriptor::<BamObserverDesc>(
                core::ptr::from_ref(&single_desc).cast::<c_void>(),
            )
        }
        .unwrap();
        // SAFETY: `parsed` passed `read_descriptor`.
        let mut binding = unsafe {
            build_observer_binding_from_desc(
                &parsed,
                &reg.snapshot_arc(),
                reg.clone(),
                Arc::clone(&active),
                Some(mock_mod_id),
            )
        }
        .unwrap();
        let dup_spec = binding.query_specs[0].clone();
        binding.query_specs.push(dup_spec);
        ctx.finish_init();
        binding
    };
    let conflict_spawned =
        spawn_registered_observers_in_world(&mut world, vec![conflict_binding], &reg, mock_mod_id);
    assert_eq!(conflict_spawned.len(), 1);
    let conflict_state = world.get::<ModObserverState>(conflict_spawned[0]).unwrap();
    assert!(!conflict_state.enabled.load(Ordering::Acquire));
    assert!(conflict_state.ctx_state.is_none());
    let init_faults: Vec<ModSystemFaulted> = world
        .resource::<Messages<ModSystemFaulted>>()
        .iter_current_update_messages()
        .cloned()
        .collect();
    assert!(init_faults.iter().any(|f| {
        f.system_name == "self_conflict_init_obs"
            && f.init_error.is_some()
            && f.mod_id == Some(mock_mod_id)
    }));

    // 3. Re-entrancy `ACCESS_CONFLICT` guard when `ctx_state` mutex is held + runtime fault rollback.
    let mut hits = AtomicUsize::new(0);
    let mut_fault_bindings = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg.clone(),
            Arc::clone(&active),
            &mut world as *mut World,
            Some(mock_mod_id),
        );
        let r_term = [BamTermDesc::new(score_h, AccessMode::READ)];
        let r_q = [BamQueryDesc::new(r_term.as_ptr(), 1)];
        let d_reent =
            BamObserverDesc::new_with_ctx(evt_h, dummy_ctx_cb, r_q.as_ptr(), 1, null_mut())
                .with_name(c"reentrant_ctx_obs".as_ptr());
        let d_fault = BamObserverDesc::new_with_ctx(
            evt_h,
            faulting_ctx_obs,
            r_q.as_ptr(),
            1,
            core::ptr::from_ref(&hits).cast_mut().cast::<c_void>(),
        )
        .with_name(c"faulting_ctx_obs".as_ptr());
        // SAFETY: Valid `LoadContext` and stack descriptors.
        unsafe {
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &d_reent),
                FfiResult::OK
            );
            assert_eq!(
                host_register_observer(ctx.as_ffi_ptr(), &d_fault),
                FfiResult::OK
            );
        }
        ctx.finish_init();
        core::mem::take(&mut ctx.registered_observers)
    };
    let spawned =
        spawn_registered_observers_in_world(&mut world, mut_fault_bindings, &reg, mock_mod_id);
    assert_eq!(spawned.len(), 2);

    let reent_mutex = world
        .get::<ModObserverState>(spawned[0])
        .unwrap()
        .ctx_state
        .clone()
        .unwrap();
    let entities_before = world.entities().len();
    {
        let _lock = reent_mutex.lock().unwrap();
        world.trigger(FaultEvt(99));
    }
    // Faulting observer's `host_cmd_spawn_empty` was rolled back and both faulted observers quarantined.
    assert_eq!(world.entities().len(), entities_before);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert!(
        !world
            .get::<ModObserverState>(spawned[0])
            .unwrap()
            .enabled
            .load(Ordering::Acquire)
    );
    assert!(
        !world
            .get::<ModObserverState>(spawned[1])
            .unwrap()
            .enabled
            .load(Ordering::Acquire)
    );

    let faults: Vec<ModSystemFaulted> = world
        .resource::<Messages<ModSystemFaulted>>()
        .iter_current_update_messages()
        .cloned()
        .collect();
    let reent_fault = faults
        .iter()
        .find(|f| f.system_name == "reentrant_ctx_obs")
        .expect("reentrant fault message");
    assert_eq!(reent_fault.status, FfiResult::ACCESS_CONFLICT);
    assert!(
        reent_fault
            .last_error
            .as_deref()
            .is_some_and(|m| m.contains("re-entrant"))
    );
    let runtime_fault = faults
        .iter()
        .find(|f| f.system_name == "faulting_ctx_obs")
        .expect("runtime ctx fault message");
    assert_eq!(runtime_fault.status, FfiResult::INTERNAL_ERROR);
    assert_eq!(
        runtime_fault.last_error.as_deref(),
        Some("ctx observer explicit fault")
    );
    assert_eq!(CAPTURED.lock().unwrap_or_else(|e| e.into_inner()).len(), 2);
    assert!(
        CAPTURED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .all(|r| r.severity == Severity::Error && r.is_observer_ctx)
    );
    let _ = &mut hits;
}
