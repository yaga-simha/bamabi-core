//! Integration tests for `bamabi_watcher` R4.3 hardening:
//! - 2-poll file stability gate (`with_stability_polls(2)`)
//! - Native shared-library magic header validation (ELF / Mach-O / PE)
//! - Bounded FIFO ring buffer (`ModHotReloadHistory::with_max_history`)
//! - `.schedule_delta.json` live hot-rewire without unloading/relinking
//! - `ModHotReloadPlugin::build` recording watcher initialization failures

use bamabi_host::plugin::AbiModPlugin;
use bamabi_host::{ModHost, ModLifecycleFlush, ModRegistry, rewire_mod_schedule_sets_in_world};
use bamabi_host_sys::schedule::{
    ModNamedSystemSet, ModScheduleSetRegistry, ScheduleSetConfig, ScheduleSetMode,
    mount_mod_systems_in_world,
};
use bamabi_host_sys::{GuestSystem, LoadOptions};
use bamabi_sys::{BamSystemCtx, FfiResult, ScheduleId};
use bamabi_watcher::{
    ModHotReloadAction, ModHotReloadHistory, ModHotReloadOutcome, ModHotReloadPlugin,
    ModHotReloadWatcher,
};
use bevy_app::{App, PostUpdate, Update};
use core::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}_{}_{}", std::process::id(), id));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn dylib_filename(stem: &str) -> String {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    format!("{prefix}{stem}.{ext}")
}

fn compile_c_cdylib(work_dir: &Path, c_src: &Path, out_dylib: &Path) -> bool {
    let candidates = if cfg!(target_os = "windows") {
        ["clang", "clang-cl", "cl", "gcc", "cc"]
    } else {
        ["clang", "gcc", "cc", "clang", "gcc"]
    };
    let Some(cc) = candidates.iter().copied().find(|cand| {
        Command::new(cand)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }) else {
        return false;
    };
    let inc_dir = work_dir.join("_inc");
    std::fs::create_dir_all(&inc_dir).expect("create inc dir");
    std::fs::write(
        inc_dir.join("bamabi.h"),
        bamabi_sdk_gen::render_c99_header().expect("render bamabi.h"),
    )
    .expect("write bamabi.h");

    let mut cmd = Command::new(cc);
    cmd.args(["-std=c99", "-O2", "-shared"]);
    if !cfg!(windows) {
        cmd.arg("-fPIC");
    }
    let out = cmd
        .arg("-I")
        .arg(&inc_dir)
        .arg("-o")
        .arg(out_dylib)
        .arg(c_src)
        .output()
        .expect("run C compiler");
    assert!(
        out.status.success(),
        "C compiler failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    true
}

#[test]
fn history_ring_buffer_evicts_oldest_outcomes_and_plugin_records_init_errors() {
    let mut history = ModHotReloadHistory::with_max_history(3);
    assert_eq!(history.max_history(), 3);

    for i in 0..5usize {
        history.push(ModHotReloadOutcome {
            path: PathBuf::from(format!("mod_{i}.dylib")),
            mod_id: None,
            action: ModHotReloadAction::Loaded,
        });
    }
    assert_eq!(history.outcomes.len(), 3);
    let paths: Vec<_> = history
        .outcomes
        .iter()
        .map(|o| o.path.to_string_lossy().into_owned())
        .collect();
    assert_eq!(paths, vec!["mod_2.dylib", "mod_3.dylib", "mod_4.dylib"]);

    // Missing watch directory records init failure in `ModHotReloadHistory` instead of panicking.
    let missing_dir = std::env::temp_dir().join(format!(
        "bam_missing_watch_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut app = App::new();
    app.add_plugins((
        AbiModPlugin,
        ModHotReloadPlugin::new(&missing_dir)
            .with_debounce(Duration::from_millis(20))
            .with_max_history(8),
    ));
    let hist = app.world().resource::<ModHotReloadHistory>();
    assert_eq!(hist.max_history(), 8);
    assert_eq!(hist.outcomes.len(), 1);
    assert!(matches!(
        &hist.outcomes[0].action,
        ModHotReloadAction::Failed(msg) if !msg.is_empty()
    ));
}

fn wait_for_outcomes(
    watcher: &ModHotReloadWatcher,
    world: &mut bevy_ecs::world::World,
) -> Vec<ModHotReloadOutcome> {
    for _ in 0..80 {
        let out = watcher.process_pending_reloads(world);
        if !out.is_empty() {
            return out;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Vec::new()
}

#[test]
fn two_poll_stability_invalid_magic_rejection_and_schedule_delta_hot_rewire() {
    let temp = TempDirGuard::new("bam_watcher_r43");
    let watcher = ModHotReloadWatcher::watch_with_options(
        &temp.0,
        Duration::from_millis(20),
        false,
        LoadOptions::default(),
    )
    .expect("start watcher")
    .with_stability_polls(2);

    // 1. Corrupted non-binary `.dylib` file fails magic-header check once stabilized across 2 polls.
    let bad_dylib = temp.0.join(dylib_filename("corrupt"));
    std::fs::write(&bad_dylib, b"NOT_AN_EXECUTABLE_BINARY").expect("write corrupt dylib");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    // First poll once notify delivers event only registers stability observation; second returns outcome.
    let poll_out = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(poll_out.len(), 1);
    assert!(
        matches!(&poll_out[0].action, ModHotReloadAction::Failed(msg) if msg.contains("header")),
        "expected invalid binary header failure, got {poll_out:?}"
    );
    let _ = std::fs::remove_file(&bad_dylib);
    // Drain any removal event.
    std::thread::sleep(Duration::from_millis(60));
    let _ = watcher.process_pending_reloads(app.world_mut());

    // 2. Compile real C fixture and verify live load + `.schedule_delta.json` hot-rewire.
    let ordered_c = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests/fixtures/c_mods/ordered_sets_mod.c");
    let stage_dylib = temp.0.join("_stage_ordered.tmp");
    let good_dylib = temp.0.join(dylib_filename("ordered_sets_mod"));
    if !compile_c_cdylib(&temp.0, &ordered_c, &stage_dylib) {
        return;
    }
    std::fs::rename(&stage_dylib, &good_dylib).expect("stage dylib atomically");

    let loaded = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].action, ModHotReloadAction::Loaded);
    let mod_id = loaded[0].mod_id.expect("loaded mod id");
    app.world_mut().run_schedule(ModLifecycleFlush);

    // Register a `ModScheduleSetRegistry` set and trigger `.schedule_delta.json` hot-rewire
    // without unloading/relinking the loaded mod!
    let set_handle = {
        let mut reg = app.world_mut().resource_mut::<ModScheduleSetRegistry>();
        let h = reg
            .configure_set(ScheduleSetConfig {
                mod_id: Some(mod_id),
                mod_name: Arc::from("ordered_sets_mod"),
                set_name: Arc::from("live_group"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .expect("configure set");
        reg.mount_system(
            h,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("ordered_sets_mod", "first_pass"),
        )
        .expect("mount first_pass");
        h
    };

    let delta_path = temp.0.join("ordered_sets_mod.schedule_delta.json");
    std::fs::write(
        &delta_path,
        r#"{"schedule": "Update", "sets": [{"set": "ordered_sets_mod::live_group", "mode": "Sequence"}]}"#,
    )
    .expect("write schedule_delta.json");

    let delta_outcomes = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(delta_outcomes.len(), 1);
    assert_eq!(delta_outcomes[0].action, ModHotReloadAction::Reloaded);
    assert_eq!(
        app.world()
            .resource::<ModScheduleSetRegistry>()
            .get(set_handle)
            .unwrap()
            .mode,
        ScheduleSetMode::Sequence
    );
    // ModId stayed identical (rewired in-place without relinking!).
    assert_eq!(app.world().resource::<ModHost>().loaded_mods().len(), 1);
    assert_eq!(
        app.world().resource::<ModHost>().loaded_mods()[0].id,
        mod_id
    );

    // 3. Rewire again with `before_weak`, `after_weak`, `parent_sets`, `SequenceWeak`, and bad JSON.
    let set2_handle = {
        let mut reg = app.world_mut().resource_mut::<ModScheduleSetRegistry>();
        reg.configure_set(ScheduleSetConfig {
            mod_id: Some(mod_id),
            mod_name: Arc::from("ordered_sets_mod"),
            set_name: Arc::from("second_group"),
            generation: 0,
            schedule: ScheduleId::UPDATE,
            mode: ScheduleSetMode::Parallel,
            ..ScheduleSetConfig::default()
        })
        .expect("configure second_group")
    };
    std::fs::write(
        &delta_path,
        r#"{"schedule": "Update", "sets": [{"set": "ordered_sets_mod::live_group", "mode": "SequenceWeak", "before": ["ordered_sets_mod::second_group"], "after": [], "before_weak": true, "after_weak": false, "parent_sets": ["second_group"]}]}"#,
    )
    .expect("write second schedule_delta.json");
    let delta2 = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(delta2.len(), 1);
    assert_eq!(delta2[0].action, ModHotReloadAction::Reloaded);
    {
        let reg = app.world().resource::<ModScheduleSetRegistry>();
        let entry = reg.get(set_handle).unwrap();
        assert_eq!(entry.mode, ScheduleSetMode::SequenceWeak);
        assert!(entry.before_weak);
        assert!(!entry.after_weak);
        assert_eq!(entry.before_sets, vec![set2_handle]);
        assert_eq!(entry.parent_sets, vec!["second_group".to_string()]);
    }

    // Invalid JSON in `.schedule_delta.json` records `Failed` outcome cleanly.
    std::fs::write(&delta_path, b"{invalid_json").expect("write invalid delta");
    let bad_delta = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(bad_delta.len(), 1);
    assert!(matches!(bad_delta[0].action, ModHotReloadAction::Failed(_)));

    // 4. Multi-entry `.schedule_delta.json` where entry 1 mutates `live_group` and entry 2 fails
    // must be completely atomic: `ModScheduleSetRegistry` retains `SequenceWeak` and prior edges.
    std::fs::write(
        &delta_path,
        r#"{
            "schedule": "Update",
            "sets": [
                {"set": "ordered_sets_mod::live_group", "mode": "Parallel"},
                {"set": "ordered_sets_mod::nonexistent_set", "mode": "Sequence"}
            ]
        }"#,
    )
    .expect("write failing multi-entry schedule_delta.json");
    let multi_fail = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(multi_fail.len(), 1);
    assert!(
        matches!(&multi_fail[0].action, ModHotReloadAction::Failed(msg) if msg.contains("nonexistent_set")),
        "expected Failed outcome for nonexistent_set, got {multi_fail:?}"
    );
    {
        let reg = app.world().resource::<ModScheduleSetRegistry>();
        let entry = reg.get(set_handle).unwrap();
        assert_eq!(
            entry.mode,
            ScheduleSetMode::SequenceWeak,
            "entry 1 must not partially mutate ModScheduleSetRegistry when entry 2 fails"
        );
        assert!(entry.before_weak);
        assert_eq!(entry.before_sets, vec![set2_handle]);
        assert_eq!(entry.parent_sets, vec!["second_group".to_string()]);
    }
}

#[test]
fn sig_hot_reload_with_configure_schedule_set_binds_new_generation() {
    let temp = TempDirGuard::new("bam_watcher_sig_set");
    let c_src = temp.0.join("set_sig_mod.c");
    std::fs::write(
        &c_src,
        r#"
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include "bamabi.h"

static const BamManifest MANIFEST = {
    .struct_size = (uint32_t)sizeof(BamManifest),
    .abi_version = BAM_ABI_VERSION,
    .name = "set_sig_mod",
    .version = "0.4.0",
    .flags = 0u,
    .reserved0 = 0u
};

static BamResult noop_cb(BamSystemCtx* sys_ctx, void* user_data) {
    (void)sys_ctx;
    (void)user_data;
    return BAM_RESULT_OK;
}

BAM_EXPORT const BamManifest* bamabi_manifest(void) {
    return &MANIFEST;
}

BAM_EXPORT BamResult bamabi_init(
    const BamHostApi* api,
    BamLoadCtx* load_ctx,
    BamInstance** instance_out
) {
    BamHostApi local_api;
    BamScheduleSetDesc set_desc;
    BamSystemDesc sys_desc;
    BamSystemSet set_handle = BAM_SYSTEM_SET_INVALID;
    BamSystem sys_handle = BAM_SYSTEM_INVALID;
    BamResult rc;

    if (load_ctx == NULL || instance_out == NULL) {
        return BAM_RESULT_NULL_POINTER;
    }
    *instance_out = NULL;

    rc = bam_host_api_copy_bounded(&local_api, sizeof(local_api), api);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    set_desc = bam_schedule_set_desc("set_a", BAM_SCHEDULE_UPDATE);
    set_desc.flags = BAM_SCHEDULE_SET_FLAG_SEQUENCE;
    rc = local_api.configure_schedule_set(load_ctx, &set_desc, &set_handle);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    memset(&sys_desc, 0, sizeof(sys_desc));
    sys_desc.struct_size = (uint32_t)sizeof(BamSystemDesc);
    sys_desc.name = "tick";
    sys_desc.callback = noop_cb;
    sys_desc.schedule = BAM_SCHEDULE_UPDATE;
    rc = local_api.register_system_handle(load_ctx, &sys_desc, &sys_handle);
    if (rc != BAM_RESULT_OK) {
        return rc;
    }

    return local_api.mount_system_in_schedule(
        load_ctx,
        sys_handle,
        BAM_SCHEDULE_UPDATE,
        set_handle
    );
}

BAM_EXPORT void bamabi_shutdown(BamInstance* instance) {
    (void)instance;
}
"#,
    )
    .expect("write C source");

    let watcher = ModHotReloadWatcher::watch_with_options(
        &temp.0,
        Duration::from_millis(20),
        false,
        LoadOptions::default(),
    )
    .expect("start watcher")
    .with_stability_polls(2);

    let stage_dylib = temp.0.join("_stage_set_sig.tmp");
    let mod_dylib = temp.0.join(dylib_filename("set_sig_mod"));
    if !compile_c_cdylib(&temp.0, &c_src, &stage_dylib) {
        return;
    }
    std::fs::rename(&stage_dylib, &mod_dylib).expect("stage dylib");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let loaded = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].action, ModHotReloadAction::Loaded);
    {
        let reg = app.world().resource::<ModScheduleSetRegistry>();
        let entries: Vec<_> = reg.sets_for_schedule(ScheduleId::UPDATE).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].generation, 0);
        assert_eq!(entries[0].mounted_systems[0].generation, 0);
    }

    // Touching/writing a `.sig` sidecar must trigger reload of sibling `.dylib` cleanly with gen=1.
    let sig_path = temp
        .0
        .join(format!("{}.sig", dylib_filename("set_sig_mod")));
    std::fs::write(&sig_path, b"dummy-sig-v1").expect("write .sig sidecar");
    let reloaded = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(reloaded.len(), 1);
    assert_eq!(
        reloaded[0].action,
        ModHotReloadAction::Reloaded,
        "expected .sig hot-reload to succeed with new generation, got {reloaded:?}"
    );
    {
        let reg = app.world().resource::<ModScheduleSetRegistry>();
        let entries: Vec<_> = reg.sets_for_schedule(ScheduleId::UPDATE).collect();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].generation, 1);
        assert_eq!(entries[0].mounted_systems[0].generation, 1);
    }
}

#[test]
fn schedule_delta_parent_sets_cycle_and_unknown_target_do_not_poison_schedule() {
    let temp = TempDirGuard::new("bam_watcher_r43_3a");
    let watcher = ModHotReloadWatcher::watch_with_options(
        &temp.0,
        Duration::from_millis(20),
        false,
        LoadOptions::default(),
    )
    .expect("start watcher")
    .with_stability_polls(2);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let hits = Arc::new(AtomicU64::new(0));
    unsafe extern "C" fn bump_cb(_sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        // SAFETY: `ud` points to `Arc::into_raw` leak-free `AtomicU64` alive for test duration.
        unsafe { (*(ud.cast::<AtomicU64>())).fetch_add(1, Ordering::SeqCst) };
        FfiResult::OK
    }

    let (set1_h, set2_h) = {
        let mut reg = app.world_mut().resource_mut::<ModScheduleSetRegistry>();
        let h1 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("pcycle_mod"),
                set_name: Arc::from("set_1"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .expect("configure set_1");
        let h2 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("pcycle_mod"),
                set_name: Arc::from("set_2"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .expect("configure set_2");
        reg.mount_system(
            h1,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("pcycle_mod", "s1"),
        )
        .unwrap();
        reg.mount_system(
            h2,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("pcycle_mod", "s2"),
        )
        .unwrap();
        (h1, h2)
    };

    let mod_reg = app.world().resource::<ModRegistry>().clone();
    let ud_ptr = Arc::as_ptr(&hits).cast_mut().cast::<c_void>();
    let s1 = GuestSystem::new("s1", vec![], bump_cb, ud_ptr, mod_reg.clone())
        .with_schedule(ScheduleId::UPDATE)
        .with_in_sets(vec!["pcycle_mod::set_1".to_owned()]);
    let s2 = GuestSystem::new("s2", vec![], bump_cb, ud_ptr, mod_reg)
        .with_schedule(ScheduleId::UPDATE)
        .with_in_sets(vec!["pcycle_mod::set_2".to_owned()]);
    mount_mod_systems_in_world(app.world_mut(), "pcycle_mod", 0, vec![s1, s2]).unwrap();
    app.update();
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    let delta_path = temp.0.join("pcycle_mod.schedule_delta.json");

    // 1. 2-node parent_sets cycle (`set_1 <-> set_2`) must fail atomically without poisoning `Update`.
    std::fs::write(
        &delta_path,
        r#"{
            "schedule": "Update",
            "sets": [
                {"set": "pcycle_mod::set_1", "parent_sets": ["pcycle_mod::set_2"]},
                {"set": "pcycle_mod::set_2", "parent_sets": ["pcycle_mod::set_1"]}
            ]
        }"#,
    )
    .expect("write cyclic parent_sets delta");
    let out1 = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(out1.len(), 1);
    assert!(
        matches!(&out1[0].action, ModHotReloadAction::Failed(_)),
        "expected Failed for cyclic parent_sets, got {out1:?}"
    );
    {
        let reg = app.world().resource::<ModScheduleSetRegistry>();
        assert!(reg.get(set1_h).unwrap().parent_sets.is_empty());
        assert!(reg.get(set2_h).unwrap().parent_sets.is_empty());
    }
    rewire_mod_schedule_sets_in_world(app.world_mut(), ScheduleId::UPDATE)
        .expect("Update schedule must not be poisoned after cyclic parent_sets failure");
    app.update();
    assert_eq!(hits.load(Ordering::SeqCst), 4);

    // 2. Self-parenting (`set_1 -> set_1`) and unknown parent (`missing`) must also fail cleanly.
    for (idx, bad_json) in [
        r#"{"schedule": "Update", "sets": [{"set": "pcycle_mod::set_1", "parent_sets": ["pcycle_mod::set_1"]}]}"#,
        r#"{"schedule": "Update", "sets": [{"set": "pcycle_mod::set_1", "parent_sets": ["pcycle_mod::missing"]}]}"#,
    ]
    .into_iter()
    .enumerate()
    {
        std::fs::write(&delta_path, bad_json).expect("write invalid parent_sets delta");
        let out = wait_for_outcomes(&watcher, app.world_mut());
        assert_eq!(out.len(), 1, "case {idx}");
        assert!(
            matches!(&out[0].action, ModHotReloadAction::Failed(_)),
            "expected Failed for invalid parent_sets case {idx}, got {out:?}"
        );
        rewire_mod_schedule_sets_in_world(app.world_mut(), ScheduleId::UPDATE)
            .expect("Update schedule must remain healthy");
        app.update();
    }
    assert_eq!(hits.load(Ordering::SeqCst), 8);

    // 3. Incremental reversal: first apply valid `set_1 in set_2`, then attempt `set_1=[], set_2 in set_1`.
    // Because Bevy ScheduleGraph retains `set_2 -> set_1` statically, `try_sync` must reject the
    // reversal before mutating `ScheduleGraph`.
    std::fs::write(
        &delta_path,
        r#"{"schedule": "Update", "sets": [{"set": "pcycle_mod::set_1", "parent_sets": ["pcycle_mod::set_2"]}]}"#,
    )
    .expect("write valid parent_sets delta");
    let out_ok = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(out_ok.len(), 1);
    assert_eq!(out_ok[0].action, ModHotReloadAction::Reloaded);

    std::fs::write(
        &delta_path,
        r#"{
            "schedule": "Update",
            "sets": [
                {"set": "pcycle_mod::set_1", "parent_sets": []},
                {"set": "pcycle_mod::set_2", "parent_sets": ["pcycle_mod::set_1"]}
            ]
        }"#,
    )
    .expect("write reversing parent_sets delta");
    let out_rev = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(out_rev.len(), 1);
    assert!(
        matches!(&out_rev[0].action, ModHotReloadAction::Failed(_)),
        "expected Failed when reversing parent_sets against ScheduleGraph, got {out_rev:?}"
    );
    {
        let reg = app.world().resource::<ModScheduleSetRegistry>();
        assert_eq!(
            reg.get(set1_h).unwrap().parent_sets,
            vec!["pcycle_mod::set_2".to_string()]
        );
        assert!(reg.get(set2_h).unwrap().parent_sets.is_empty());
    }
    rewire_mod_schedule_sets_in_world(app.world_mut(), ScheduleId::UPDATE)
        .expect("Update schedule must not be poisoned after parent_sets reversal attempt");
    app.update();
    assert_eq!(hits.load(Ordering::SeqCst), 10);
}

#[test]
fn multi_schedule_delta_failure_rolls_back_earlier_rewired_schedules() {
    let temp = TempDirGuard::new("bam_watcher_r43_3b");
    let watcher = ModHotReloadWatcher::watch_with_options(
        &temp.0,
        Duration::from_millis(20),
        false,
        LoadOptions::default(),
    )
    .expect("start watcher")
    .with_stability_polls(2);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    struct OrderLog {
        entries: Mutex<Vec<&'static str>>,
    }
    struct LogItem {
        log: Arc<OrderLog>,
        label: &'static str,
    }

    unsafe extern "C" fn log_cb(_sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        // SAFETY: `ud` points to `Box<LogItem>` kept alive for the duration of the test.
        let item = unsafe { &*(ud.cast::<LogItem>()) };
        item.log.entries.lock().unwrap().push(item.label);
        FfiResult::OK
    }

    let log = Arc::new(OrderLog {
        entries: Mutex::new(Vec::new()),
    });
    let items: Vec<LogItem> = ["u1", "u2", "p1", "p2"]
        .into_iter()
        .map(|label| LogItem {
            log: Arc::clone(&log),
            label,
        })
        .collect();

    let (su1_h, su2_h, sp1_h, sp2_h) = {
        let mut reg = app.world_mut().resource_mut::<ModScheduleSetRegistry>();
        let su2 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("msch_mod"),
                set_name: Arc::from("su2"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        let su1 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("msch_mod"),
                set_name: Arc::from("su1"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                before_sets: vec![su2],
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        let sp1 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("msch_mod"),
                set_name: Arc::from("sp1"),
                generation: 0,
                schedule: ScheduleId::POST_UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        let sp2 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("msch_mod"),
                set_name: Arc::from("sp2"),
                generation: 0,
                schedule: ScheduleId::POST_UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        reg.mount_system(
            su1,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("msch_mod", "u1"),
        )
        .unwrap();
        reg.mount_system(
            su2,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("msch_mod", "u2"),
        )
        .unwrap();
        reg.mount_system(
            sp1,
            ScheduleId::POST_UPDATE,
            ModNamedSystemSet::new("msch_mod", "p1"),
        )
        .unwrap();
        reg.mount_system(
            sp2,
            ScheduleId::POST_UPDATE,
            ModNamedSystemSet::new("msch_mod", "p2"),
        )
        .unwrap();
        (su1, su2, sp1, sp2)
    };

    let mod_reg = app.world().resource::<ModRegistry>().clone();
    let u1 = GuestSystem::new(
        "u1",
        vec![],
        log_cb,
        (&raw const items[0]).cast_mut().cast::<c_void>(),
        mod_reg.clone(),
    )
    .with_schedule(ScheduleId::UPDATE)
    .with_in_sets(vec!["msch_mod::su1".to_owned()]);
    let u2 = GuestSystem::new(
        "u2",
        vec![],
        log_cb,
        (&raw const items[1]).cast_mut().cast::<c_void>(),
        mod_reg.clone(),
    )
    .with_schedule(ScheduleId::UPDATE)
    .with_in_sets(vec!["msch_mod::su2".to_owned()]);
    // `p2` is statically ordered BEFORE `p1`, so trying to wire `sp1` BEFORE `sp2` in `PostUpdate`
    // creates a `ScheduleBuildError` dependency cycle on schedule #2 (`PostUpdate`)!
    let p1 = GuestSystem::new(
        "p1",
        vec![],
        log_cb,
        (&raw const items[2]).cast_mut().cast::<c_void>(),
        mod_reg.clone(),
    )
    .with_schedule(ScheduleId::POST_UPDATE)
    .with_in_sets(vec!["msch_mod::sp1".to_owned()]);
    let p2 = GuestSystem::new(
        "p2",
        vec![],
        log_cb,
        (&raw const items[3]).cast_mut().cast::<c_void>(),
        mod_reg,
    )
    .with_schedule(ScheduleId::POST_UPDATE)
    .with_in_sets(vec!["msch_mod::sp2".to_owned()])
    .with_ordering(vec![], vec!["msch_mod::p1".to_owned()]);

    mount_mod_systems_in_world(app.world_mut(), "msch_mod", 0, vec![u1, u2, p1, p2]).unwrap();

    app.world_mut().run_schedule(Update);
    assert_eq!(*log.entries.lock().unwrap(), vec!["u1", "u2"]);
    log.entries.lock().unwrap().clear();

    app.world_mut().run_schedule(PostUpdate);
    assert_eq!(*log.entries.lock().unwrap(), vec!["p2", "p1"]);
    log.entries.lock().unwrap().clear();

    let delta_path = temp.0.join("msch_mod.schedule_delta.json");
    std::fs::write(
        &delta_path,
        r#"{
            "schedule": "Update",
            "sets": [
                {"schedule": "Update", "set": "msch_mod::su1", "before": [], "after": ["msch_mod::su2"]},
                {"schedule": "PostUpdate", "set": "msch_mod::sp1", "before": ["msch_mod::sp2"], "after": []}
            ]
        }"#,
    )
    .expect("write multi-schedule delta");

    let out = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(out.len(), 1);
    assert!(
        matches!(&out[0].action, ModHotReloadAction::Failed(_)),
        "expected Failed when PostUpdate rewire hits cycle, got {out:?}"
    );

    {
        let reg = app.world().resource::<ModScheduleSetRegistry>();
        assert_eq!(reg.get(su1_h).unwrap().before_sets, vec![su2_h]);
        assert!(reg.get(su1_h).unwrap().after_sets.is_empty());
        assert!(reg.get(sp1_h).unwrap().before_sets.is_empty());
        assert!(reg.get(sp2_h).unwrap().after_sets.is_empty());
    }

    // Both `Update` and `PostUpdate` must still execute in their original pre-delta orders!
    app.world_mut().run_schedule(Update);
    assert_eq!(
        *log.entries.lock().unwrap(),
        vec!["u1", "u2"],
        "Update schedule order must roll back when PostUpdate rewire fails"
    );
    log.entries.lock().unwrap().clear();

    app.world_mut().run_schedule(PostUpdate);
    assert_eq!(*log.entries.lock().unwrap(), vec!["p2", "p1"]);
}

#[test]
fn schedule_delta_parent_sets_cross_dependency_does_not_poison_schedule() {
    let temp = TempDirGuard::new("bam_watcher_r43_xdep");
    let watcher = ModHotReloadWatcher::watch_with_options(
        &temp.0,
        Duration::from_millis(20),
        false,
        LoadOptions::default(),
    )
    .expect("start watcher")
    .with_stability_polls(2);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let hits = Arc::new(AtomicU64::new(0));
    unsafe extern "C" fn bump_cb(_sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        // SAFETY: `ud` points to live `Arc<AtomicU64>` for the duration of this test.
        unsafe { (*(ud.cast::<AtomicU64>())).fetch_add(1, Ordering::SeqCst) };
        FfiResult::OK
    }

    let set1_h = {
        let mut reg = app.world_mut().resource_mut::<ModScheduleSetRegistry>();
        let h1 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("xdep"),
                set_name: Arc::from("set_1"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .expect("configure set_1");
        let h2 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("xdep"),
                set_name: Arc::from("set_2"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .expect("configure set_2");
        reg.mount_system(h1, ScheduleId::UPDATE, ModNamedSystemSet::new("xdep", "s1"))
            .unwrap();
        reg.mount_system(h2, ScheduleId::UPDATE, ModNamedSystemSet::new("xdep", "s2"))
            .unwrap();
        h1
    };

    let mod_reg = app.world().resource::<ModRegistry>().clone();
    let ud_ptr = Arc::as_ptr(&hits).cast_mut().cast::<c_void>();
    let s1 = GuestSystem::new("s1", vec![], bump_cb, ud_ptr, mod_reg.clone())
        .with_schedule(ScheduleId::UPDATE)
        .with_in_sets(vec!["xdep::set_1".to_owned()])
        .with_ordering(vec!["xdep::set_2".to_owned()], vec![]);
    let s2 = GuestSystem::new("s2", vec![], bump_cb, ud_ptr, mod_reg)
        .with_schedule(ScheduleId::UPDATE)
        .with_in_sets(vec!["xdep::set_2".to_owned()]);
    mount_mod_systems_in_world(app.world_mut(), "xdep", 0, vec![s1, s2]).unwrap();
    app.update();
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    let delta_path = temp.0.join("xdep.schedule_delta.json");
    std::fs::write(
        &delta_path,
        r#"{"schedule": "Update", "sets": [{"set": "xdep::set_1", "parent_sets": ["xdep::set_2"]}]}"#,
    )
    .expect("write CrossDependency parent_sets delta");

    let out = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(out.len(), 1);
    assert!(
        matches!(&out[0].action, ModHotReloadAction::Failed(_)),
        "expected Failed for CrossDependency parent_sets, got {out:?}"
    );
    assert!(
        app.world()
            .resource::<ModScheduleSetRegistry>()
            .get(set1_h)
            .unwrap()
            .parent_sets
            .is_empty()
    );
    rewire_mod_schedule_sets_in_world(app.world_mut(), ScheduleId::UPDATE)
        .expect("Update schedule must not be poisoned after CrossDependency parent_sets failure");
    app.update();
    assert_eq!(hits.load(Ordering::SeqCst), 4);
}

#[test]
fn schedule_delta_single_schedule_parent_sets_is_atomic_when_ordering_fails() {
    let temp = TempDirGuard::new("bam_watcher_r43_sleak");
    let watcher = ModHotReloadWatcher::watch_with_options(
        &temp.0,
        Duration::from_millis(20),
        false,
        LoadOptions::default(),
    )
    .expect("start watcher")
    .with_stability_polls(2);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    let u1_hits = Arc::new(AtomicU64::new(0));
    let other_hits = Arc::new(AtomicU64::new(0));
    unsafe extern "C" fn bump_cb(_sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        // SAFETY: `ud` points to live `Arc<AtomicU64>` for the duration of this test.
        unsafe { (*(ud.cast::<AtomicU64>())).fetch_add(1, Ordering::SeqCst) };
        FfiResult::OK
    }
    unsafe extern "C" fn always_false(_sys_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        FfiResult::INVALID_STATE
    }

    let (su1_h, su3_h) = {
        let mut reg = app.world_mut().resource_mut::<ModScheduleSetRegistry>();
        let su1 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("sleak"),
                set_name: Arc::from("su1"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        let su2 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("sleak"),
                set_name: Arc::from("su2"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                run_if_ctx: Some(always_false),
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        let su3 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("sleak"),
                set_name: Arc::from("su3"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        let su4 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("sleak"),
                set_name: Arc::from("su4"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        reg.mount_system(
            su1,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("sleak", "u1"),
        )
        .unwrap();
        reg.mount_system(
            su2,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("sleak", "u2"),
        )
        .unwrap();
        reg.mount_system(
            su3,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("sleak", "u3"),
        )
        .unwrap();
        reg.mount_system(
            su4,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("sleak", "u4"),
        )
        .unwrap();
        (su1, su3)
    };

    let mod_reg = app.world().resource::<ModRegistry>().clone();
    let u1_ptr = Arc::as_ptr(&u1_hits).cast_mut().cast::<c_void>();
    let other_ptr = Arc::as_ptr(&other_hits).cast_mut().cast::<c_void>();
    let u1 = GuestSystem::new("u1", vec![], bump_cb, u1_ptr, mod_reg.clone())
        .with_schedule(ScheduleId::UPDATE)
        .with_in_sets(vec!["sleak::su1".to_owned()]);
    let u2 = GuestSystem::new("u2", vec![], bump_cb, other_ptr, mod_reg.clone())
        .with_schedule(ScheduleId::UPDATE)
        .with_in_sets(vec!["sleak::su2".to_owned()]);
    let u3 = GuestSystem::new("u3", vec![], bump_cb, other_ptr, mod_reg.clone())
        .with_schedule(ScheduleId::UPDATE)
        .with_in_sets(vec!["sleak::su3".to_owned()]);
    let u4 = GuestSystem::new("u4", vec![], bump_cb, other_ptr, mod_reg)
        .with_schedule(ScheduleId::UPDATE)
        .with_in_sets(vec!["sleak::su4".to_owned()])
        .with_ordering(vec![], vec!["sleak::u3".to_owned()]);
    mount_mod_systems_in_world(app.world_mut(), "sleak", 0, vec![u1, u2, u3, u4]).unwrap();

    app.world_mut().run_schedule(Update);
    assert_eq!(u1_hits.load(Ordering::SeqCst), 1);
    assert_eq!(other_hits.load(Ordering::SeqCst), 2);

    let delta_path = temp.0.join("sleak.schedule_delta.json");
    std::fs::write(
        &delta_path,
        r#"{
            "schedule": "Update",
            "sets": [
                {"set": "sleak::su1", "parent_sets": ["sleak::su2"]},
                {"set": "sleak::su3", "before": ["sleak::su4"]}
            ]
        }"#,
    )
    .expect("write single-schedule delta with parent_sets + cycle");

    let out = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(out.len(), 1);
    assert!(
        matches!(&out[0].action, ModHotReloadAction::Failed(_)),
        "expected Failed when entry 2 creates FlatDependencySort cycle, got {out:?}"
    );
    {
        let reg = app.world().resource::<ModScheduleSetRegistry>();
        assert!(reg.get(su1_h).unwrap().parent_sets.is_empty());
        assert!(reg.get(su3_h).unwrap().before_sets.is_empty());
    }
    rewire_mod_schedule_sets_in_world(app.world_mut(), ScheduleId::UPDATE)
        .expect("Update schedule must remain healthy");
    app.world_mut().run_schedule(Update);
    assert_eq!(
        u1_hits.load(Ordering::SeqCst),
        2,
        "su1.in_set(su2) must not leak into Update ScheduleGraph when entry 2 fails"
    );
}

#[test]
fn schedule_delta_multi_schedule_parent_sets_is_atomic_on_later_schedule_failure() {
    let temp = TempDirGuard::new("bam_watcher_r43_mpleak");
    let watcher = ModHotReloadWatcher::watch_with_options(
        &temp.0,
        Duration::from_millis(20),
        false,
        LoadOptions::default(),
    )
    .expect("start watcher")
    .with_stability_polls(2);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    struct OrderLog {
        entries: Mutex<Vec<&'static str>>,
    }
    struct LogItem {
        log: Arc<OrderLog>,
        label: &'static str,
    }

    unsafe extern "C" fn log_cb(_sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        // SAFETY: `ud` points to `LogItem` kept alive for the duration of the test.
        let item = unsafe { &*(ud.cast::<LogItem>()) };
        item.log.entries.lock().unwrap().push(item.label);
        FfiResult::OK
    }
    unsafe extern "C" fn always_false(_sys_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        FfiResult::INVALID_STATE
    }

    let log = Arc::new(OrderLog {
        entries: Mutex::new(Vec::new()),
    });
    let items: Vec<LogItem> = ["u1", "u2", "p1", "p2"]
        .into_iter()
        .map(|label| LogItem {
            log: Arc::clone(&log),
            label,
        })
        .collect();

    let (su1_h, sp1_h) = {
        let mut reg = app.world_mut().resource_mut::<ModScheduleSetRegistry>();
        let su1 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("mpleak"),
                set_name: Arc::from("su1"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        let su2 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("mpleak"),
                set_name: Arc::from("su2"),
                generation: 0,
                schedule: ScheduleId::UPDATE,
                mode: ScheduleSetMode::Parallel,
                run_if_ctx: Some(always_false),
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        let sp1 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("mpleak"),
                set_name: Arc::from("sp1"),
                generation: 0,
                schedule: ScheduleId::POST_UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        let sp2 = reg
            .configure_set(ScheduleSetConfig {
                mod_id: None,
                mod_name: Arc::from("mpleak"),
                set_name: Arc::from("sp2"),
                generation: 0,
                schedule: ScheduleId::POST_UPDATE,
                mode: ScheduleSetMode::Parallel,
                ..ScheduleSetConfig::default()
            })
            .unwrap();
        reg.mount_system(
            su1,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("mpleak", "u1"),
        )
        .unwrap();
        reg.mount_system(
            su2,
            ScheduleId::UPDATE,
            ModNamedSystemSet::new("mpleak", "u2"),
        )
        .unwrap();
        reg.mount_system(
            sp1,
            ScheduleId::POST_UPDATE,
            ModNamedSystemSet::new("mpleak", "p1"),
        )
        .unwrap();
        reg.mount_system(
            sp2,
            ScheduleId::POST_UPDATE,
            ModNamedSystemSet::new("mpleak", "p2"),
        )
        .unwrap();
        (su1, sp1)
    };

    let mod_reg = app.world().resource::<ModRegistry>().clone();
    let u1 = GuestSystem::new(
        "u1",
        vec![],
        log_cb,
        (&raw const items[0]).cast_mut().cast::<c_void>(),
        mod_reg.clone(),
    )
    .with_schedule(ScheduleId::UPDATE)
    .with_in_sets(vec!["mpleak::su1".to_owned()]);
    let u2 = GuestSystem::new(
        "u2",
        vec![],
        log_cb,
        (&raw const items[1]).cast_mut().cast::<c_void>(),
        mod_reg.clone(),
    )
    .with_schedule(ScheduleId::UPDATE)
    .with_in_sets(vec!["mpleak::su2".to_owned()]);
    let p1 = GuestSystem::new(
        "p1",
        vec![],
        log_cb,
        (&raw const items[2]).cast_mut().cast::<c_void>(),
        mod_reg.clone(),
    )
    .with_schedule(ScheduleId::POST_UPDATE)
    .with_in_sets(vec!["mpleak::sp1".to_owned()]);
    let p2 = GuestSystem::new(
        "p2",
        vec![],
        log_cb,
        (&raw const items[3]).cast_mut().cast::<c_void>(),
        mod_reg,
    )
    .with_schedule(ScheduleId::POST_UPDATE)
    .with_in_sets(vec!["mpleak::sp2".to_owned()])
    .with_ordering(vec![], vec!["mpleak::p1".to_owned()]);
    mount_mod_systems_in_world(app.world_mut(), "mpleak", 0, vec![u1, u2, p1, p2]).unwrap();

    app.world_mut().run_schedule(Update);
    assert_eq!(*log.entries.lock().unwrap(), vec!["u1"]);
    log.entries.lock().unwrap().clear();

    app.world_mut().run_schedule(PostUpdate);
    assert_eq!(*log.entries.lock().unwrap(), vec!["p2", "p1"]);
    log.entries.lock().unwrap().clear();

    let delta_path = temp.0.join("mpleak.schedule_delta.json");
    std::fs::write(
        &delta_path,
        r#"{
            "schedule": "Update",
            "sets": [
                {"schedule": "Update", "set": "mpleak::su1", "parent_sets": ["mpleak::su2"]},
                {"schedule": "PostUpdate", "set": "mpleak::sp1", "before": ["mpleak::sp2"]}
            ]
        }"#,
    )
    .expect("write multi-schedule delta with parent_sets on Update + cycle on PostUpdate");

    let out = wait_for_outcomes(&watcher, app.world_mut());
    assert_eq!(out.len(), 1);
    assert!(
        matches!(&out[0].action, ModHotReloadAction::Failed(_)),
        "expected Failed when PostUpdate hits cycle, got {out:?}"
    );
    {
        let reg = app.world().resource::<ModScheduleSetRegistry>();
        assert!(reg.get(su1_h).unwrap().parent_sets.is_empty());
        assert!(reg.get(sp1_h).unwrap().before_sets.is_empty());
    }

    app.world_mut().run_schedule(Update);
    assert_eq!(
        *log.entries.lock().unwrap(),
        vec!["u1"],
        "su1.in_set(su2) must not leak into Update ScheduleGraph when PostUpdate fails"
    );
    log.entries.lock().unwrap().clear();

    app.world_mut().run_schedule(PostUpdate);
    assert_eq!(*log.entries.lock().unwrap(), vec!["p2", "p1"]);
}
