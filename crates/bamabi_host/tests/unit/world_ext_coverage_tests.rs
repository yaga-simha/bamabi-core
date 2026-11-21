use super::*;
use bamabi_macros::ModComponent;
use bevy_app::{Startup, Update};
use core::sync::atomic::{AtomicUsize, Ordering};

#[derive(Message, Default, Debug, ModComponent)]
#[mod_component(name = "world_ext_cov::NonPodMsg")]
struct NonPodMsg {
    title: String,
}

#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
struct CovVisSet;

#[derive(ScheduleLabel, Clone, Debug, PartialEq, Eq, Hash)]
struct CovCustomSchedule;

static UNLOAD_HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);

fn test_unload_hook(_world: &mut World, _id: ModId) {
    UNLOAD_HOOK_CALLS.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn mod_unload_hooks_and_world_ext_error_paths() {
    // `ModUnloadHooks` unit operations
    let mut hooks = ModUnloadHooks::new();
    assert!(hooks.is_empty());
    assert_eq!(hooks.len(), 0);
    hooks.register("k1", test_unload_hook);
    hooks.register("k1", test_unload_hook);
    assert_eq!(hooks.len(), 1);
    assert!(!hooks.is_empty());
    assert!(!hooks.unregister("missing"));
    assert!(hooks.unregister("k1"));
    assert!(hooks.is_empty());

    let mut world = World::new();
    ensure_world_mod_resources(&mut world);
    ensure_hierarchy_registered(&mut world);

    // Register & invoke unload hook on world directly
    UNLOAD_HOOK_CALLS.store(0, Ordering::SeqCst);
    register_mod_unload_hook_in_world(&mut world, "cov::hook", test_unload_hook);
    run_mod_unload_hooks(&mut world, ModId(1));
    assert_eq!(UNLOAD_HOOK_CALLS.load(Ordering::SeqCst), 1);

    // `register_mod_non_pod_message` + `register_mod_visible_set`
    assert!(world.register_mod_non_pod_message::<NonPodMsg>().is_ok());
    assert!(world.register_mod_visible_set("cov_vis", CovVisSet));
    assert!(!world.register_mod_visible_set("cov::bad", CovVisSet));

    // Built-in schedule `< CUSTOM_BASE` is rejected, custom schedule `>= CUSTOM_BASE` succeeds
    assert!(matches!(
        world.register_mod_schedule(ScheduleId::UPDATE, Update),
        Err(ModLoadError::InvalidScheduleRegistration { .. })
    ));
    assert!(matches!(
        world.register_mod_schedule(ScheduleId::STARTUP, Startup),
        Err(ModLoadError::InvalidScheduleRegistration { .. })
    ));
    assert!(
        world
            .register_mod_schedule(
                ScheduleId::custom(ScheduleId::CUSTOM_BASE),
                CovCustomSchedule
            )
            .is_ok()
    );

    world.set_mod_error_policy(ModErrorPolicy::ReportToBevy);
    assert_eq!(
        world.resource::<bamabi_host_sys::ModErrorPolicyConfig>().0,
        ModErrorPolicy::ReportToBevy
    );

    #[cfg(feature = "signature-verification")]
    {
        world.set_mod_trust_policy(ModTrustPolicy::disabled());
        assert!(world.evict_revoked_mods().is_empty());
    }

    // Missing `ModId` error paths for unload & reload methods
    assert!(matches!(
        world.unload_mod(ModId(999)),
        Err(ModLoadError::ModNotFound { id: ModId(999) })
    ));
    assert!(matches!(
        world.reload_mod(ModId(999)),
        Err(ModLoadError::ModNotFound { id: ModId(999) })
    ));
    assert!(matches!(
        world.reload_mod_with(ModId(999), LoadOptions::default()),
        Err(ModLoadError::ModNotFound { id: ModId(999) })
    ));
    assert!(matches!(
        world.reload_mod_from(ModId(999), "missing.so", LoadOptions::default()),
        Err(ModLoadError::ModNotFound { id: ModId(999) })
    ));

    world.unload_all_mods();

    assert!(
        world
            .load_mods_dir("/nonexistent/bamabi_world_ext_missing")
            .is_err()
    );
    assert!(
        world
            .load_mods_dir_with(
                "/nonexistent/bamabi_world_ext_missing",
                LoadOptions::default()
            )
            .is_err()
    );
}
