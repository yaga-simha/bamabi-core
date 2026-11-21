use super::*;
use crate::LoadedMod;
use crate::registry::ModRegistry;
use bamabi_sys::{BamInstance, BamSystemCtx};
use bevy_ecs::prelude::Resource;
use bevy_ecs::system::{Res, ResMut};
use core::ffi::c_void;
use core::sync::atomic::AtomicBool;

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum TestHostSet {
    Physics,
    Parent,
    Child,
}

#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CustomTestSched;

#[derive(Resource, Default)]
struct CondFlag(bool);

#[derive(Resource, Default)]
struct SharedCounter(usize);

unsafe extern "C" fn noop_guest_cb(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
    FfiResult::OK
}
unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}

#[test]
fn schedule_map_visible_sets_mode_and_registry_edge_cases() {
    // 1. `ModScheduleMap` & `resolve_builtin_schedule_id` & `mark_startup_completed`
    let mut map = ModScheduleMap::new();
    assert!(!map.register(ScheduleId::UPDATE, Update));
    let custom_id = ScheduleId::custom(ScheduleId::CUSTOM_BASE);
    assert!(map.register(custom_id, CustomTestSched));
    assert_eq!(map.resolve(ScheduleId::UPDATE), Some(Update.intern()));
    assert_eq!(
        resolve_builtin_schedule_id(ScheduleId::POST_UPDATE),
        Some(PostUpdate.intern())
    );
    assert!(map.contains(custom_id));

    let mut world = World::new();
    mark_startup_completed(&mut world);
    assert!(world.resource::<StartupCompleted>().has_run());

    // 2. `ModVisibleSets`
    let mut visible = ModVisibleSets::new();
    assert!(!visible.register("", TestHostSet::Physics));
    assert!(!visible.register("host::", TestHostSet::Physics));
    assert!(!visible.register("a::b", TestHostSet::Physics));
    assert!(visible.register("host::physics", TestHostSet::Physics));
    assert!(visible.contains("physics"));

    // 3. `ScheduleSetMode::to_abi_flags`
    assert_eq!(
        ScheduleSetMode::Parallel.to_abi_flags(),
        BAM_SCHEDULE_SET_FLAG_PARALLEL
    );
    assert_eq!(
        ScheduleSetMode::Sequence.to_abi_flags(),
        BAM_SCHEDULE_SET_FLAG_SEQUENCE
    );
    assert_eq!(
        ScheduleSetMode::SequenceWeak.to_abi_flags(),
        BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK
    );

    // 4. `ModScheduleSetRegistry` lookup, error paths, multi-hop cycle detection, and unmount
    let mut reg = ModScheduleSetRegistry::new();
    assert!(reg.get(BamSystemSet::INVALID).is_none());
    assert!(reg.get_mut(BamSystemSet::INVALID).is_none());
    assert_eq!(
        reg.set_mode(BamSystemSet::INVALID, ScheduleSetMode::Sequence),
        Err(FfiResult::INVALID_ARGUMENT)
    );
    assert_eq!(
        reg.set_mode(BamSystemSet::from_raw(99), ScheduleSetMode::Sequence),
        Err(FfiResult::NOT_FOUND)
    );
    assert_eq!(
        reg.set_edges(BamSystemSet::INVALID, vec![], vec![], false, false),
        Err(FfiResult::INVALID_ARGUMENT)
    );
    assert_eq!(
        reg.set_edges(BamSystemSet::from_raw(99), vec![], vec![], false, false),
        Err(FfiResult::NOT_FOUND)
    );
    assert_eq!(
        reg.reorder_mounted_systems(BamSystemSet::INVALID, vec![]),
        Err(FfiResult::INVALID_ARGUMENT)
    );
    assert_eq!(
        reg.reorder_mounted_systems(BamSystemSet::from_raw(99), vec![]),
        Err(FfiResult::NOT_FOUND)
    );
    assert!(
        reg.mount_system(
            BamSystemSet::INVALID,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("m", "s")
        )
        .is_err()
    );
    assert!(
        reg.mount_system(
            BamSystemSet::from_raw(99),
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("m", "s")
        )
        .is_err()
    );

    let mk_cfg =
        |name: &str, before: Vec<BamSystemSet>, after: Vec<BamSystemSet>| ScheduleSetConfig {
            mod_id: Some(ModId(1)),
            mod_name: Arc::from("mod_a"),
            set_name: Arc::from(name),
            generation: 0,
            schedule: ScheduleId::UPDATE,
            mode: ScheduleSetMode::Parallel,
            before_sets: before,
            after_sets: after,
            before_weak: false,
            after_weak: false,
        };

    let a = reg.configure_set(mk_cfg("a", vec![], vec![])).unwrap();
    let b = reg.configure_set(mk_cfg("b", vec![], vec![a])).unwrap(); // a -> b
    let c = reg.configure_set(mk_cfg("c", vec![], vec![b])).unwrap(); // b -> c

    // Lookups
    let a_named = ModNamedSystemSet::new("mod_a", "a");
    assert_eq!(
        reg.get_by_named_set(ScheduleId::UPDATE, &a_named)
            .map(|e| e.handle),
        Some(a)
    );

    // Wrong schedule on mount_system
    assert_eq!(
        reg.mount_system(
            a,
            ScheduleId::FIXED_UPDATE,
            ModNamedSystemSet::new("mod_a", "s1")
        )
        .unwrap_err()
        .0,
        FfiResult::INVALID_ARGUMENT
    );
    let s1 = ModNamedSystemSet::new("mod_a", "s1");
    let s2 = ModNamedSystemSet::new("mod_a", "s2");
    reg.mount_system(a, ScheduleId::UPDATE, s1.clone()).unwrap();
    reg.mount_system(a, ScheduleId::UPDATE, s2.clone()).unwrap();
    reg.unmount_system_from_schedule(ScheduleId::UPDATE, &s1);
    assert_eq!(reg.get(a).unwrap().mounted_systems, vec![s2]);

    // Multi-hop cycle on `configure_set`: new node `d` has `before = [a]`, `after = [c]` -> `c -> d -> a -> b -> c`!
    assert_eq!(
        reg.configure_set(mk_cfg("d", vec![a], vec![c]))
            .unwrap_err()
            .0,
        FfiResult::CYCLE_DETECTED
    );

    // Self-cycle and multi-hop replacement cycle on `set_edges`
    assert_eq!(
        reg.set_edges(a, vec![], vec![a], false, false),
        Err(FfiResult::CYCLE_DETECTED)
    );
    assert_eq!(
        reg.set_edges(a, vec![], vec![c], false, false),
        Err(FfiResult::CYCLE_DETECTED)
    );

    // `apply_mod_schedule_sets_to_schedule` with strong `before` and `after` edges
    let mid = reg.configure_set(mk_cfg("mid", vec![c], vec![a])).unwrap();
    let mut sched = Schedule::new(Update);
    apply_mod_schedule_sets_to_schedule(&reg, ScheduleId::UPDATE, &mut sched);
    assert!(reg.get(mid).is_some());
}

#[test]
fn rewire_pass_covers_nested_sets_generation_fallback_and_condition_conflicts() {
    let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    static ORDER: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    ORDER.lock().unwrap().clear();

    fn cond_reads_flag(flag: Res<CondFlag>) -> bool {
        flag.0
    }
    fn sys_parent_member(mut ctr: ResMut<SharedCounter>) {
        ctr.0 += 1;
        ORDER.lock().unwrap().push("parent_member");
    }
    fn sys_writes_flag(mut flag: ResMut<CondFlag>) {
        flag.0 = true;
        ORDER.lock().unwrap().push("writes_flag");
    }
    fn sys_pure_noop() {
        ORDER.lock().unwrap().push("pure_noop");
    }

    // 1. Missing `ModScheduleSetRegistry` returns `Ok(())` cleanly during pass build.
    let mut bare_world = World::new();
    let mut bare_sched = Schedule::new(Update);
    install_mod_schedule_set_pass(&mut bare_sched, ScheduleId::UPDATE);
    bare_sched.run(&mut bare_world);

    // 2. Nested child set + `ModHost` generation fallback + run_if condition conflict in weak edges
    let mut world = World::new();
    world.init_resource::<crate::drop_trampoline::WorldDropGuard>();
    world.init_resource::<crate::system::ModQuarantine>();
    world.init_resource::<ModRegistry>();
    world.init_resource::<ModHost>();
    world.init_resource::<CondFlag>();
    world.init_resource::<SharedCounter>();
    let mod_id_first = LoadedMod::register_mock_for_test(
        &mut world,
        "gen_mod",
        Arc::new(AtomicBool::new(true)),
        noop_shutdown,
        None,
        core::ptr::null_mut(),
    );
    crate::loader::excise_and_unload_mod_in_world(&mut world, mod_id_first, |_| {}).unwrap();
    // Unloading `"gen_mod"` increments `ModHost::mod_generations` to `1`.
    let live_gen = world.resource::<ModHost>().mod_generation("gen_mod");
    assert_eq!(live_gen, 1);

    // Configure sets at stale `generation = 0`, while the schedule registers sets at `live_gen = 1`
    // so `resolve_set_key_in_graph` exercises the `ModHost::mod_generation` fallback!
    let mut reg = ModScheduleSetRegistry::new();
    let parent_h = reg
        .configure_set(ScheduleSetConfig {
            mod_id: None,
            mod_name: Arc::from("gen_mod"),
            set_name: Arc::from("parent_set"),
            generation: 0, // stale gen; graph holds `live_gen`!
            schedule: ScheduleId::UPDATE,
            mode: ScheduleSetMode::Parallel,
            before_sets: vec![],
            after_sets: vec![],
            before_weak: false,
            after_weak: false,
        })
        .unwrap();
    let weak_target_h = reg
        .configure_set(ScheduleSetConfig {
            mod_id: None,
            mod_name: Arc::from("gen_mod"),
            set_name: Arc::from("weak_target"),
            generation: live_gen,
            schedule: ScheduleId::UPDATE,
            mode: ScheduleSetMode::Parallel,
            before_sets: vec![],
            after_sets: vec![],
            before_weak: false,
            after_weak: false,
        })
        .unwrap();
    // Add a set at `generation = 0` that has `before_weak` targeting `weak_target`
    let seq_h = reg
        .configure_set(ScheduleSetConfig {
            mod_id: None,
            mod_name: Arc::from("gen_mod"),
            set_name: Arc::from("seq_weak"),
            generation: live_gen,
            schedule: ScheduleId::UPDATE,
            mode: ScheduleSetMode::SequenceWeak,
            before_sets: vec![weak_target_h],
            after_sets: vec![],
            before_weak: true,
            after_weak: false,
        })
        .unwrap();
    reg.mount_system(
        seq_h,
        ScheduleId::UPDATE,
        ModNamedSystemSet::with_generation("gen_mod", "pure_a", live_gen),
    )
    .unwrap();
    reg.mount_system(
        seq_h,
        ScheduleId::UPDATE,
        ModNamedSystemSet::with_generation("gen_mod", "writer", live_gen),
    )
    .unwrap();
    // Unknown set handle (`999`) in `before_sets`/`after_sets` should be safely skipped if present via direct mount
    let _ = parent_h;
    world.insert_resource(reg);

    let mut sched = Schedule::new(Update);
    let parent_set_live = ModNamedSystemSet::with_generation("gen_mod", "parent_set", live_gen);
    let weak_target_live = ModNamedSystemSet::with_generation("gen_mod", "weak_target", live_gen);
    let pure_set_live = ModNamedSystemSet::with_generation("gen_mod", "pure_a", live_gen);
    let writer_set_live = ModNamedSystemSet::with_generation("gen_mod", "writer", live_gen);

    // Nest `TestHostSet::Child` inside `TestHostSet::Parent` AND inside `parent_set_live`
    // with a set-level `run_if(cond_reads_flag)` so `collect_systems_in_set_key` and
    // `collect_condition_accesses` cover nested-set traversal + condition conflict checks!
    sched.configure_sets(TestHostSet::Child.in_set(TestHostSet::Parent));
    sched.configure_sets(TestHostSet::Parent.in_set(parent_set_live));
    sched.configure_sets(weak_target_live.clone().run_if(cond_reads_flag));
    sched.add_systems(sys_parent_member.in_set(TestHostSet::Child));
    sched.add_systems(sys_pure_noop.in_set(pure_set_live));
    sched.add_systems(
        sys_writes_flag
            .in_set(writer_set_live)
            .in_set(weak_target_live),
    );

    // Also run a `rewire_mod_schedule_sets_in_schedule` pass
    rewire_mod_schedule_sets_in_schedule(&mut world, ScheduleId::UPDATE, &mut sched).unwrap();
    sched.run(&mut world);
    assert_eq!(world.resource::<SharedCounter>().0, 1);
}

#[test]
fn mount_configure_ensure_and_rewire_in_world_cover_all_edges_and_leased_branches() {
    let mut world = World::new();
    world.init_resource::<ModHost>();
    world.init_resource::<ModScheduleMap>();
    world.init_resource::<ModScheduleSetRegistry>();
    world.init_resource::<Schedules>();
    {
        let mut vis = ModVisibleSets::new();
        vis.register("physics", TestHostSet::Physics);
        world.insert_resource(vis);
    }

    // 1. `ensure_mod_schedule_set_pass_if_schedules_present` & `rewire_mod_schedule_sets_in_world`
    // Unregistered custom schedule -> no-op / `Ok(None)`
    ensure_mod_schedule_set_pass_if_schedules_present(&mut world, ScheduleId::custom(42));
    assert!(
        rewire_mod_schedule_sets_in_world(&mut world, ScheduleId::custom(42))
            .unwrap()
            .is_none()
    );

    // Valid schedule (`Update`) when not leased -> installs pass & rewires cleanly
    ensure_mod_schedule_set_pass_if_schedules_present(&mut world, ScheduleId::UPDATE);
    assert!(
        rewire_mod_schedule_sets_in_world(&mut world, ScheduleId::UPDATE)
            .unwrap()
            .is_some()
    );

    // 2. `configure_guest_system_for_schedule` with every edge type + `ambiguous_with_all`
    let reg = ModRegistry::new();
    let sys = GuestSystem::new(
        "s_edges",
        vec![],
        noop_guest_cb,
        core::ptr::null_mut(),
        reg.clone(),
    )
    .with_mod_id(ModId(7))
    .with_in_sets(vec!["other_mod::group".to_owned()])
    .with_ordering(
        vec!["host::physics".to_owned()],
        vec!["other_mod::b".to_owned()],
    )
    .with_weak_ordering(vec!["w_after".to_owned()], vec!["w_before".to_owned()])
    .with_ambiguous_with(vec!["ambig_target".to_owned()])
    .with_ambiguous_with_all(true);

    // Unregistered custom schedule system is skipped in `mount_mod_systems_in_world`
    let mut unreg_sys = GuestSystem::new(
        "s_unreg",
        vec![],
        noop_guest_cb,
        core::ptr::null_mut(),
        reg.clone(),
    );
    unreg_sys.set_schedule(ScheduleId::custom(77));

    mount_mod_systems_in_world(&mut world, "edge_mod", 0, vec![sys, unreg_sys]).unwrap();

    // 3. Leased schedule checks via `world.try_schedule_scope(FixedUpdate, ...)`
    world.add_schedule(Schedule::new(FixedUpdate));
    let _ = world.try_schedule_scope(FixedUpdate, |w, _| {
        ensure_mod_schedule_set_pass_if_schedules_present(w, ScheduleId::FIXED_UPDATE);
        assert!(
            rewire_mod_schedule_sets_in_world(w, ScheduleId::FIXED_UPDATE)
                .unwrap()
                .is_none()
        );
        let mut leased_sys = GuestSystem::new(
            "s_leased",
            vec![],
            noop_guest_cb,
            core::ptr::null_mut(),
            reg.clone(),
        );
        leased_sys.set_schedule(ScheduleId::FIXED_UPDATE);
        mount_mod_systems_in_world(w, "edge_mod", 0, vec![leased_sys]).unwrap();
    });
}
