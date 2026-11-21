use super::*;
use crate::AbiModPlugin;
use crate::app_ext::ModAppExt;
use bamabi_host_sys::{GuestSystem, ModRegistry};
use bamabi_sys::{BamSystemCtx, FfiResult, ScheduleId};
use bevy_app::{App, Startup, Update};
use bevy_ecs::schedule::{ScheduleLabel, SystemSet};
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::path::Path;

#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
struct HostVisibleGroup;

extern "C" fn ok_guest_cb(_ctx: *mut BamSystemCtx, _user_data: *mut c_void) -> FfiResult {
    FfiResult::OK
}

static LATE_RUNS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn bump_late_cb(_ctx: *mut BamSystemCtx, _user_data: *mut c_void) -> FfiResult {
    LATE_RUNS.fetch_add(1, Ordering::SeqCst);
    FfiResult::OK
}

fn make_sys(name: &str, schedule: ScheduleId, reg: ModRegistry) -> GuestSystem {
    GuestSystem::new(name, Vec::new(), ok_guest_cb, core::ptr::null_mut(), reg)
        .with_schedule(schedule)
}

#[test]
fn prevalidate_ordering_edges_weak_chains_and_late_startup_cover_edge_branches() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_visible_set("vis", HostVisibleGroup);

    // Ensure `Update` and `Startup` schedules exist for lease / scheduling checks.
    app.init_schedule(Update);
    app.init_schedule(Startup);

    let reg = app.world().resource::<ModRegistry>().clone();

    // 1. Unregistered custom schedule -> `Err(UnregisteredSchedule)`
    let bad_sched_sys = make_sys("bad_sched", ScheduleId::custom(7777), reg.clone());
    assert!(matches!(
        prevalidate_mod_schedules_and_ordering(
            app.world(),
            ModId(1),
            Path::new("mod.so"),
            "m",
            false,
            &[bad_sched_sys],
        ),
        Err(ModLoadError::UnregisteredSchedule { .. })
    ));

    // 2. Leased schedule while `Update` is currently executing / scoped -> `Err(ScheduleLeased)`
    let upd_sys = make_sys("upd", ScheduleId::UPDATE, reg.clone());
    app.world_mut().schedule_scope(Update, |world, _| {
        assert!(leased_schedules(world).contains(&Update.intern()));
        assert!(matches!(
            prevalidate_mod_schedules_and_ordering(
                world,
                ModId(1),
                Path::new("mod.so"),
                "m",
                false,
                core::slice::from_ref(&upd_sys),
            ),
            Err(ModLoadError::ScheduleLeased { .. })
        ));
        // Also cover `configure_weak_set_chain` skip when target schedule is leased.
        configure_weak_set_chain(
            world,
            Update.intern(),
            &[
                ModNamedSystemSet::new("m", "a"),
                ModNamedSystemSet::new("m", "b"),
            ],
        );
    });

    // Early return on `< 2` sets
    configure_weak_set_chain(
        app.world_mut(),
        Update.intern(),
        &[ModNamedSystemSet::new("m", "only_one")],
    );

    // 3. `strict = true` validation failures: invalid syntax & unresolved host set
    let bad_syntax = make_sys("upd", ScheduleId::UPDATE, reg.clone())
        .with_ordering(vec!["no_double_colon".to_string()], vec![]);
    assert!(matches!(
        prevalidate_mod_schedules_and_ordering(
            app.world(),
            ModId(1),
            Path::new("mod.so"),
            "m",
            true,
            &[bad_syntax],
        ),
        Err(ModLoadError::UnresolvedOrderingTarget { .. })
    ));

    let missing_host = make_sys("upd", ScheduleId::UPDATE, reg.clone())
        .with_ordering(vec![], vec!["host::missing".to_string()]);
    assert!(matches!(
        prevalidate_mod_schedules_and_ordering(
            app.world(),
            ModId(1),
            Path::new("mod.so"),
            "m",
            true,
            &[missing_host],
        ),
        Err(ModLoadError::UnresolvedOrderingTarget { .. })
    ));

    // Also unresolved weak target syntax in strict mode fails.
    let bad_weak_syntax = make_sys("upd", ScheduleId::UPDATE, reg.clone())
        .with_weak_ordering(vec!["::empty_mod".to_string()], vec![]);
    assert!(matches!(
        prevalidate_mod_schedules_and_ordering(
            app.world(),
            ModId(1),
            Path::new("mod.so"),
            "m",
            true,
            &[bad_weak_syntax],
        ),
        Err(ModLoadError::UnresolvedOrderingTarget { .. })
    ));

    // Non-strict mode tolerates unresolved strong, weak, invalid syntax, and cross-mod edges
    // and registers systems cleanly when scheduled.
    let build_lenient = || {
        let a = make_sys("a", ScheduleId::UPDATE, reg.clone())
            .with_in_sets(vec!["custom_group".to_string()])
            .with_main_thread(true)
            .with_ordering(
                vec!["host::vis".to_string(), "bad_target".to_string()],
                vec!["m::b".to_string(), "other_mod::x".to_string()],
            )
            .with_weak_ordering(
                vec!["host::vis".to_string(), "other_mod::missing".to_string()],
                vec!["host::missing_weak".to_string(), "bad_weak".to_string()],
            )
            .with_ambiguous_with(vec!["m::b".to_string(), "other_mod::any".to_string()]);

        let b = make_sys("b", ScheduleId::UPDATE, reg.clone())
            .with_ambiguous_with_all(true)
            .with_weak_ordering(
                vec!["m::a".to_string()],
                vec!["m::custom_group".to_string()],
            );
        (a, b)
    };

    let (lenient_a, lenient_b) = build_lenient();
    assert!(
        prevalidate_mod_schedules_and_ordering(
            app.world(),
            ModId(1),
            Path::new("mod.so"),
            "m",
            false,
            &[lenient_a, lenient_b],
        )
        .is_ok()
    );

    let (lenient_a, lenient_b) = build_lenient();
    schedule_mod_systems_in_world(app.world_mut(), ModId(1), "m", vec![lenient_a, lenient_b]);

    // 4. `run_late_startup_systems_in_world`: empty slice + non-empty late startup batch
    run_late_startup_systems_in_world(app.world_mut(), "m", Vec::new());

    LATE_RUNS.store(0, Ordering::SeqCst);
    app.world_mut().insert_resource(StartupCompleted(true));
    let build_late = || {
        let late_first = GuestSystem::new(
            "first",
            Vec::new(),
            bump_late_cb,
            core::ptr::null_mut(),
            reg.clone(),
        )
        .with_schedule(ScheduleId::STARTUP)
        .with_weak_ordering(vec![], vec!["m::second".to_string()]);
        let late_second = GuestSystem::new(
            "second",
            Vec::new(),
            bump_late_cb,
            core::ptr::null_mut(),
            reg.clone(),
        )
        .with_schedule(ScheduleId::STARTUP)
        .with_ordering(vec!["m::first".to_string()], vec![]);
        (late_first, late_second)
    };

    let (late_first, late_second) = build_late();
    assert!(
        prevalidate_mod_schedules_and_ordering(
            app.world(),
            ModId(2),
            Path::new("late.so"),
            "m",
            true,
            &[late_first, late_second],
        )
        .is_ok()
    );

    let (late_first, late_second) = build_late();
    let late_returned = schedule_mod_systems_in_world(
        app.world_mut(),
        ModId(2),
        "m",
        vec![late_first, late_second],
    );
    assert_eq!(late_returned.len(), 2);
    run_late_startup_systems_in_world(app.world_mut(), "m", late_returned);
    assert_eq!(LATE_RUNS.load(Ordering::SeqCst), 2);
}
