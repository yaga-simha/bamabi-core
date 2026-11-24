//! RAII `LoadTransaction` rolling back drop trampolines and `ModRegistry` snapshot before `dlclose`.

use crate::drop_trampoline::{StagedDropBinding, WorldDropGuard, prune_drop_trampolines_for_guard};
use crate::loader::schema_migration::{
    StagedColumnMigrationUndo, rollback_staged_column_migrations,
};
use crate::registry::{ModRegistry, RegistrySnapshot};
use bevy_ecs::world::World;
use core::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

/// RAII transaction guard bound after opening a [`crate::loader::shared_lib::LoadedLibrary`] so that
/// if `bamabi_init` fails (and `commit` is not reached), all drop trampoline slots (including
/// freshly allocated slots returned to `FREE_LIST`), staged column migrations, and `ModRegistry`
/// snapshot mutations are rolled back **strictly before** `LoadedLibrary::drop` (`dlclose`).
pub(crate) struct LoadTransaction {
    world_ptr: *mut World,
    is_active: Arc<AtomicBool>,
    pre_snapshot: Arc<RegistrySnapshot>,
    staged_drop_bindings: Arc<Mutex<Vec<StagedDropBinding>>>,
    staged_migrations: Arc<Mutex<Vec<StagedColumnMigrationUndo>>>,
    committed: bool,
}

impl LoadTransaction {
    /// Starts a new rollback guard restoring `pre_snapshot` on failure.
    ///
    /// Declare this binding *after* `LoadedLibrary::open(...)` so Rust's reverse local drop order
    /// guarantees `LoadTransaction::drop` executes prior to `dlclose`.
    pub(crate) fn new(
        world_ptr: *mut World,
        is_active: Arc<AtomicBool>,
        pre_snapshot: Arc<RegistrySnapshot>,
    ) -> Self {
        Self {
            world_ptr,
            is_active,
            pre_snapshot,
            staged_drop_bindings: Arc::new(Mutex::new(Vec::new())),
            staged_migrations: Arc::new(Mutex::new(Vec::new())),
            committed: false,
        }
    }

    /// Returns a shared handle to the staged drop-binding log for `LoadContext`.
    #[inline]
    pub(crate) fn staged_drop_bindings(&self) -> Arc<Mutex<Vec<StagedDropBinding>>> {
        Arc::clone(&self.staged_drop_bindings)
    }

    /// Returns a shared handle to the staged `migrate_component_column` undo log for `LoadContext`.
    #[inline]
    pub(crate) fn staged_migrations(&self) -> Arc<Mutex<Vec<StagedColumnMigrationUndo>>> {
        Arc::clone(&self.staged_migrations)
    }

    /// Marks `bamabi_init` as succeeded, disarming automatic rollback and returning the staged
    /// drop bindings and column migration undo records so `rollback_unscheduled_mod_in_world`
    /// can still revert them if post-init validation or cascade staging fails before commit.
    #[inline]
    pub(crate) fn commit(mut self) -> (Vec<StagedDropBinding>, Vec<StagedColumnMigrationUndo>) {
        self.committed = true;
        let bindings = {
            let mut guard = self
                .staged_drop_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };
        let migrations = {
            let mut guard = self
                .staged_migrations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };
        (bindings, migrations)
    }
}

impl Drop for LoadTransaction {
    fn drop(&mut self) {
        if self.committed {
            return;
        }

        let bindings = {
            let mut guard = self
                .staged_drop_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };
        let migrations = {
            let mut guard = self
                .staged_migrations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };

        // 1. Reverse any staged `migrate_component_column` swaps, roll back staged drop bindings in
        //    `WorldDropGuard` (returning fresh slots to `FREE_LIST` and restoring `previous_snapshot`
        //    on hot-updated slots), and restore `ModRegistry`.
        if !self.world_ptr.is_null() {
            // SAFETY: `self.world_ptr` comes from the exclusive `&mut World` passed into `load_mod_core`
            // and no inner `&mut World` alias is live while `LoadTransaction` drops.
            let world = unsafe { &mut *self.world_ptr };
            if !migrations.is_empty() {
                rollback_staged_column_migrations(world, migrations);
            }
            if let Some(mut drop_guard) = world.get_resource_mut::<WorldDropGuard>() {
                drop_guard.rollback_staged_bindings(&bindings);
            }
            if let Some(mut reg) = world.get_resource_mut::<ModRegistry>() {
                reg.restore_snapshot(Arc::clone(&self.pre_snapshot));
            }
        }

        // 2. Disarm `is_active = false` and prune any remaining references before `dlclose`.
        prune_drop_trampolines_for_guard(&self.is_active);
    }
}

#[cfg(test)]
#[path = "../../tests/unit/transaction_tests.rs"]
mod tests;
