use super::*;
use crate::ensure_mod_lifecycle_flush_schedule;
use bevy_app::{App, FixedUpdate, Last, MainScheduleOrder, Update};
use bevy_ecs::schedule::Schedules;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn ops_are_deferred_when_non_update_schedule_is_leased() {
    let mut app = App::new();
    ensure_mod_lifecycle_flush_schedule(&mut app);
    app.init_schedule(FixedUpdate);

    let executed = Arc::new(AtomicUsize::new(0));

    // Lease `FixedUpdate` (not `Update`): op must defer into `PendingModLifecycleOps`.
    let fixed_sched = app
        .world_mut()
        .resource_mut::<Schedules>()
        .remove_temporarily(FixedUpdate)
        .expect("FixedUpdate schedule exists");

    let hit = Arc::clone(&executed);
    run_or_defer_lifecycle_op(app.world_mut(), move |_| {
        hit.fetch_add(1, Ordering::SeqCst);
    });

    assert_eq!(executed.load(Ordering::SeqCst), 0);
    assert_eq!(
        app.world().resource::<PendingModLifecycleOps>().queue.len(),
        1
    );

    app.world_mut()
        .resource_mut::<Schedules>()
        .reinsert(fixed_sched);

    // Lease `Main` without `ModLifecycleFlush`: op must also defer.
    let main_sched = app
        .world_mut()
        .resource_mut::<Schedules>()
        .remove_temporarily(Main)
        .expect("Main schedule exists");

    let hit2 = Arc::clone(&executed);
    run_or_defer_lifecycle_op(app.world_mut(), move |_| {
        hit2.fetch_add(1, Ordering::SeqCst);
    });

    assert_eq!(executed.load(Ordering::SeqCst), 0);
    assert_eq!(
        app.world().resource::<PendingModLifecycleOps>().queue.len(),
        2
    );

    app.world_mut()
        .resource_mut::<Schedules>()
        .reinsert(main_sched);

    // With empty leased set, `run_or_defer_lifecycle_op` runs immediately.
    let hit3 = Arc::clone(&executed);
    run_or_defer_lifecycle_op(app.world_mut(), move |_| {
        hit3.fetch_add(10, Ordering::SeqCst);
    });
    assert_eq!(executed.load(Ordering::SeqCst), 10);
    assert_eq!(
        app.world().resource::<PendingModLifecycleOps>().queue.len(),
        2
    );

    // Bare `World` without `PendingModLifecycleOps` runs immediately even when a schedule is leased.
    let mut bare_world = World::new();
    bare_world.init_resource::<Schedules>();
    bare_world
        .resource_mut::<Schedules>()
        .insert(bevy_ecs::schedule::Schedule::new(FixedUpdate));
    let bare_fixed = bare_world
        .resource_mut::<Schedules>()
        .remove_temporarily(FixedUpdate)
        .expect("bare FixedUpdate");
    let bare_hit = Arc::new(AtomicUsize::new(0));
    let bare_hit_clone = Arc::clone(&bare_hit);
    run_or_defer_lifecycle_op(&mut bare_world, move |_| {
        bare_hit_clone.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(bare_hit.load(Ordering::SeqCst), 1);
    bare_world.resource_mut::<Schedules>().reinsert(bare_fixed);
}

#[test]
fn ops_are_drained_by_running_mod_lifecycle_flush() {
    let mut app = App::new();
    ensure_mod_lifecycle_flush_schedule(&mut app);
    app.init_schedule(FixedUpdate);

    let counter = Arc::new(AtomicUsize::new(0));

    let fixed_sched = app
        .world_mut()
        .resource_mut::<Schedules>()
        .remove_temporarily(FixedUpdate)
        .expect("FixedUpdate schedule exists");

    let c1 = Arc::clone(&counter);
    run_or_defer_lifecycle_op(app.world_mut(), move |w| {
        // Inside `ModLifecycleFlush`, `ModLifecycleFlush` is leased and `run_or_defer_lifecycle_op`
        // should execute nested ops immediately.
        c1.fetch_add(1, Ordering::SeqCst);
        let c_nested = Arc::clone(&c1);
        run_or_defer_lifecycle_op(w, move |_| {
            c_nested.fetch_add(2, Ordering::SeqCst);
        });
    });

    assert_eq!(counter.load(Ordering::SeqCst), 0);
    assert_eq!(
        app.world().resource::<PendingModLifecycleOps>().queue.len(),
        1
    );

    app.world_mut()
        .resource_mut::<Schedules>()
        .reinsert(fixed_sched);

    app.world_mut().run_schedule(ModLifecycleFlush);
    assert_eq!(counter.load(Ordering::SeqCst), 3);
    assert!(
        app.world()
            .resource::<PendingModLifecycleOps>()
            .queue
            .is_empty()
    );
}

#[test]
fn ops_are_requeued_when_mod_schedule_is_leased_at_flush_time() {
    let mut app = App::new();
    ensure_mod_lifecycle_flush_schedule(&mut app);
    app.init_schedule(FixedUpdate);
    app.init_schedule(Update);

    let log = Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));

    let fixed_sched = app
        .world_mut()
        .resource_mut::<Schedules>()
        .remove_temporarily(FixedUpdate)
        .expect("FixedUpdate exists");

    let log1 = Arc::clone(&log);
    run_or_defer_lifecycle_op(app.world_mut(), move |_| {
        log1.lock().expect("lock").push(1);
    });
    let log2 = Arc::clone(&log);
    run_or_defer_lifecycle_op(app.world_mut(), move |_| {
        log2.lock().expect("lock").push(2);
    });
    assert_eq!(
        app.world().resource::<PendingModLifecycleOps>().queue.len(),
        2
    );

    // Flush while `FixedUpdate` is STILL leased: both ops must be re-queued without running.
    app.world_mut().run_schedule(ModLifecycleFlush);
    assert!(log.lock().expect("lock").is_empty());
    assert_eq!(
        app.world().resource::<PendingModLifecycleOps>().queue.len(),
        2
    );

    // Return `FixedUpdate`, then flush: both ops drain in FIFO order.
    app.world_mut()
        .resource_mut::<Schedules>()
        .reinsert(fixed_sched);
    app.world_mut().run_schedule(ModLifecycleFlush);
    assert_eq!(&*log.lock().expect("lock"), &[1, 2]);
    assert!(
        app.world()
            .resource::<PendingModLifecycleOps>()
            .queue
            .is_empty()
    );

    // Partial flush: op A runs and leases `Update`; op B must be re-queued without re-running A.
    let log_a = Arc::clone(&log);
    let log_b = Arc::clone(&log);
    let leased_update = Arc::new(std::sync::Mutex::new(None));
    let leased_update_holder = Arc::clone(&leased_update);

    let temp_fixed = app
        .world_mut()
        .resource_mut::<Schedules>()
        .remove_temporarily(FixedUpdate)
        .expect("FixedUpdate exists");
    run_or_defer_lifecycle_op(app.world_mut(), move |w| {
        log_a.lock().expect("lock").push(10);
        let sched = w
            .resource_mut::<Schedules>()
            .remove_temporarily(Update)
            .expect("Update exists");
        *leased_update_holder.lock().expect("lock") = Some(sched);
    });
    run_or_defer_lifecycle_op(app.world_mut(), move |_| {
        log_b.lock().expect("lock").push(20);
    });
    app.world_mut()
        .resource_mut::<Schedules>()
        .reinsert(temp_fixed);

    app.world_mut().run_schedule(ModLifecycleFlush);
    assert_eq!(&*log.lock().expect("lock"), &[1, 2, 10]);
    assert_eq!(
        app.world().resource::<PendingModLifecycleOps>().queue.len(),
        1
    );

    let taken_update = leased_update
        .lock()
        .expect("lock")
        .take()
        .expect("Update was leased");
    app.world_mut()
        .resource_mut::<Schedules>()
        .reinsert(taken_update);
    app.world_mut().run_schedule(ModLifecycleFlush);
    assert_eq!(&*log.lock().expect("lock"), &[1, 2, 10, 20]);
    assert!(
        app.world()
            .resource::<PendingModLifecycleOps>()
            .queue
            .is_empty()
    );
}

#[test]
fn ensure_mod_lifecycle_flush_schedule_is_idempotent() {
    let mut app = App::new();
    ensure_mod_lifecycle_flush_schedule(&mut app);
    ensure_mod_lifecycle_flush_schedule(&mut app);
    app.add_plugins(crate::AbiModPlugin);
    ensure_mod_lifecycle_flush_schedule(&mut app);

    let flush_label = ModLifecycleFlush.intern();
    let last_label = Last.intern();
    let order = app.world().resource::<MainScheduleOrder>();
    let flush_positions: Vec<usize> = order
        .labels
        .iter()
        .enumerate()
        .filter_map(|(idx, label)| (*label == flush_label).then_some(idx))
        .collect();
    assert_eq!(flush_positions.len(), 1);
    let last_pos = order
        .labels
        .iter()
        .position(|label| *label == last_label)
        .expect("Last in MainScheduleOrder");
    assert_eq!(flush_positions[0], last_pos + 1);

    let sched = app
        .world()
        .resource::<Schedules>()
        .get(ModLifecycleFlush)
        .expect("ModLifecycleFlush schedule exists");
    assert_eq!(sched.systems_len(), 1);

    // Calling without `MainScheduleOrder` (e.g., `App::empty()`) also succeeds idempotently.
    let mut empty_app = App::empty();
    ensure_mod_lifecycle_flush_schedule(&mut empty_app);
    ensure_mod_lifecycle_flush_schedule(&mut empty_app);
    assert!(
        empty_app
            .world()
            .contains_resource::<PendingModLifecycleOps>()
    );
    let empty_sched = empty_app
        .world()
        .resource::<Schedules>()
        .get(ModLifecycleFlush)
        .expect("ModLifecycleFlush on empty App");
    assert_eq!(empty_sched.systems_len(), 1);
}
