//! Extended deferred entity command entrypoints (`cmd_spawn_persistent`, `cmd_spawn_batch`,
//! `cmd_despawn_all`, `cmd_insert_if_new`, `cmd_clone_entity`, `cmd_insert_resource`, and
//! `cmd_remove_resource`).

use bamabi_sys::{BamCommands, BamComponent, BamTermDesc, FfiEntity, FfiResult};

use crate::{
    ffi::commands::spawn_batch_with_scope,
    handle_header::{ffi_try, validate_commands_mut},
};

/// Reserves a fresh entity whose lifecycle persists across mod unload and queues a batched spawn.
///
/// # Safety
/// - `commands` must point to an active [`crate::ffi::commands::HostCommands`] cast to [`BamCommands`], or null.
/// - `out_entity` must point to writable [`FfiEntity`] storage, or null.
/// - When `count > 0`, `components` and `data_ptrs` must each point to `count` readable entries.
pub unsafe extern "C" fn host_cmd_spawn_persistent(
    commands: *mut BamCommands,
    components: *const BamComponent,
    data_ptrs: *const *const u8,
    count: usize,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if out_entity.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    // SAFETY: `cmds` is validated, `out_entity` is non-null and writable, and caller guarantees `components`/`data_ptrs`.
    unsafe { spawn_batch_with_scope(cmds, components, data_ptrs, count, out_entity, true) }
}

/// Reserves `entity_count` fresh entities and queues a bulk SoA spawn across `components[0..component_count]`.
///
/// # Safety
/// - `commands` must point to an active [`crate::ffi::commands::HostCommands`] cast to [`BamCommands`], or null.
/// - When `component_count > 0`, `components` and `column_ptrs` must be non-null and readable.
/// - When `out_entities` is non-null and `entity_count > 0`, `out_entities` must be writable for `entity_count` slots.
pub unsafe extern "C" fn host_cmd_spawn_batch(
    commands: *mut BamCommands,
    components: *const BamComponent,
    column_ptrs: *const *const u8,
    component_count: usize,
    entity_count: usize,
    out_entities: *mut FfiEntity,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    // SAFETY: `host_cmds` is validated and caller guarantees `components`, `column_ptrs`, and
    // optional `out_entities` invariants.
    unsafe {
        host_cmds.spawn_soa_batch_raw(
            components,
            column_ptrs,
            component_count,
            entity_count,
            out_entities,
        )
    }
}

/// Queues deferred bulk despawning (`World::despawn_all`) of non-resource entities matching `terms[0..term_count]`.
///
/// # Safety
/// - `commands` must point to an active [`crate::ffi::commands::HostCommands`] cast to [`BamCommands`], or null.
/// - When `term_count > 0`, `terms` must point to `term_count` readable [`BamTermDesc`] elements.
pub unsafe extern "C" fn host_cmd_despawn_all(
    commands: *mut BamCommands,
    terms: *const BamTermDesc,
    term_count: usize,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    // SAFETY: `host_cmds` is validated and caller guarantees `terms` is readable for `term_count`.
    unsafe { host_cmds.despawn_all_raw(terms, term_count) }
}

/// Queues deferred conditional insertion (`InsertMode::Keep`) of `component` onto `entity` iff absent.
///
/// # Safety
/// - `commands` must point to an active [`crate::ffi::commands::HostCommands`] cast to [`BamCommands`], or null.
/// - When `data_len > 0`, `data_ptr` must point to `data_len` readable bytes.
pub unsafe extern "C" fn host_cmd_insert_if_new(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    // SAFETY: `host_cmds` is validated and caller guarantees `data_ptr` is readable for `data_len`.
    unsafe { host_cmds.insert_raw_inner(entity, component, data_ptr, data_len, true) }
}

/// Queues deferred cloning of all cloneable components from `src_entity` onto `dst_entity`
/// (`EntityCloner::build_opt_out`, respecting `clone_fn` and POD dynamic components).
///
/// # Safety
/// `commands` must point to an active [`crate::ffi::commands::HostCommands`] cast to [`BamCommands`], or null.
pub unsafe extern "C" fn host_cmd_clone_entity(
    commands: *mut BamCommands,
    src_entity: FfiEntity,
    dst_entity: FfiEntity,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    host_cmds.clone_entity_raw(src_entity, dst_entity)
}

/// Queues deferred insertion or replacement of singleton `resource` (`data_len` bytes at `data_ptr`).
///
/// # Safety
/// - `commands` must point to an active [`crate::ffi::commands::HostCommands`] cast to [`BamCommands`], or null.
/// - If `data_len > 0`, `data_ptr` must point to at least `data_len` readable bytes matching the
///   registered layout of `resource`.
pub unsafe extern "C" fn host_cmd_insert_resource(
    commands: *mut BamCommands,
    resource: BamComponent,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    // SAFETY: `host_cmds` is validated and caller guarantees `data_ptr` is readable for `data_len`.
    unsafe { host_cmds.insert_resource_raw(resource, data_ptr, data_len) }
}

/// Queues deferred removal of singleton `resource` from the `World`.
///
/// # Safety
/// `commands` must point to an active [`crate::ffi::commands::HostCommands`] cast to [`BamCommands`], or null.
pub unsafe extern "C" fn host_cmd_remove_resource(
    commands: *mut BamCommands,
    resource: BamComponent,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    host_cmds.remove_resource_raw(resource)
}

#[cfg(test)]
#[path = "../../tests/unit/commands_ext_tests.rs"]
mod tests;
