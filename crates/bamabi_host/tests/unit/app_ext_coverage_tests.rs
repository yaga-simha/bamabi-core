use super::*;
use crate::AbiModPlugin;
use bamabi_macros::ModComponent;
use bevy_ecs::message::Message;

#[derive(Message, Default, Debug, ModComponent)]
#[mod_component(name = "app_ext_cov::NonPodMsg")]
struct NonPodMsg {
    label: String,
}

#[test]
fn app_ext_forwards_non_pod_message_policy_and_load_dir_error() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    assert!(app.register_mod_non_pod_message::<NonPodMsg>().is_ok());
    app.set_mod_error_policy(ModErrorPolicy::Log);
    assert_eq!(
        app.world()
            .resource::<bamabi_host_sys::ModErrorPolicyConfig>()
            .0,
        ModErrorPolicy::Log
    );

    #[cfg(feature = "signature-verification")]
    {
        app.set_mod_trust_policy(ModTrustPolicy::disabled());
        assert!(app.evict_revoked_mods().is_empty());
    }

    let err = app
        .load_mods_dir("/nonexistent/bamabi_app_ext_missing_mods")
        .expect_err("nonexistent directory should yield ModDagError::Io");
    assert!(err.is_io());
}
