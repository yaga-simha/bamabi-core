use super::*;
use crate::budgets::ModBudgetPolicy;
use bevy_app::{MainScheduleOrder, Update};

#[test]
fn configured_plugin_builders_and_main_schedule_without_last() {
    let cfg = AbiModPlugin::with_error_policy(ModErrorPolicy::Log)
        .with_budget_policy(ModBudgetPolicy::default())
        .with_error_policy(ModErrorPolicy::Quarantine);
    assert_eq!(cfg.error_policy, ModErrorPolicy::Quarantine);
    assert!(cfg.budget_policy.is_some());

    #[cfg(feature = "signature-verification")]
    {
        let cfg_trust = AbiModPlugin::with_trust_policy(ModTrustPolicy::disabled())
            .with_trust_policy(ModTrustPolicy::disabled());
        assert!(cfg_trust.trust_policy.is_some());
    }

    let order = MainScheduleOrder {
        labels: vec![Update.intern()],
        ..Default::default()
    };

    let mut app = App::new();
    app.insert_resource(order);
    ensure_mod_lifecycle_flush_schedule(&mut app);

    let updated = app.world().resource::<MainScheduleOrder>();
    assert_eq!(
        updated.labels.last().copied(),
        Some(ModLifecycleFlush.intern())
    );
}
