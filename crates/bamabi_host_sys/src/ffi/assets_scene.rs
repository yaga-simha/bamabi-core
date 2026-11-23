//! Cross-ABI asset (`asset_*`) and scene (`cmd_spawn_scene`) host entrypoints delegating to safe
//! [`HostAssetBackend`] and [`HostSceneBackend`] trait hooks on [`crate::ModRegistry`].
//!
//! # Intent
//! `bamabi_host_sys` and headless `bamabi_host` intentionally avoid depending on `bevy_asset` or
//! `bevy_scene` so dedicated servers and simulations stay lightweight. Keeping asset and scene
//! integration behind [`HostAssetBackend`] and [`HostSceneBackend`] trait objects allows Layer 3
//! companion crates (such as `bamabi_bridges`) to plug in live `AssetServer` / `DynamicScene`
//! implementations without altering the 73-slot `BamHostApi` table.
//!
//! # How It Works
//! - Each slot (`asset_load`, `asset_get_load_state`, `asset_release`, and `cmd_spawn_scene`)
//!   validates pointers, alignment, handle headers (`HostContext` / `HostCommands`), and
//!   [`FfiAssetHandle`] wire invariants before dispatching to the backend stored on
//!   [`RegistrySnapshot`].
//! - When no backend is registered on [`crate::ModRegistry`], every slot records a diagnostic on
//!   [`HostContext::last_error`](crate::HostContext::set_last_error) (for `asset_*`) and returns
//!   [`FfiResult::UNSUPPORTED`].
//! - Backend errors `(FfiResult, String)` returned from [`HostAssetBackend`] are propagated
//!   verbatim into `HostContext` `last_error` alongside the `FfiResult` status code.

use bamabi_sys::{
    BamCommands, BamComponent, BamHostCtx, FfiAssetHandle, FfiAssetLoadState, FfiEntity, FfiResult,
    StrView,
};
use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;

use crate::{
    handle_header::{
        HostContext, decode_optional_entity, ffi_try, init_out_ptr, validate_commands_mut,
        validate_host_ctx,
    },
    registry::RegistrySnapshot,
};

/// Safe backend trait implemented in higher-level crates (`#![forbid(unsafe_code)]`) to service
/// `BamHostApi::asset_load`, `asset_get_load_state`, and `asset_release` without adding `bevy_asset`
/// to `bamabi_host_sys`.
///
/// Implementations are installed onto [`crate::ModRegistry`] and read lock-free via
/// [`RegistrySnapshot::asset_backend`]. Returned `Err((code, message))` pairs populate
/// `HostContext`'s per-context `last_error` buffer and return `code` to the guest.
pub trait HostAssetBackend: Send + Sync + 'static {
    /// Initiates asynchronous asset loading for non-empty `path` under pre-resolved `asset_type`
    /// (`BamComponent::INVALID` when untyped loading is requested), retaining a strong handle in
    /// host memory under `owner_namespace` and returning its wire [`FfiAssetHandle`].
    fn load_asset(
        &self,
        snapshot: &RegistrySnapshot,
        owner_namespace: Option<&str>,
        asset_type: BamComponent,
        path: &str,
    ) -> Result<FfiAssetHandle, (FfiResult, String)>;

    /// Queries the current [`FfiAssetLoadState`] of a non-none [`FfiAssetHandle`].
    ///
    /// Note that [`host_asset_get_load_state`] short-circuits [`FfiAssetHandle::NONE`] to
    /// [`FfiAssetLoadState::NOT_LOADED`] (`FfiResult::OK`) before invoking this method.
    fn get_load_state(
        &self,
        snapshot: &RegistrySnapshot,
        handle: &FfiAssetHandle,
    ) -> Result<FfiAssetLoadState, (FfiResult, String)>;

    /// Releases the host-retained strong handle corresponding to a non-none `handle`.
    ///
    /// Note that [`host_asset_release`] treats [`FfiAssetHandle::NONE`] as a successful no-op
    /// (`FfiResult::OK`) without calling this method.
    fn release_asset(
        &self,
        snapshot: &RegistrySnapshot,
        owner_namespace: Option<&str>,
        handle: &FfiAssetHandle,
    ) -> Result<(), (FfiResult, String)>;

    /// Releases all retained strong handles associated with `owner_namespace` when a mod unloads
    /// so asset allocations do not outlive the owning mod session.
    fn release_all_for_owner(&self, owner_namespace: &str) {
        let _ = owner_namespace;
    }
}

/// Safe backend trait implemented in higher-level crates (`#![forbid(unsafe_code)]`) to service
/// deferred `BamHostApi::cmd_spawn_scene` commands without adding `bevy_scene` to
/// `bamabi_host_sys`.
///
/// Installed on [`crate::ModRegistry`] and invoked in two phases: synchronous handle validation
/// when [`host_cmd_spawn_scene`] queues the command, followed by [`Self::spawn_scene`] during
/// deferred [`HostCommandsStorage::apply_to_world`](crate::HostCommandsStorage::apply_to_world).
pub trait HostSceneBackend: Send + Sync + 'static {
    /// Validates `scene_handle` synchronously when `cmd_spawn_scene` is queued.
    ///
    /// Default implementation accepts all structurally valid non-none handles. Returning `Err((code,
    /// message))` causes [`host_cmd_spawn_scene`] to abort queuing and return `code` before
    /// reserving a root entity.
    fn validate_scene_handle(
        &self,
        snapshot: &RegistrySnapshot,
        scene_handle: &FfiAssetHandle,
    ) -> Result<(), (FfiResult, String)> {
        let _ = (snapshot, scene_handle);
        Ok(())
    }

    /// Instantiates `scene_handle` onto the already-spawned `root_entity` in `world` during
    /// deferred command application (`parent` has already been attached via `ChildOf` when `Some`).
    fn spawn_scene(
        &self,
        world: &mut World,
        snapshot: &RegistrySnapshot,
        root_entity: Entity,
        parent: Option<Entity>,
        scene_handle: &FfiAssetHandle,
    ) -> Result<(), FfiResult>;
}

/// Validates a guest-supplied `*const FfiAssetHandle` and active [`HostContext`], resolving the
/// registered [`HostAssetBackend`].
///
/// Returns [`FfiResult::NULL_POINTER`] if `handle` or `ctx` is null, [`FfiResult::INVALID_ARGUMENT`]
/// if `handle` is misaligned or fails [`FfiAssetHandle::validate`], [`FfiResult::INVALID_STATE`] if
/// `ctx` is poisoned/mismatched, or [`FfiResult::UNSUPPORTED`] if no [`HostAssetBackend`] is
/// registered.
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - `handle` must point to a readable [`FfiAssetHandle`], or null.
#[inline]
unsafe fn validate_asset_handle_call<'a>(
    ctx: *const BamHostCtx,
    handle: *const FfiAssetHandle,
    op: &str,
) -> Result<(&'a HostContext, FfiAssetHandle, &'a dyn HostAssetBackend), FfiResult> {
    if handle.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    if !handle.is_aligned() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = unsafe { validate_host_ctx(ctx) }?;
    // SAFETY: `handle` is non-null, aligned, and readable per caller contract.
    let parsed_handle = unsafe { *handle };
    let val = parsed_handle.validate();
    if val.is_err() {
        host_ctx.set_last_error(format!("{op}: invalid FfiAssetHandle ({val})"));
        return Err(val);
    }
    let Some(backend) = host_ctx.snapshot().asset_backend() else {
        host_ctx.set_last_error(format!(
            "{op}: no HostAssetBackend registered on ModRegistry"
        ));
        return Err(FfiResult::UNSUPPORTED);
    };
    Ok((host_ctx, parsed_handle, backend.as_ref()))
}

/// Requests asynchronous asset loading via the registered [`HostAssetBackend`], writing the
/// resulting [`FfiAssetHandle`] into `*out_handle` (`FfiResult::UNSUPPORTED` if no backend is installed).
///
/// Initializes `*out_handle = FfiAssetHandle::NONE` unconditionally once `out_handle` passes
/// non-null and alignment checks.
///
/// # Errors
/// - [`FfiResult::NULL_POINTER`]: `out_handle` or `ctx` is null (or `path.ptr` is null with `path.len > 0`).
/// - [`FfiResult::INVALID_ARGUMENT`]: `out_handle` is misaligned or `path` is empty.
/// - [`FfiResult::INVALID_STATE`]: `ctx` header magic/generation is invalid or poisoned.
/// - [`FfiResult::INVALID_UTF8`]: `path` is not valid UTF-8.
/// - [`FfiResult::UNSUPPORTED`]: No [`HostAssetBackend`] is registered on [`crate::ModRegistry`].
/// - Any backend-supplied error code returned by [`HostAssetBackend::load_asset`] (with message stored in `last_error`).
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - If `path.len > 0` and `!path.ptr.is_null()`, `path` must reference `path.len` readable bytes.
/// - `out_handle` must point to writable [`FfiAssetHandle`] storage, or null.
pub unsafe extern "C" fn host_asset_load(
    ctx: *const BamHostCtx,
    asset_type: BamComponent,
    path: StrView,
    out_handle: *mut FfiAssetHandle,
) -> FfiResult {
    // SAFETY: `out_handle` is validated non-null and aligned before writing `NONE`.
    ffi_try!(unsafe { init_out_ptr(out_handle, FfiAssetHandle::NONE) });
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = ffi_try!(unsafe { validate_host_ctx(ctx) });

    // SAFETY: Caller guarantees `path` points to initialized bytes when `path.len > 0 && !path.ptr.is_null()`.
    let path_str = match unsafe { path.try_as_str() } {
        Ok(s) => s,
        Err(err) => {
            host_ctx.set_last_error(format!("asset_load: invalid path StrView ({err})"));
            return err;
        }
    };
    if path_str.is_empty() {
        host_ctx.set_last_error("asset_load: asset path must not be empty");
        return FfiResult::INVALID_ARGUMENT;
    }

    let snap = host_ctx.snapshot();
    let Some(backend) = snap.asset_backend() else {
        host_ctx.set_last_error("asset_load: no HostAssetBackend registered on ModRegistry");
        return FfiResult::UNSUPPORTED;
    };

    match backend.load_asset(snap, host_ctx.default_namespace(), asset_type, path_str) {
        Ok(handle) => {
            // SAFETY: `out_handle` was verified non-null and aligned above.
            unsafe {
                *out_handle = handle;
            }
            FfiResult::OK
        }
        Err((code, msg)) => {
            host_ctx.set_last_error(msg);
            code
        }
    }
}

/// Queries [`FfiAssetLoadState`] for `handle` via the registered [`HostAssetBackend`]
/// (`FfiResult::UNSUPPORTED` if no backend is installed).
///
/// Initializes `*out_state = FfiAssetLoadState::NOT_LOADED` when both `handle` and `out_state`
/// are non-null and properly aligned. Passing [`FfiAssetHandle::NONE`] returns [`FfiResult::OK`]
/// with `NOT_LOADED` without calling the backend.
///
/// # Errors
/// - [`FfiResult::NULL_POINTER`]: `handle`, `out_state`, or `ctx` is null.
/// - [`FfiResult::INVALID_ARGUMENT`]: `handle` or `out_state` is misaligned, or `*handle` fails [`FfiAssetHandle::validate`].
/// - [`FfiResult::INVALID_STATE`]: `ctx` header magic/generation is invalid or poisoned.
/// - [`FfiResult::UNSUPPORTED`]: No [`HostAssetBackend`] is registered on [`crate::ModRegistry`].
/// - Any backend-supplied error code returned by [`HostAssetBackend::get_load_state`] (with message stored in `last_error`).
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - `handle` must point to a readable [`FfiAssetHandle`], or null.
/// - `out_state` must point to writable [`FfiAssetLoadState`] storage, or null.
pub unsafe extern "C" fn host_asset_get_load_state(
    ctx: *const BamHostCtx,
    handle: *const FfiAssetHandle,
    out_state: *mut FfiAssetLoadState,
) -> FfiResult {
    if handle.is_null() || out_state.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !handle.is_aligned() || !out_state.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_state` is non-null, aligned, and writable.
    unsafe {
        *out_state = FfiAssetLoadState::NOT_LOADED;
    }
    // SAFETY: Validates `ctx`, `handle`, and backend presence under caller contract.
    let (host_ctx, parsed_handle, backend) =
        ffi_try!(unsafe { validate_asset_handle_call(ctx, handle, "asset_get_load_state") });
    if parsed_handle.is_none() {
        return FfiResult::OK;
    }
    match backend.get_load_state(host_ctx.snapshot(), &parsed_handle) {
        Ok(state) => {
            // SAFETY: `out_state` was verified non-null and aligned above.
            unsafe {
                *out_state = state;
            }
            FfiResult::OK
        }
        Err((code, msg)) => {
            host_ctx.set_last_error(msg);
            code
        }
    }
}

/// Releases a host-retained strong asset handle via the registered [`HostAssetBackend`]
/// (`FfiResult::UNSUPPORTED` if no backend is installed).
///
/// Passing [`FfiAssetHandle::NONE`] is a safe no-op returning [`FfiResult::OK`] once `ctx` and
/// the backend are validated.
///
/// # Errors
/// - [`FfiResult::NULL_POINTER`]: `handle` or `ctx` is null.
/// - [`FfiResult::INVALID_ARGUMENT`]: `handle` is misaligned or `*handle` fails [`FfiAssetHandle::validate`].
/// - [`FfiResult::INVALID_STATE`]: `ctx` header magic/generation is invalid or poisoned.
/// - [`FfiResult::UNSUPPORTED`]: No [`HostAssetBackend`] is registered on [`crate::ModRegistry`].
/// - Any backend-supplied error code returned by [`HostAssetBackend::release_asset`] (with message stored in `last_error`).
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - `handle` must point to a readable [`FfiAssetHandle`], or null.
pub unsafe extern "C" fn host_asset_release(
    ctx: *const BamHostCtx,
    handle: *const FfiAssetHandle,
) -> FfiResult {
    // SAFETY: Validates `ctx`, `handle`, and backend presence under caller contract.
    let (host_ctx, parsed_handle, backend) =
        ffi_try!(unsafe { validate_asset_handle_call(ctx, handle, "asset_release") });
    if parsed_handle.is_none() {
        return FfiResult::OK;
    }
    match backend.release_asset(
        host_ctx.snapshot(),
        host_ctx.default_namespace(),
        &parsed_handle,
    ) {
        Ok(()) => FfiResult::OK,
        Err((code, msg)) => {
            host_ctx.set_last_error(msg);
            code
        }
    }
}

/// Reserves a root entity and queues deferred scene instantiation via the registered
/// [`HostSceneBackend`], writing the reserved [`FfiEntity`] into `*out_entity`
/// (`FfiResult::UNSUPPORTED` if no scene backend is installed).
///
/// Initializes `*out_entity = FfiEntity::NULL` once `scene_handle` and `out_entity` pass null
/// and alignment checks. When `parent` is non-null, it is decoded via `decode_optional_entity`
/// and attached as a Bevy `ChildOf` parent before [`HostSceneBackend::spawn_scene`] runs during
/// deferred flush.
///
/// # Errors
/// - [`FfiResult::NULL_POINTER`]: `scene_handle`, `out_entity`, or `commands` is null.
/// - [`FfiResult::INVALID_ARGUMENT`]: `scene_handle` or `out_entity` is misaligned, `*scene_handle`
///   fails [`FfiAssetHandle::validate`], or `*scene_handle` is [`FfiAssetHandle::NONE`].
/// - [`FfiResult::INVALID_STATE`]: `commands` header magic/generation is invalid or poisoned.
/// - [`FfiResult::INVALID_ENTITY`]: `parent` is non-null but does not decode to a valid [`Entity`].
/// - [`FfiResult::UNSUPPORTED`]: No [`HostSceneBackend`] is registered on [`crate::ModRegistry`].
/// - [`FfiResult::CAPACITY_EXCEEDED`] or backend error code from [`HostSceneBackend::validate_scene_handle`].
///
/// # Safety
/// - `commands` must point to an active [`crate::HostCommands`] cast to [`BamCommands`], or null.
/// - `scene_handle` must point to a readable [`FfiAssetHandle`], or null.
/// - `out_entity` must point to writable [`FfiEntity`] storage, or null.
pub unsafe extern "C" fn host_cmd_spawn_scene(
    commands: *mut BamCommands,
    scene_handle: *const FfiAssetHandle,
    parent: FfiEntity,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if scene_handle.is_null() || out_entity.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !scene_handle.is_aligned() || !out_entity.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_entity` is non-null, aligned, and writable.
    unsafe {
        *out_entity = FfiEntity::NULL;
    }
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });

    // SAFETY: `scene_handle` is non-null, aligned, and readable per caller contract.
    let parsed_handle = unsafe { *scene_handle };
    let val = parsed_handle.validate();
    if val.is_err() {
        return val;
    }
    if parsed_handle.is_none() {
        return FfiResult::INVALID_ARGUMENT;
    }

    let parent_entity = ffi_try!(decode_optional_entity(parent));

    match host_cmds.queue_spawn_scene(parsed_handle, parent_entity) {
        Ok(entity) => {
            // SAFETY: `out_entity` was verified non-null and aligned above.
            unsafe {
                *out_entity = FfiEntity::from_bits(entity.to_bits());
            }
            FfiResult::OK
        }
        Err(err) => err,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/assets_scene_tests.rs"]
mod tests;
