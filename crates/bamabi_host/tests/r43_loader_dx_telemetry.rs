#![allow(unreachable_pub)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Integration tests for Bamabi `0.4.0` R4.3 deliverables:
//! - Static / in-memory loading (`InMemoryModEntrypoints`, `ModRegistryAppExt`, `ModCommandsExt`)
//! - Atomic non-destructive hot-reload preserving `ModScoped` & active systems on `bamabi_init` failure
//! - `LoadOptions::with_system_filter` & `ModLoadFailed` diagnostics
//! - `HeadlessModTestHarness`, POD bundle/hash/table helpers, & field-level introspection helpers
//! - Headless `ModTelemetrySinkPlugin`, `ModTelemetryRing`, & native system telemetry wrappers
//! - `.bampack` direct loading & reloading (`load_bampack` / `reload_bampack`)

#[cfg(all(feature = "dynamic-loader", feature = "signature-verification"))]
#[path = "support/helpers.rs"]
mod support;

use bamabi_host::prelude::*;
use bamabi_host::{
    HeadlessModTestHarness, ModLoadFailed, ModTelemetryRing, ModTelemetrySinkPlugin,
    with_native_system_telemetry, wrap_native_system,
};
use bamabi_host_sys::{
    BamFieldBytePatch, InMemoryModEntrypoints, ModScoped, inspect_component_bam_fields,
    read_entity_bam_field_bytes, read_resource_bam_field_bytes, write_entity_bam_field_bytes,
    write_resource_bam_field_bytes,
};
use bamabi_macros::ModComponent;
use bamabi_sys::{
    BamComponent, BamHostApi, BamInstance, BamLoadCtx, BamManifest, BamSystemCtx, BamSystemDesc,
    FfiResult, SYSTEM_TELEMETRY_MAGIC, ScheduleId,
};
use bevy_ecs::component::Component;
use bevy_ecs::prelude::Resource;
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "r43_test::ProbePod")]
struct ProbePod {
    ticks: u32,
    tag: u32,
}

#[repr(C)]
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "r43_test::ProbeRes")]
struct ProbeRes {
    ticks: u32,
    tag: u32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "r43_test::MarkerPod")]
struct MarkerPod {
    code: u64,
}

static V1_INIT_COUNT: AtomicU64 = AtomicU64::new(0);
static V1_TICKS: AtomicU64 = AtomicU64::new(0);
static V1_FILTERED_TICKS: AtomicU64 = AtomicU64::new(0);
static V2_TICKS: AtomicU64 = AtomicU64::new(0);
static V2_SHOULD_FAIL_INIT: AtomicBool = AtomicBool::new(false);

static MANIFEST_V1: BamManifest = BamManifest::new(c"r43_inmem_mod".as_ptr(), c"0.4.0".as_ptr());

static MANIFEST_V2: BamManifest = BamManifest::new(c"r43_inmem_mod".as_ptr(), c"0.4.1".as_ptr());

unsafe extern "C" fn v1_manifest() -> *const BamManifest {
    &raw const MANIFEST_V1
}

unsafe extern "C" fn v2_manifest() -> *const BamManifest {
    &raw const MANIFEST_V2
}

unsafe extern "C" fn v1_kept_sys(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
    V1_TICKS.fetch_add(1, Ordering::SeqCst);
    FfiResult::OK
}

unsafe extern "C" fn v1_filtered_sys(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
    V1_FILTERED_TICKS.fetch_add(1, Ordering::SeqCst);
    FfiResult::OK
}

unsafe extern "C" fn v2_kept_sys(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
    V2_TICKS.fetch_add(1, Ordering::SeqCst);
    FfiResult::OK
}

unsafe extern "C" fn v1_init(
    api: *const BamHostApi,
    load_ctx: *mut BamLoadCtx,
    instance_out: *mut *mut BamInstance,
) -> FfiResult {
    V1_INIT_COUNT.fetch_add(1, Ordering::SeqCst);
    // SAFETY: Valid host pointers for `api`, `load_ctx`, and `instance_out`.
    unsafe {
        *instance_out = core::ptr::null_mut();
        let reg_sys = (*api).register_system.expect("register_system wired");
        let d_kept = BamSystemDesc::new(
            c"kept_tick".as_ptr(),
            core::ptr::null(),
            0,
            v1_kept_sys,
            core::ptr::null_mut(),
        );
        let rc = reg_sys(load_ctx, &raw const d_kept);
        if rc.is_err() {
            return rc;
        }
        let d_skip = BamSystemDesc::new(
            c"skipped_tick".as_ptr(),
            core::ptr::null(),
            0,
            v1_filtered_sys,
            core::ptr::null_mut(),
        );
        reg_sys(load_ctx, &raw const d_skip)
    }
}

unsafe extern "C" fn v2_init(
    api: *const BamHostApi,
    load_ctx: *mut BamLoadCtx,
    instance_out: *mut *mut BamInstance,
) -> FfiResult {
    if V2_SHOULD_FAIL_INIT.load(Ordering::SeqCst) {
        return FfiResult::INVALID_STATE;
    }
    // SAFETY: Valid host pointers for `api`, `load_ctx`, and `instance_out`.
    unsafe {
        *instance_out = core::ptr::null_mut();
        let reg_sys = (*api).register_system.expect("register_system wired");
        let d_kept = BamSystemDesc::new(
            c"kept_tick".as_ptr(),
            core::ptr::null(),
            0,
            v2_kept_sys,
            core::ptr::null_mut(),
        );
        reg_sys(load_ctx, &raw const d_kept)
    }
}

unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}

#[test]
fn in_memory_load_atomic_failed_reload_preserves_entities_and_succeeds_on_recovery() {
    V1_INIT_COUNT.store(0, Ordering::SeqCst);
    V1_TICKS.store(0, Ordering::SeqCst);
    V1_FILTERED_TICKS.store(0, Ordering::SeqCst);
    V2_TICKS.store(0, Ordering::SeqCst);
    V2_SHOULD_FAIL_INIT.store(true, Ordering::SeqCst);

    let mut harness = HeadlessModTestHarness::new();
    let probe_h: BamComponent = harness
        .app_mut()
        .register_mod_component::<ProbePod>(ComponentOptions::default())
        .expect("register ProbePod");
    let ep_v1 = InMemoryModEntrypoints::new(v1_manifest, v1_init, noop_shutdown);
    let filter_opts = LoadOptions::default().with_system_filter(|name| name == "kept_tick");

    let mod_v1 = harness
        .load_mod_from_entrypoints_with("static://r43_inmem", ep_v1, filter_opts.clone())
        .expect("in-memory v1 load succeeds");

    // Spawn a `ModScoped(mod_v1)` entity to verify non-destructive reload rollback keeps it alive.
    let mut pod_bytes = [0u8; core::mem::size_of::<ProbePod>()];
    pod_bytes[0..4].copy_from_slice(&42u32.to_le_bytes());
    pod_bytes[4..8].copy_from_slice(&7u32.to_le_bytes());
    let scoped_ent = harness
        .spawn_pod_bundle(&[(probe_h, &pod_bytes)])
        .expect("spawn POD bundle");
    harness
        .world_mut()
        .entity_mut(scoped_ent)
        .insert(ModScoped(mod_v1));

    harness.step();
    assert_eq!(V1_TICKS.load(Ordering::SeqCst), 1);
    assert_eq!(V1_FILTERED_TICKS.load(Ordering::SeqCst), 0);

    // Attempt reload where v2 `bamabi_init` fails: MUST fail atomically WITHOUT despawning
    // `scoped_ent`, emitting `ModLoadFailed`, while keeping v1 running!
    let ep_v2_fail = InMemoryModEntrypoints::new(v2_manifest, v2_init, noop_shutdown);
    let reload_err = harness
        .reload_mod_from_entrypoints(
            mod_v1,
            "static://r43_inmem_v2",
            ep_v2_fail,
            filter_opts.clone(),
        )
        .expect_err("v2 failing init must return error");
    assert!(matches!(reload_err, ModLoadError::InitFailed { .. }));

    let failed_msgs: Vec<ModLoadFailed> = harness.drain_load_failed_events();
    assert_eq!(failed_msgs.len(), 1);
    assert_eq!(failed_msgs[0].reload_target, Some(mod_v1));

    assert!(
        harness.world().entities().contains(scoped_ent),
        "ModScoped entity must NOT be despawned on failed reload"
    );
    assert_eq!(
        harness.world().get::<ProbePod>(scoped_ent),
        Some(&ProbePod { ticks: 42, tag: 7 })
    );

    harness.step();
    assert_eq!(
        V1_TICKS.load(Ordering::SeqCst),
        2,
        "v1 systems must stay live after failed reload"
    );
    assert_eq!(V2_TICKS.load(Ordering::SeqCst), 0);

    // Now allow v2 init to succeed and reload atomically.
    V2_SHOULD_FAIL_INIT.store(false, Ordering::SeqCst);
    let ep_v2_ok = InMemoryModEntrypoints::new(v2_manifest, v2_init, noop_shutdown);
    let mod_v2 = harness
        .reload_mod_from_entrypoints(mod_v1, "static://r43_inmem_v2", ep_v2_ok, filter_opts)
        .expect("v2 reload succeeds cleanly");
    assert_ne!(mod_v2, mod_v1);
    assert_eq!(
        harness.world().get::<ModScoped>(scoped_ent),
        Some(&ModScoped(mod_v2)),
        "ModScoped entity must migrate cleanly to replacement ModId once reload succeeds"
    );

    harness.step();
    assert_eq!(V1_TICKS.load(Ordering::SeqCst), 2);
    assert_eq!(V2_TICKS.load(Ordering::SeqCst), 1);
}

#[test]
fn harness_field_introspection_and_headless_telemetry_sink_work() {
    let emitted_batches: Arc<Mutex<Vec<usize>>> = Arc::new(Mutex::new(Vec::new()));
    let emitted_clone = Arc::clone(&emitted_batches);

    let mut harness = HeadlessModTestHarness::new();
    harness
        .app_mut()
        .add_plugins(
            ModTelemetrySinkPlugin::new()
                .with_capacity(64)
                .with_sink(move |batch| {
                    emitted_clone.lock().unwrap().push(batch.len());
                }),
        );

    let probe_h = harness
        .app_mut()
        .register_mod_component::<ProbePod>(ComponentOptions::default())
        .expect("register ProbePod component");
    let marker_h = harness
        .app_mut()
        .register_mod_component::<MarkerPod>(ComponentOptions::default())
        .expect("register MarkerPod component");
    harness
        .world_mut()
        .insert_resource(ProbeRes { ticks: 5, tag: 9 });

    let probe_res_h = harness
        .world_mut()
        .register_mod_resource::<ProbeRes>()
        .expect("register ProbeRes resource");

    let inspected = inspect_component_bam_fields(harness.world(), probe_h).expect("inspect fields");
    if let Some(fields) = inspected {
        assert!(fields.iter().any(|f| f.name == "ticks"));
        assert!(fields.iter().any(|f| f.name == "tag"));
    }

    let mut pod_buf = [0u8; 8];
    pod_buf[0..4].copy_from_slice(&11u32.to_le_bytes());
    pod_buf[4..8].copy_from_slice(&22u32.to_le_bytes());
    let ent = harness
        .spawn_pod_bundle(&[(probe_h, &pod_buf), (marker_h, &99u64.to_le_bytes())])
        .expect("spawn POD bundle");

    let read_ticks =
        read_entity_bam_field_bytes(harness.world(), ent, probe_h, 0, 4).expect("read field");
    assert_eq!(read_ticks, 11u32.to_le_bytes());
    write_entity_bam_field_bytes(
        harness.world_mut(),
        ent,
        probe_h,
        &[BamFieldBytePatch {
            offset: 0,
            bytes: 33u32.to_le_bytes().to_vec(),
        }],
    )
    .expect("write entity field");
    assert_eq!(harness.world().get::<ProbePod>(ent).unwrap().ticks, 33);

    let res_tag =
        read_resource_bam_field_bytes(harness.world(), probe_res_h, 4, 4).expect("read res field");
    assert_eq!(res_tag, 9u32.to_le_bytes());
    write_resource_bam_field_bytes(
        harness.world_mut(),
        probe_res_h,
        &[BamFieldBytePatch {
            offset: 4,
            bytes: 77u32.to_le_bytes().to_vec(),
        }],
    )
    .expect("write res field");
    assert_eq!(harness.world().resource::<ProbeRes>().tag, 77);

    let hash_before = harness.pod_state_hash();
    let cols = harness.read_table_column(probe_h).expect("read column");
    assert_eq!(cols.len(), 1);
    assert_eq!(cols[0].0, ent);

    // Instrument native Bevy systems via both `wrap_native_system` & `with_native_system_telemetry`.
    let native_runs = Arc::new(AtomicU64::new(0));
    let native_runs_clone = Arc::clone(&native_runs);
    harness.app_mut().add_systems(
        bevy_app::Update,
        wrap_native_system(
            "native_game",
            "step_counter",
            ScheduleId::UPDATE,
            move || {
                native_runs_clone.fetch_add(1, Ordering::SeqCst);
            },
        ),
    );

    harness.step_n(2);
    assert_eq!(native_runs.load(Ordering::SeqCst), 2);
    assert_eq!(harness.pod_state_hash(), hash_before);

    with_native_system_telemetry(
        harness.world_mut(),
        "native_game",
        "manual_step",
        ScheduleId::UPDATE,
        |_w: &mut World| {},
    );

    let digests = harness.telemetry_digests();
    assert!(digests.len() >= 3);
    for d in &digests {
        assert_eq!(d.magic, SYSTEM_TELEMETRY_MAGIC);
    }
    assert!(!emitted_batches.lock().unwrap().is_empty());
    assert!(harness.world().contains_resource::<ModTelemetryRing>());

    // Also verify `ModCommandsExt` can enqueue an in-memory entrypoint load via `Commands`.
    static CMD_MANIFEST: BamManifest =
        BamManifest::new(c"r43_cmd_inmem_mod".as_ptr(), c"0.4.0".as_ptr());
    unsafe extern "C" fn cmd_manifest() -> *const BamManifest {
        &raw const CMD_MANIFEST
    }
    unsafe extern "C" fn cmd_init(
        _api: *const BamHostApi,
        _load_ctx: *mut BamLoadCtx,
        instance_out: *mut *mut BamInstance,
    ) -> FfiResult {
        // SAFETY: Valid `instance_out` pointer from host.
        unsafe { *instance_out = core::ptr::null_mut() };
        FfiResult::OK
    }
    let ep_cmd = InMemoryModEntrypoints::new(cmd_manifest, cmd_init, noop_shutdown);
    let ep_slot = Arc::new(Mutex::new(Some(ep_cmd)));
    let ep_slot_sys = Arc::clone(&ep_slot);
    harness.app_mut().add_systems(
        bevy_app::Update,
        move |mut commands: bevy_ecs::system::Commands<'_, '_>| {
            if let Some(ep) = ep_slot_sys.lock().unwrap().take() {
                commands.load_mod_from_entrypoints(Path::new("static://cmd_loaded"), ep);
            }
        },
    );
    harness.step();
    assert_eq!(harness.loaded_mods().len(), 1);
}

#[cfg(all(feature = "dynamic-loader", feature = "signature-verification"))]
#[test]
fn load_and_reload_bampack_and_non_destructive_dynamic_reload() {
    use bamabi_host_sys::{BamPackBuilder, ModTrustPolicy};

    let mut harness = HeadlessModTestHarness::with_trust_policy(ModTrustPolicy::disabled());
    let alpha_path = support::fixture_dylib("mod_alpha");
    let beta_path = support::fixture_dylib("mod_beta");
    let fail_init_path = support::fixture_dylib("init_rollback_mod");

    let alpha_bytes = std::fs::read(&alpha_path).expect("read mod_alpha");
    let beta_bytes = std::fs::read(&beta_path).expect("read mod_beta");

    let tmp_dir = std::env::temp_dir().join(format!("bam_r43_bampack_{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");
    let pack_path = tmp_dir.join("test_mod.bampack");

    let pack_v1 = BamPackBuilder::new()
        .add_entry(
            format!("native/{}", support::dylib_filename("mod_alpha")),
            alpha_bytes,
        )
        .expect("add entry")
        .build_unsigned()
        .expect("build unsigned bampack v1");
    std::fs::write(&pack_path, pack_v1).expect("write bampack v1");

    // Note: `mod_alpha` resolves `OrderTraceRes`, so test `init_rollback_mod` vs non-failing
    // `ordered_sets_mod` / `vtable_eviction_mod` packs directly without needing extra host res.
    let vtable_path = support::fixture_dylib("vtable_eviction_mod");
    let ordered_path = support::fixture_dylib("ordered_sets_mod");
    let vtable_bytes = std::fs::read(&vtable_path).expect("read vtable_eviction_mod");
    let ordered_bytes = std::fs::read(&ordered_path).expect("read ordered_sets_mod");

    let pack_v1_clean = BamPackBuilder::new()
        .add_entry(
            format!("bin/{}", support::dylib_filename("vtable_eviction_mod")),
            vtable_bytes,
        )
        .expect("add vtable entry")
        .build_unsigned()
        .expect("build pack v1");
    std::fs::write(&pack_path, pack_v1_clean).expect("write pack v1");

    let mod_id = harness
        .app_mut()
        .load_bampack(&pack_path)
        .expect("load_bampack v1 succeeds");

    // Attempt non-destructive dynamic reload with `init_rollback_mod` -> must fail without dropping `mod_id`!
    let fail_copy = support::TempDylibCopy::new(&fail_init_path, "r43_fail_reload");
    let reload_err = harness
        .app_mut()
        .reload_mod_from(mod_id, fail_copy.path(), LoadOptions::default())
        .expect_err("init_rollback_mod reload must fail");
    assert!(matches!(reload_err, ModLoadError::InitFailed { .. }));
    assert!(
        harness.loaded_mods().iter().any(|m| m.id == mod_id),
        "existing dynamic mod must survive failed reload"
    );

    // Replace `.bampack` on disk and `reload_bampack` cleanly.
    let pack_v2_clean = BamPackBuilder::new()
        .add_entry(
            format!("bin/{}", support::dylib_filename("ordered_sets_mod")),
            ordered_bytes,
        )
        .expect("add ordered entry")
        .build_unsigned()
        .expect("build pack v2");
    std::fs::write(&pack_path, pack_v2_clean).expect("write pack v2");

    let mod_v2 = harness
        .app_mut()
        .reload_bampack(mod_id, &pack_path, LoadOptions::default())
        .expect("reload_bampack v2 succeeds");
    assert_ne!(mod_v2, mod_id);
    assert_eq!(harness.loaded_mods().len(), 1);

    let _ = beta_bytes;
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

#[test]
fn telemetry_sink_flushes_guest_digests_and_ring_buffer_methods() {
    use bamabi_host::DEFAULT_TELEMETRY_RING_CAPACITY;

    static TEL_TICKS: AtomicU64 = AtomicU64::new(0);
    static TEL_MANIFEST: BamManifest =
        BamManifest::new(c"r43_tel_guest".as_ptr(), c"0.4.0".as_ptr());
    unsafe extern "C" fn tel_manifest() -> *const BamManifest {
        &raw const TEL_MANIFEST
    }
    unsafe extern "C" fn tel_sys(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        TEL_TICKS.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    }
    unsafe extern "C" fn tel_init(
        api: *const BamHostApi,
        load_ctx: *mut BamLoadCtx,
        instance_out: *mut *mut BamInstance,
    ) -> FfiResult {
        // SAFETY: Valid host pointers for `api`, `load_ctx`, and `instance_out`.
        unsafe {
            *instance_out = core::ptr::null_mut();
            let reg_sys = (*api).register_system.expect("register_system wired");
            let desc = BamSystemDesc::new(
                c"tel_tick".as_ptr(),
                core::ptr::null(),
                0,
                tel_sys,
                core::ptr::null_mut(),
            );
            reg_sys(load_ctx, &raw const desc)
        }
    }

    TEL_TICKS.store(0, Ordering::SeqCst);
    let emitted_batches: Arc<Mutex<Vec<Vec<bamabi_sys::SystemTelemetryDigest64>>>> =
        Arc::new(Mutex::new(Vec::new()));
    let emitted_clone = Arc::clone(&emitted_batches);

    // Exercise `ModTelemetrySinkPlugin::default()` and `Debug`.
    let default_plugin = ModTelemetrySinkPlugin::default();
    let dbg_str = format!("{default_plugin:?}");
    assert!(dbg_str.contains("ModTelemetrySinkPlugin"));

    // Exercise `HeadlessModTestHarness::default()` and `with_telemetry_capacity`.
    let mut harness = HeadlessModTestHarness::default().with_telemetry_capacity(4);
    assert!(
        harness
            .app()
            .world()
            .contains_resource::<ModTelemetryRing>()
    );
    // Override sink plugin with custom callback + capacity to test guest digest flushing.
    let mut app = bevy_app::App::new();
    app.add_plugins((
        bamabi_host::AbiModPlugin,
        ModTelemetrySinkPlugin::new()
            .with_capacity(4)
            .with_sink(move |batch| {
                emitted_clone.lock().unwrap().push(batch.to_vec());
            }),
    ));

    let ep_v1 = InMemoryModEntrypoints::new(tel_manifest, tel_init, noop_shutdown);
    let mod_id = app
        .load_mod_from_entrypoints("static://telemetry_guest", ep_v1)
        .expect("load telemetry guest mod");
    app.update();
    app.update();
    assert_eq!(TEL_TICKS.load(Ordering::SeqCst), 2);
    assert!(
        !emitted_batches.lock().unwrap().is_empty(),
        "flush_mod_telemetry_digests must emit guest system digests"
    );
    let ring_snap = app.world().resource::<ModTelemetryRing>().snapshot();
    assert!(!ring_snap.is_empty());
    assert!(
        ring_snap
            .iter()
            .all(|d| d.magic == SYSTEM_TELEMETRY_MAGIC && d.mod_id == mod_id.0)
    );
    app.unload_mod(mod_id).expect("unload telemetry guest mod");

    // Exercise `ModTelemetryRing` accessors & eviction (`> capacity`).
    let ring = ModTelemetryRing::default();
    assert_eq!(ring.capacity(), DEFAULT_TELEMETRY_RING_CAPACITY);
    assert!(ring.is_empty());
    assert_eq!(ring.len(), 0);

    let mut small_ring = ModTelemetryRing::with_capacity(3);
    assert_eq!(small_ring.capacity(), 3);
    for seq in 0..5u64 {
        let mut d = ring_snap[0];
        d.frame_seq = seq;
        small_ring.push(d);
    }
    assert_eq!(small_ring.len(), 3);
    assert!(!small_ring.is_empty());
    let seqs: Vec<u64> = small_ring.iter().map(|d| d.frame_seq).collect();
    assert_eq!(seqs, vec![2, 3, 4]);
    let drained = small_ring.drain();
    assert_eq!(drained.len(), 3);
    assert!(small_ring.is_empty());
    small_ring.push(ring_snap[0]);
    assert_eq!(small_ring.len(), 1);
    small_ring.clear();
    assert!(small_ring.is_empty());

    // Exercise `NativeLatencyWindow` > 32 samples wraparound (`WINDOW_SIZE == 32`).
    for i in 0..40u64 {
        with_native_system_telemetry(
            harness.world_mut(),
            "native_wrap",
            "tick",
            ScheduleId::UPDATE,
            |_w: &mut World| {
                let _ = i;
            },
        );
    }
    let wrapped_digests = harness.telemetry_digests();
    assert_eq!(wrapped_digests.len(), 4);
}

#[test]
fn headless_harness_builders_events_and_object_loader_roundtrip() {
    use bamabi_host::{ModAppExt, ModBudgetPolicy, ModRegistryAppExt, ModWorldExt};
    use bamabi_host_sys::InMemoryObjectLoader;

    static OBJ_V1_TICKS: AtomicU64 = AtomicU64::new(0);
    static OBJ_V2_TICKS: AtomicU64 = AtomicU64::new(0);
    static OBJ_M1: BamManifest = BamManifest::new(c"r43_obj_mod".as_ptr(), c"0.4.0".as_ptr());
    static OBJ_M2: BamManifest = BamManifest::new(c"r43_obj_mod".as_ptr(), c"0.4.1".as_ptr());

    unsafe extern "C" fn obj_m1() -> *const BamManifest {
        &raw const OBJ_M1
    }
    unsafe extern "C" fn obj_m2() -> *const BamManifest {
        &raw const OBJ_M2
    }
    unsafe extern "C" fn obj_sys1(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        OBJ_V1_TICKS.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    }
    unsafe extern "C" fn obj_sys2(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        OBJ_V2_TICKS.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    }
    unsafe extern "C" fn obj_init1(
        api: *const BamHostApi,
        load_ctx: *mut BamLoadCtx,
        instance_out: *mut *mut BamInstance,
    ) -> FfiResult {
        // SAFETY: Valid host pointers for `api`, `load_ctx`, and `instance_out`.
        unsafe {
            *instance_out = core::ptr::null_mut();
            let reg_sys = (*api).register_system.expect("register_system wired");
            let d = BamSystemDesc::new(
                c"obj_tick_1".as_ptr(),
                core::ptr::null(),
                0,
                obj_sys1,
                core::ptr::null_mut(),
            );
            reg_sys(load_ctx, &raw const d)
        }
    }
    unsafe extern "C" fn obj_init2(
        api: *const BamHostApi,
        load_ctx: *mut BamLoadCtx,
        instance_out: *mut *mut BamInstance,
    ) -> FfiResult {
        // SAFETY: Valid host pointers for `api`, `load_ctx`, and `instance_out`.
        unsafe {
            *instance_out = core::ptr::null_mut();
            let reg_sys = (*api).register_system.expect("register_system wired");
            let d = BamSystemDesc::new(
                c"obj_tick_2".as_ptr(),
                core::ptr::null(),
                0,
                obj_sys2,
                core::ptr::null_mut(),
            );
            reg_sys(load_ctx, &raw const d)
        }
    }

    struct MockObjectLoader;
    impl InMemoryObjectLoader for MockObjectLoader {
        fn load_object_bytes(
            &self,
            label: &str,
            object_bytes: &[u8],
        ) -> Result<InMemoryModEntrypoints, ModLoadError> {
            match object_bytes {
                b"OBJ_V1" => Ok(InMemoryModEntrypoints::new(
                    obj_m1,
                    obj_init1,
                    noop_shutdown,
                )),
                b"OBJ_V2" => Ok(InMemoryModEntrypoints::new(
                    obj_m2,
                    obj_init2,
                    noop_shutdown,
                )),
                _ => Err(ModLoadError::InMemoryLoadFailed {
                    label: label.to_owned(),
                    detail: "invalid mock object bytes".to_owned(),
                }),
            }
        }
    }

    OBJ_V1_TICKS.store(0, Ordering::SeqCst);
    OBJ_V2_TICKS.store(0, Ordering::SeqCst);

    let mut h_err = HeadlessModTestHarness::with_error_policy(ModErrorPolicy::Log);
    let _ = h_err.app();
    let _ = h_err.drain_fault_events();

    let mut h_bud =
        HeadlessModTestHarness::with_budget_policy(ModBudgetPolicy::default()).with_telemetry();
    let loader = MockObjectLoader;

    // 1. `ModAppExt::{load,reload}_mod_from_object_loader` + error paths.
    let err_load = ModAppExt::load_mod_from_object_loader(
        h_bud.app_mut(),
        &loader,
        "obj://bad",
        b"BAD",
        LoadOptions::default(),
    )
    .expect_err("invalid object bytes must fail");
    assert!(matches!(err_load, ModLoadError::InMemoryLoadFailed { .. }));
    assert_eq!(h_bud.drain_load_failed_events().len(), 1);

    let id1 = ModAppExt::load_mod_from_object_loader(
        h_bud.app_mut(),
        &loader,
        "obj://v1",
        b"OBJ_V1",
        LoadOptions::default(),
    )
    .expect("load_mod_from_object_loader v1");
    assert_eq!(h_bud.drain_loaded_events().len(), 1);

    let err_reload = ModAppExt::reload_mod_from_object_loader(
        h_bud.app_mut(),
        id1,
        &loader,
        "obj://bad",
        b"BAD",
        LoadOptions::default(),
    )
    .expect_err("invalid reload object bytes must fail");
    assert!(matches!(
        err_reload,
        ModLoadError::InMemoryLoadFailed { .. }
    ));
    assert_eq!(h_bud.drain_load_failed_events().len(), 1);

    let id2 = ModAppExt::reload_mod_from_object_loader(
        h_bud.app_mut(),
        id1,
        &loader,
        "obj://v2",
        b"OBJ_V2",
        LoadOptions::default(),
    )
    .expect("reload_mod_from_object_loader v2");
    h_bud.step();
    assert_eq!(OBJ_V2_TICKS.load(Ordering::SeqCst), 1);

    // 2. `ModRegistryAppExt` & `ModWorldExt` object loader methods + `HeadlessModTestHarness` helpers.
    let id3 = ModRegistryAppExt::reload_mod_from_object_loader(
        h_bud.app_mut(),
        id2,
        &loader,
        "obj://v1_again",
        b"OBJ_V1",
        LoadOptions::default(),
    )
    .expect("ModRegistryAppExt::reload_mod_from_object_loader");
    h_bud.unload_mod(id3).expect("harness unload_mod");
    assert_eq!(h_bud.drain_unloaded_events().len(), 3);

    let id4 = ModRegistryAppExt::load_mod_from_object_loader(
        h_bud.app_mut(),
        &loader,
        "obj://reg_v1",
        b"OBJ_V1",
        LoadOptions::default(),
    )
    .expect("ModRegistryAppExt::load_mod_from_object_loader");
    h_bud.unload_mod(id4).expect("unload id4");

    let id5 = h_bud
        .world_mut()
        .load_mod_from_object_loader(&loader, "obj://world_v1", b"OBJ_V1", LoadOptions::default())
        .expect("ModWorldExt::load_mod_from_object_loader");
    let id6 = h_bud
        .world_mut()
        .reload_mod_from_object_loader(
            id5,
            &loader,
            "obj://world_v2",
            b"OBJ_V2",
            LoadOptions::default(),
        )
        .expect("ModWorldExt::reload_mod_from_object_loader");
    h_bud.unload_mod(id6).expect("unload id6");

    let id_ep = h_bud
        .load_mod_from_entrypoints(
            "static://harness_ep",
            InMemoryModEntrypoints::new(obj_m1, obj_init1, noop_shutdown),
        )
        .expect("harness load_mod_from_entrypoints");
    h_bud.unload_mod(id_ep).expect("unload id_ep");

    #[cfg(all(feature = "dynamic-loader", feature = "signature-verification"))]
    {
        let ordered_path = support::fixture_dylib("ordered_sets_mod");
        let copy = support::TempDylibCopy::new(&ordered_path, "r43_harness_dyn");
        let dyn1 = h_bud.load_mod(copy.path()).expect("harness load_mod");
        let dyn2 = h_bud.reload_mod(dyn1).expect("harness reload_mod");
        let dyn3 = h_bud
            .reload_mod_with(dyn2, LoadOptions::default().with_shadow_copy(true))
            .expect("harness reload_mod_with");
        h_bud.unload_mod(dyn3).expect("unload dyn3");
        let dyn4 = h_bud
            .load_mod_with(copy.path(), LoadOptions::default().with_shadow_copy(true))
            .expect("harness load_mod_with");
        h_bud.unload_mod(dyn4).expect("unload dyn4");
    }
}

#[test]
fn configure_schedule_set_and_mount_system_bind_new_generation_on_reload() {
    use bamabi_host_sys::ModScheduleSetRegistry;
    use bamabi_sys::{BAM_SCHEDULE_SET_FLAG_SEQUENCE, BamScheduleSetDesc, BamSystemSet};

    static SYS_V1_TICKS: AtomicU64 = AtomicU64::new(0);
    static SYS_V2_TICKS: AtomicU64 = AtomicU64::new(0);
    static SET_MOD_MANIFEST: BamManifest =
        BamManifest::new(c"r43_set_gen_mod".as_ptr(), c"0.4.0".as_ptr());

    unsafe extern "C" fn set_manifest() -> *const BamManifest {
        &raw const SET_MOD_MANIFEST
    }
    unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}
    unsafe extern "C" fn v1_sys(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        SYS_V1_TICKS.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    }
    unsafe extern "C" fn v2_sys(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        SYS_V2_TICKS.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    }

    unsafe fn register_set_and_mount(
        api: *const BamHostApi,
        load_ctx: *mut BamLoadCtx,
        sys_fn: unsafe extern "C" fn(*mut BamSystemCtx, *mut c_void) -> FfiResult,
    ) -> FfiResult {
        // SAFETY: Caller passes valid `HOST_API` and active `BamLoadCtx`.
        unsafe {
            let api_ref = &*api;
            let cfg = api_ref.configure_schedule_set.unwrap();
            let reg_sys = api_ref.register_system_handle.unwrap();
            let mount_sys = api_ref.mount_system_in_schedule.unwrap();

            let set_desc = BamScheduleSetDesc::new(c"set_a".as_ptr())
                .with_schedule(ScheduleId::UPDATE)
                .with_flags(BAM_SCHEDULE_SET_FLAG_SEQUENCE);
            let mut set_handle = BamSystemSet::INVALID;
            let rc = cfg(load_ctx, &raw const set_desc, &raw mut set_handle);
            if rc.is_err() {
                return rc;
            }

            let sys_desc = BamSystemDesc::new(
                c"worker_sys".as_ptr(),
                core::ptr::null(),
                0,
                sys_fn,
                core::ptr::null_mut(),
            )
            .with_schedule(ScheduleId::UPDATE);
            let mut sys_handle = bamabi_sys::BamSystem::INVALID;
            let rc = reg_sys(load_ctx, &raw const sys_desc, &raw mut sys_handle);
            if rc.is_err() {
                return rc;
            }
            mount_sys(load_ctx, sys_handle, ScheduleId::UPDATE, set_handle)
        }
    }

    unsafe extern "C" fn set_init_v1(
        api: *const BamHostApi,
        load_ctx: *mut BamLoadCtx,
        _out: *mut *mut BamInstance,
    ) -> FfiResult {
        // SAFETY: `api` and `load_ctx` are host-provided for `bamabi_init`.
        unsafe { register_set_and_mount(api, load_ctx, v1_sys) }
    }

    unsafe extern "C" fn set_init_v2(
        api: *const BamHostApi,
        load_ctx: *mut BamLoadCtx,
        _out: *mut *mut BamInstance,
    ) -> FfiResult {
        // SAFETY: `api` and `load_ctx` are host-provided for `bamabi_init`.
        unsafe { register_set_and_mount(api, load_ctx, v2_sys) }
    }

    SYS_V1_TICKS.store(0, Ordering::SeqCst);
    SYS_V2_TICKS.store(0, Ordering::SeqCst);

    let mut harness = HeadlessModTestHarness::new();
    let id_v1 = harness
        .load_mod_from_entrypoints(
            "static://set_gen_v1",
            InMemoryModEntrypoints::new(set_manifest, set_init_v1, noop_shutdown),
        )
        .expect("initial load with configure_schedule_set");

    {
        let reg = harness.app().world().resource::<ModScheduleSetRegistry>();
        let entries: Vec<_> = reg.sets_for_schedule(ScheduleId::UPDATE).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].generation, 0);
        assert_eq!(entries[0].mounted_systems.len(), 1);
        assert_eq!(entries[0].mounted_systems[0].generation, 0);
    }

    harness.step();
    assert_eq!(SYS_V1_TICKS.load(Ordering::SeqCst), 1);

    let id_v2 = harness
        .reload_mod_from_entrypoints(
            id_v1,
            "static://set_gen_v2",
            InMemoryModEntrypoints::new(set_manifest, set_init_v2, noop_shutdown),
            LoadOptions::default(),
        )
        .expect("reload with configure_schedule_set and mount_system_in_schedule must bind gen=1");

    {
        let reg = harness.app().world().resource::<ModScheduleSetRegistry>();
        let entries: Vec<_> = reg.sets_for_schedule(ScheduleId::UPDATE).collect();
        assert_eq!(entries.len(), 1, "old gen=0 set must be unregistered");
        assert_eq!(entries[0].mod_id, Some(id_v2));
        assert_eq!(entries[0].generation, 1);
        assert_eq!(entries[0].mounted_systems.len(), 1);
        assert_eq!(entries[0].mounted_systems[0].generation, 1);
    }

    harness.step();
    assert_eq!(SYS_V1_TICKS.load(Ordering::SeqCst), 1);
    assert_eq!(SYS_V2_TICKS.load(Ordering::SeqCst), 1);
}

#[test]
fn migrate_component_column_rollback_preserves_v1_entities_on_init_or_validation_failure() {
    use bamabi_host::read_entity_pod_bytes;
    use bamabi_sys::{ComponentDesc, ComponentUuid};

    static MIG_MANIFEST: BamManifest =
        BamManifest::new(c"r43_mig_rollback_mod".as_ptr(), c"0.4.0".as_ptr());

    const FP_V1: u64 = 0x1111_2222_3333_0001;
    const FP_V2: u64 = 0x1111_2222_3333_0002;
    const UUID_V1: ComponentUuid =
        ComponentUuid::from_parts("r43_mig_rollback_mod::PodComp", 8, 4, 1);
    const UUID_V2: ComponentUuid =
        ComponentUuid::from_parts("r43_mig_rollback_mod::PodComp", 16, 8, 2);

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PodV1 {
        x: f32,
        y: f32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct PodV2 {
        x: f64,
        y: f64,
    }

    unsafe extern "C" fn mig_manifest() -> *const BamManifest {
        &raw const MIG_MANIFEST
    }
    unsafe extern "C" fn noop_shutdown(_inst: *mut BamInstance) {}
    unsafe extern "C" fn noop_sys(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        FfiResult::OK
    }

    unsafe extern "C" fn widen_pod_v1_to_v2(
        src_row: *const c_void,
        dst_row: *mut c_void,
        _user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `src_row` holds `PodV1` and `dst_row` holds writable `PodV2`.
        unsafe {
            let old = core::ptr::read(src_row.cast::<PodV1>());
            core::ptr::write(
                dst_row.cast::<PodV2>(),
                PodV2 {
                    x: f64::from(old.x),
                    y: f64::from(old.y),
                },
            );
        }
        FfiResult::OK
    }

    unsafe extern "C" fn init_v1(
        api: *const BamHostApi,
        load_ctx: *mut BamLoadCtx,
        _out: *mut *mut BamInstance,
    ) -> FfiResult {
        // SAFETY: `api` and `load_ctx` are host-provided for `bamabi_init`.
        unsafe {
            let reg_comp = (*api).register_component.unwrap();
            let desc = ComponentDesc::new(c"PodComp".as_ptr(), UUID_V1, 8, 4)
                .with_schema_fingerprint(FP_V1);
            let mut handle = BamComponent::INVALID;
            reg_comp(load_ctx, &raw const desc, &raw mut handle)
        }
    }

    unsafe fn do_migrate_v1_to_v2(api: *const BamHostApi, load_ctx: *mut BamLoadCtx) -> FfiResult {
        // SAFETY: Caller passes valid `HOST_API` and active `BamLoadCtx`.
        unsafe {
            let api_ref = &*api;
            let host_ctx = (api_ref.load_host_ctx.unwrap())(load_ctx);
            let mut comp_h = BamComponent::INVALID;
            let rc = (api_ref.resolve_component.unwrap())(
                host_ctx,
                c"r43_mig_rollback_mod::PodComp".as_ptr(),
                core::ptr::null(),
                &raw mut comp_h,
            );
            if rc.is_err() {
                return rc;
            }
            let v2_desc = ComponentDesc::new(c"PodComp".as_ptr(), UUID_V2, 16, 8)
                .with_schema_fingerprint(FP_V2);
            (api_ref.migrate_component_column.unwrap())(
                load_ctx,
                comp_h,
                FP_V1,
                &raw const v2_desc,
                Some(widen_pod_v1_to_v2),
                core::ptr::null_mut(),
            )
        }
    }

    unsafe extern "C" fn init_v2_then_fail_init(
        api: *const BamHostApi,
        load_ctx: *mut BamLoadCtx,
        _out: *mut *mut BamInstance,
    ) -> FfiResult {
        // SAFETY: `api` and `load_ctx` are host-provided for `bamabi_init`.
        let rc = unsafe { do_migrate_v1_to_v2(api, load_ctx) };
        if rc.is_err() {
            return rc;
        }
        FfiResult::INVALID_ARGUMENT
    }

    unsafe extern "C" fn init_v2_then_fail_strict(
        api: *const BamHostApi,
        load_ctx: *mut BamLoadCtx,
        _out: *mut *mut BamInstance,
    ) -> FfiResult {
        // SAFETY: `api` and `load_ctx` are host-provided for `bamabi_init`.
        unsafe {
            let rc = do_migrate_v1_to_v2(api, load_ctx);
            if rc.is_err() {
                return rc;
            }
            // Register a system with an unregistered `BamComponent` handle to fail Phase 4 strict
            // validation *after* `bamabi_init` has returned `OK`.
            let missing_after = c"unknown_other_mod::missing_sys".as_ptr();
            let sys_desc = BamSystemDesc::new(
                c"bad_strict_sys".as_ptr(),
                core::ptr::null(),
                0,
                noop_sys,
                core::ptr::null_mut(),
            )
            .with_schedule(ScheduleId::UPDATE)
            .with_after(&raw const missing_after, 1);
            ((*api).register_system.unwrap())(load_ctx, &raw const sys_desc)
        }
    }

    unsafe extern "C" fn init_v2_ok(
        api: *const BamHostApi,
        load_ctx: *mut BamLoadCtx,
        _out: *mut *mut BamInstance,
    ) -> FfiResult {
        // SAFETY: `api` and `load_ctx` are host-provided for `bamabi_init`.
        unsafe { do_migrate_v1_to_v2(api, load_ctx) }
    }

    let mut harness = HeadlessModTestHarness::new();
    let id_v1 = harness
        .load_mod_from_entrypoints(
            "static://mig_v1",
            InMemoryModEntrypoints::new(mig_manifest, init_v1, noop_shutdown),
        )
        .expect("load v1");

    let comp_h = harness
        .world()
        .resource::<ModRegistry>()
        .get_by_name("r43_mig_rollback_mod::PodComp")
        .expect("PodComp registered")
        .handle;

    let initial_bytes: [u8; 8] = {
        let mut b = [0u8; 8];
        b[0..4].copy_from_slice(&5.0f32.to_le_bytes());
        b[4..8].copy_from_slice(&6.0f32.to_le_bytes());
        b
    };
    let entity = harness.world_mut().spawn_empty().id();
    bamabi_host::write_entity_pod_bytes(harness.world_mut(), entity, comp_h, &initial_bytes)
        .expect("seed PodComp v1");

    // 1. Failing `bamabi_init` after `migrate_component_column` must restore `old_comp_id` bytes!
    let err_init = harness
        .reload_mod_from_entrypoints(
            id_v1,
            "static://mig_v2_fail_init",
            InMemoryModEntrypoints::new(mig_manifest, init_v2_then_fail_init, noop_shutdown),
            LoadOptions::default(),
        )
        .expect_err("v2 returning INVALID_ARGUMENT must fail reload");
    assert!(matches!(err_init, ModLoadError::InitFailed { .. }));
    assert_eq!(
        read_entity_pod_bytes(harness.world(), entity, comp_h),
        Ok(initial_bytes.to_vec()),
        "surviving v1 entity must keep original PodComp bytes after failed bamabi_init"
    );

    // 2. Failing Phase 4 strict validation after `bamabi_init` must also restore `old_comp_id`!
    let err_strict = harness
        .reload_mod_from_entrypoints(
            id_v1,
            "static://mig_v2_fail_strict",
            InMemoryModEntrypoints::new(mig_manifest, init_v2_then_fail_strict, noop_shutdown),
            LoadOptions::default().strict(),
        )
        .expect_err("v2 failing Phase 4 strict validation must fail reload");
    assert!(matches!(
        err_strict,
        ModLoadError::UnresolvedOrderingTarget { .. }
    ));
    assert_eq!(
        read_entity_pod_bytes(harness.world(), entity, comp_h),
        Ok(initial_bytes.to_vec()),
        "surviving v1 entity must keep original PodComp bytes after failed Phase 4 validation"
    );

    // 3. Subsequent clean reload migrates v1 -> v2 properly.
    harness
        .reload_mod_from_entrypoints(
            id_v1,
            "static://mig_v2_ok",
            InMemoryModEntrypoints::new(mig_manifest, init_v2_ok, noop_shutdown),
            LoadOptions::default(),
        )
        .expect("clean v2 reload succeeds");
    let mut expected_v2 = Vec::with_capacity(16);
    expected_v2.extend_from_slice(&5.0f64.to_le_bytes());
    expected_v2.extend_from_slice(&6.0f64.to_le_bytes());
    assert_eq!(
        read_entity_pod_bytes(harness.world(), entity, comp_h),
        Ok(expected_v2)
    );
}
