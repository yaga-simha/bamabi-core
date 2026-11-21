//! Deferred [`ModCommandsExt`] helpers on Bevy [`Commands`] for parallel systems.

use crate::plugin::ModLifecycleFlush;
use crate::schedule_sets::leased_schedules;
use crate::world_ext::ModWorldExt;
use bamabi_host_sys::{LoadOptions, ModId};
use bevy_app::Main;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::ScheduleLabel;
use bevy_ecs::system::Commands;
use bevy_ecs::world::World;
use std::path::Path;

type DeferredLifecycleFn = Box<dyn FnOnce(&mut World) + Send + Sync>;

/// Internal queue holding deferred [`ModCommandsExt`] operations that flushed while a frame or
/// mod schedule (or `Main` outside [`ModLifecycleFlush`]) was leased out of
/// [`Schedules`](bevy_ecs::schedule::Schedules), replaying them in [`ModLifecycleFlush`] once all
/// active schedules have returned to `Schedules`.
#[derive(Resource, Default)]
pub(crate) struct PendingModLifecycleOps {
    queue: Vec<DeferredLifecycleFn>,
}

fn is_safe_to_run_lifecycle_op_now(world: &World) -> bool {
    let leased = leased_schedules(world);
    let main_label = Main.intern();
    let flush_label = ModLifecycleFlush.intern();
    let only_flush_or_main = leased
        .iter()
        .all(|&label| label == flush_label || label == main_label);
    let inside_flush_or_outside_main =
        leased.contains(&flush_label) || !leased.contains(&main_label);
    only_flush_or_main && inside_flush_or_outside_main
}

pub(crate) fn flush_pending_mod_lifecycle_ops(world: &mut World) {
    let ops: Vec<DeferredLifecycleFn> = world
        .get_resource_mut::<PendingModLifecycleOps>()
        .map(|mut pending| core::mem::take(&mut pending.queue))
        .unwrap_or_default();
    if ops.is_empty() {
        return;
    }

    let main_label = Main.intern();
    let flush_label = ModLifecycleFlush.intern();
    let mut iter = ops.into_iter();
    while let Some(op) = iter.next() {
        let leased = leased_schedules(world);
        if let Some(blocking) = leased
            .iter()
            .copied()
            .find(|&label| label != main_label && label != flush_label)
        {
            tracing::warn!(
                schedule = ?blocking,
                remaining_ops = iter.len() + 1,
                "deferring remaining pending mod lifecycle ops because a mod schedule is currently leased"
            );
            let mut requeued = Vec::with_capacity(iter.len() + 1);
            requeued.push(op);
            requeued.extend(iter);
            if let Some(mut pending) = world.get_resource_mut::<PendingModLifecycleOps>() {
                requeued.extend(core::mem::take(&mut pending.queue));
                pending.queue = requeued;
            }
            return;
        }
        op(world);
    }
}

fn run_or_defer_lifecycle_op<F>(world: &mut World, op: F)
where
    F: FnOnce(&mut World) + Send + Sync + 'static,
{
    if world.contains_resource::<PendingModLifecycleOps>()
        && !is_safe_to_run_lifecycle_op_now(world)
    {
        if let Some(mut pending) = world.get_resource_mut::<PendingModLifecycleOps>() {
            pending.queue.push(Box::new(op));
        }
        return;
    }
    op(world);
}

/// Deferred mod load, unload, and reload commands usable from any normal parallel Bevy system.
///
/// Queued operations execute during an exclusive `&mut World` point once active schedules are
/// resident in [`Schedules`](bevy_ecs::schedule::Schedules), preserving the safety invariant that
/// shared libraries are never opened or unmapped while worker threads run schedule systems.
pub trait ModCommandsExt {
    /// Queues a deferred [`ModWorldExt::load_mod`] operation with [`LoadOptions::default()`].
    fn load_mod<P: AsRef<Path>>(&mut self, path: P);

    /// Queues a deferred [`ModWorldExt::load_mod_with`] operation with explicit [`LoadOptions`].
    fn load_mod_with<P: AsRef<Path>>(&mut self, path: P, options: LoadOptions);

    /// Queues a deferred [`ModWorldExt::unload_mod`] operation for `id`.
    fn unload_mod(&mut self, id: ModId);

    /// Queues a deferred [`ModWorldExt::reload_mod`] operation for `id`.
    fn reload_mod(&mut self, id: ModId);

    /// Queues a deferred [`ModWorldExt::reload_mod_with`] operation for `id`.
    fn reload_mod_with(&mut self, id: ModId, options: LoadOptions);
}

fn queue_mod_lifecycle<F>(commands: &mut Commands<'_, '_>, op: F)
where
    F: FnOnce(&mut World) + Send + Sync + 'static,
{
    commands.queue(move |world: &mut World| run_or_defer_lifecycle_op(world, op));
}

impl ModCommandsExt for Commands<'_, '_> {
    fn load_mod<P: AsRef<Path>>(&mut self, path: P) {
        self.load_mod_with(path, LoadOptions::default());
    }

    fn load_mod_with<P: AsRef<Path>>(&mut self, path: P, options: LoadOptions) {
        let path_buf = path.as_ref().to_path_buf();
        queue_mod_lifecycle(self, move |w| {
            if let Err(err) = w.load_mod_with(&path_buf, options) {
                tracing::error!(path = %path_buf.display(), error = %err, "deferred Commands::load_mod_with failed");
            }
        });
    }

    fn unload_mod(&mut self, id: ModId) {
        queue_mod_lifecycle(self, move |w| {
            if let Err(err) = w.unload_mod(id) {
                tracing::error!(mod_id = ?id, error = %err, "deferred Commands::unload_mod failed");
            }
        });
    }

    fn reload_mod(&mut self, id: ModId) {
        self.reload_mod_with(id, LoadOptions::default().with_shadow_copy(true));
    }

    fn reload_mod_with(&mut self, id: ModId, options: LoadOptions) {
        queue_mod_lifecycle(self, move |w| {
            if let Err(err) = w.reload_mod_with(id, options) {
                tracing::error!(mod_id = ?id, error = %err, "deferred Commands::reload_mod_with failed");
            }
        });
    }
}

#[cfg(test)]
#[path = "../tests/unit/commands_ext_tests.rs"]
mod tests;
