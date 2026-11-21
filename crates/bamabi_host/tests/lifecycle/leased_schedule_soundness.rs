//! Soundness tests verifying that mods cannot be loaded into, unloaded from, or reloaded
//! out of schedules currently leased out of [`Schedules`](bevy_ecs::schedule::Schedules), and that
//! [`ModCommandsExt`] operations queued from any frame schedule (`FixedUpdate`, `Last`, `Update`,
//! etc.) safely drain in [`ModLifecycleFlush`].

use crate::DROP_SLOT_POOL_RWLOCK;
use crate::support::{TempDylibCopy, fixture_dylib};
use bamabi_host::mod_dag::ModDependencyGraph;
use bamabi_host::prelude::*;
use bevy_app::{App, FixedUpdate, Last, PreUpdate, RunFixedMainLoop, Update};
use bevy_ecs::prelude::Resource;
use bevy_ecs::system::Commands;
use bevy_ecs::world::World;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Resource, Default)]
struct DirectUnloadOutcome(Arc<Mutex<Option<Result<(), ModLoadError>>>>);

#[derive(Resource, Default)]
struct DirectReloadOutcome(Arc<Mutex<Option<Result<ModId, ModLoadError>>>>);

#[derive(Resource, Default)]
struct DirectLoadOutcome(Arc<Mutex<Option<Result<ModId, ModLoadError>>>>);

#[test]
fn direct_unload_and_reload_from_leased_schedule_return_schedule_leased_and_preserve_mod() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "leased_direct_unload");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let mod_id = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load ordered_sets_mod into Update");

    let unload_res = Arc::new(Mutex::new(None));
    let reload_res = Arc::new(Mutex::new(None));
    app.insert_resource(DirectUnloadOutcome(Arc::clone(&unload_res)));
    app.insert_resource(DirectReloadOutcome(Arc::clone(&reload_res)));

    let attempt_flag = Arc::new(AtomicUsize::new(0));
    let attempt_flag_sys = Arc::clone(&attempt_flag);
    app.add_systems(Update, move |world: &mut World| {
        if attempt_flag_sys.fetch_add(1, Ordering::SeqCst) == 0 {
            let u = world.unload_mod(mod_id);
            *world.resource::<DirectUnloadOutcome>().0.lock().unwrap() = Some(u);
            let r = world.reload_mod(mod_id);
            *world.resource::<DirectReloadOutcome>().0.lock().unwrap() = Some(r);
        }
    });

    app.update();

    let u = unload_res
        .lock()
        .unwrap()
        .take()
        .expect("unload attempted in Update");
    assert!(
        matches!(
            u,
            Err(ModLoadError::ScheduleLeased { id, ref schedule })
                if id == mod_id && schedule.contains("Update")
        ),
        "expected ScheduleLeased for Update, got {u:?}"
    );

    let r = reload_res
        .lock()
        .unwrap()
        .take()
        .expect("reload attempted in Update");
    assert!(
        matches!(
            r,
            Err(ModLoadError::ScheduleLeased { id, ref schedule })
                if id == mod_id && schedule.contains("Update")
        ),
        "expected ScheduleLeased on reload for Update, got {r:?}"
    );

    // Mod must still be loaded and continue executing cleanly on subsequent frames.
    assert!(app.world().resource::<ModHost>().contains_mod(mod_id));
    app.update();
    assert!(app.world().resource::<ModHost>().contains_mod(mod_id));

    // Direct unload outside the leased schedule succeeds.
    app.unload_mod(mod_id)
        .expect("unload outside leased schedule");
    assert!(app.world().resource::<ModHost>().is_empty());
}

#[test]
fn direct_cascade_unload_rejects_atomically_when_dependent_lives_in_leased_schedule() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let dylib_a = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "cascade_dep_pre");
    let dylib_b = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "cascade_dep_upd");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    // Mod A lives in `PreUpdate`, Mod B lives in `Update` and depends on `Mod A`.
    let mod_a = app
        .load_mod_with(
            dylib_a.path(),
            LoadOptions::new()
                .with_shadow_copy(true)
                .with_schedule_override(ScheduleId::PRE_UPDATE),
        )
        .expect("load mod_a in PreUpdate");
    let mod_b = app
        .load_mod_with(dylib_b.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod_b in Update");

    app.world_mut()
        .resource_mut::<ModDependencyGraph>()
        .register_loaded_mod(
            mod_a,
            "dep_a".to_string(),
            dylib_a.path().to_path_buf(),
            LoadOptions::default(),
            Vec::new(),
        );
    app.world_mut()
        .resource_mut::<ModDependencyGraph>()
        .register_loaded_mod(
            mod_b,
            "dep_b".to_string(),
            dylib_b.path().to_path_buf(),
            LoadOptions::default(),
            vec!["dep_a".to_string()],
        );

    let unload_res = Arc::new(Mutex::new(None));
    app.insert_resource(DirectUnloadOutcome(Arc::clone(&unload_res)));
    let once = Arc::new(AtomicUsize::new(0));
    app.add_systems(Update, move |world: &mut World| {
        if once.fetch_add(1, Ordering::SeqCst) == 0 {
            let res = world.unload_mod(mod_a);
            *world.resource::<DirectUnloadOutcome>().0.lock().unwrap() = Some(res);
        }
    });

    app.update();

    let res = unload_res.lock().unwrap().take().expect("unload ran");
    assert!(
        matches!(
            res,
            Err(ModLoadError::ScheduleLeased { id, ref schedule })
                if id == mod_b && schedule.contains("Update")
        ),
        "expected ScheduleLeased pointing to leased dependent mod_b, got {res:?}"
    );
    assert!(app.world().resource::<ModHost>().contains_mod(mod_a));
    assert!(app.world().resource::<ModHost>().contains_mod(mod_b));
}

#[test]
fn commands_unload_queued_from_fixed_update_and_last_completes_without_uaf() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let dylib_fixed = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "cmd_fixed_unload");
    let dylib_last = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "cmd_last_unload");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_schedule(FixedUpdate);

    // Run `FixedUpdate` inside `RunFixedMainLoop` so `app.update()` executes `FixedUpdate` while
    // `Main`, `RunFixedMainLoop`, and `FixedUpdate` are all leased.
    app.add_systems(RunFixedMainLoop, |world: &mut World| {
        let _ = world.try_run_schedule(FixedUpdate);
    });

    let fixed_mod_id = app
        .load_mod_with(
            dylib_fixed.path(),
            LoadOptions::new()
                .with_shadow_copy(true)
                .with_schedule_override(ScheduleId::FIXED_UPDATE),
        )
        .expect("load mod in FixedUpdate");
    let last_mod_id = app
        .load_mod_with(
            dylib_last.path(),
            LoadOptions::new()
                .with_shadow_copy(true)
                .with_schedule_override(ScheduleId::LAST),
        )
        .expect("load mod in Last");

    let fixed_triggered = Arc::new(AtomicUsize::new(0));
    let fixed_triggered_sys = Arc::clone(&fixed_triggered);
    app.add_systems(FixedUpdate, move |mut commands: Commands| {
        if fixed_triggered_sys.fetch_add(1, Ordering::SeqCst) == 0 {
            commands.unload_mod(fixed_mod_id);
        }
    });

    let last_triggered = Arc::new(AtomicUsize::new(0));
    let last_triggered_sys = Arc::clone(&last_triggered);
    app.add_systems(Last, move |mut commands: Commands| {
        if last_triggered_sys.fetch_add(1, Ordering::SeqCst) == 0 {
            commands.unload_mod(last_mod_id);
        }
    });

    // Both unloads defer until `ModLifecycleFlush` runs after `Last` in the same `app.update()`.
    app.update();

    assert_eq!(fixed_triggered.load(Ordering::SeqCst), 1);
    assert_eq!(last_triggered.load(Ordering::SeqCst), 1);
    assert!(
        app.world().resource::<ModHost>().is_empty(),
        "both mods queued for unload from FixedUpdate and Last must be gone after app.update()"
    );

    // Subsequent updates execute cleanly with zero UAF.
    for _ in 0..5 {
        app.update();
    }
    assert!(app.world().resource::<ModHost>().is_empty());
}

#[test]
fn direct_load_into_leased_target_returns_schedule_leased_and_rolls_back() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "leased_direct_load");
    let late_startup_dylib =
        TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "leased_late_startup");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    // Run one initial frame so `StartupCompleted` becomes `true`.
    app.update();
    assert!(app.world().resource::<StartupCompleted>().has_run());

    let load_res = Arc::new(Mutex::new(None));
    let late_load_res = Arc::new(Mutex::new(None));
    app.insert_resource(DirectLoadOutcome(Arc::clone(&load_res)));

    let path_for_update = dylib.path().to_path_buf();
    let path_for_late = late_startup_dylib.path().to_path_buf();
    let late_res_clone = Arc::clone(&late_load_res);
    let ran = Arc::new(AtomicUsize::new(0));
    app.add_systems(Update, move |world: &mut World| {
        if ran.fetch_add(1, Ordering::SeqCst) == 0 {
            let res =
                world.load_mod_with(&path_for_update, LoadOptions::new().with_shadow_copy(true));
            *world.resource::<DirectLoadOutcome>().0.lock().unwrap() = Some(res);

            // Late-startup-only mod runs in a temporary local `Schedule`, so it succeeds even
            // while `Update` is leased.
            let late_res = world.load_mod_with(
                &path_for_late,
                LoadOptions::new()
                    .with_shadow_copy(true)
                    .with_schedule_override(ScheduleId::STARTUP),
            );
            *late_res_clone.lock().unwrap() = Some(late_res);
        }
    });

    app.update();

    let res = load_res.lock().unwrap().take().expect("load ran in Update");
    assert!(
        matches!(
            res,
            Err(ModLoadError::ScheduleLeased { ref schedule, .. })
                if schedule.contains("Update")
        ),
        "expected ScheduleLeased when loading into leased Update, got {res:?}"
    );

    let late_res = late_load_res
        .lock()
        .unwrap()
        .take()
        .expect("late startup load ran");
    let late_id = late_res.expect("late Startup load into temp schedule must succeed");

    // Only the rolled-in late-startup mod exists; the rolled-back Update mod is absent.
    let host = app.world().resource::<ModHost>();
    assert_eq!(host.len(), 1);
    assert!(host.contains_mod(late_id));

    app.unload_mod(late_id).expect("unload late startup mod");
    assert!(app.world().resource::<ModHost>().is_empty());
}

#[test]
fn unload_all_mods_skips_leased_mods_without_panicking() {
    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let dylib_pre = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "unload_all_pre");
    let dylib_upd = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "unload_all_upd");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let mod_pre = app
        .load_mod_with(
            dylib_pre.path(),
            LoadOptions::new()
                .with_shadow_copy(true)
                .with_schedule_override(ScheduleId::PRE_UPDATE),
        )
        .expect("load mod in PreUpdate");
    let mod_upd = app
        .load_mod_with(dylib_upd.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod in Update");

    let ran = Arc::new(AtomicUsize::new(0));
    app.add_systems(Update, move |world: &mut World| {
        if ran.fetch_add(1, Ordering::SeqCst) == 0 {
            world.unload_all_mods();
        }
    });

    app.update();

    let host = app.world().resource::<ModHost>();
    assert!(
        !host.contains_mod(mod_pre),
        "unleased PreUpdate mod should unload"
    );
    assert!(
        host.contains_mod(mod_upd),
        "leased Update mod must be skipped"
    );
    assert_eq!(app.world().resource::<ModHost>().len(), 1);

    app.unload_all_mods();
    assert!(app.world().resource::<ModHost>().is_empty());
    let _ = PreUpdate;
}

#[test]
fn commands_reload_from_fixed_update_completes_in_flush_and_mount_skips_leased_schedule() {
    use bamabi_host_sys::GuestSystem;
    use bamabi_host_sys::schedule::mount_mod_systems_in_world;
    use bamabi_sys::BamSystemCtx;
    use bevy_ecs::schedule::Schedules;
    use core::ffi::c_void;

    unsafe extern "C" fn noop_guest_cb(
        _ctx: *mut BamSystemCtx,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    let _pool_guard = DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "cmd_fixed_reload");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_schedule(FixedUpdate);
    app.add_systems(RunFixedMainLoop, |world: &mut World| {
        let _ = world.try_run_schedule(FixedUpdate);
    });

    let orig_id = app
        .load_mod_with(
            dylib.path(),
            LoadOptions::new()
                .with_shadow_copy(true)
                .with_schedule_override(ScheduleId::FIXED_UPDATE),
        )
        .expect("load mod in FixedUpdate");

    let reloaded_flag = Arc::new(AtomicUsize::new(0));
    let reloaded_flag_sys = Arc::clone(&reloaded_flag);
    app.add_systems(FixedUpdate, move |mut commands: Commands| {
        if reloaded_flag_sys.fetch_add(1, Ordering::SeqCst) == 0 {
            commands.reload_mod_with(
                orig_id,
                LoadOptions::new()
                    .with_shadow_copy(true)
                    .with_schedule_override(ScheduleId::FIXED_UPDATE),
            );
        }
    });

    app.update();

    let host = app.world().resource::<ModHost>();
    assert_eq!(host.len(), 1);
    let new_id = host.loaded_mods()[0].id();
    assert_ne!(new_id, orig_id);

    // Next update runs reloaded system in FixedUpdate without UAF.
    app.update();

    // Direct `mount_mod_systems_in_world` skips leased `Update` without creating a colliding entry.
    let reg = app.world().resource::<ModRegistry>().clone();
    let guest_sys = GuestSystem::new(
        "skipped_sys",
        Vec::new(),
        noop_guest_cb,
        core::ptr::null_mut(),
        reg,
    )
    .with_mod_id(new_id);
    let orig_update = app
        .world_mut()
        .resource_mut::<Schedules>()
        .remove_temporarily(Update)
        .expect("Update schedule exists");
    mount_mod_systems_in_world(app.world_mut(), "ordered_sets_mod", 0, vec![guest_sys])
        .expect("mount_mod_systems_in_world skips leased schedule cleanly");
    assert!(
        !app.world().resource::<Schedules>().contains(Update),
        "mount_mod_systems_in_world must not insert a shadow Update schedule while Update is leased"
    );
    app.world_mut()
        .resource_mut::<Schedules>()
        .reinsert(orig_update);

    app.unload_all_mods();
    assert!(app.world().resource::<ModHost>().is_empty());
}
