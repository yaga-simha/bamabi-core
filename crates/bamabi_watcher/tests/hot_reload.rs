use bamabi_host::app_ext::ModAppExt;
use bamabi_host::events::{ModLoaded, ModUnloaded};
use bamabi_host::plugin::AbiModPlugin;
use bamabi_host::world_ext::ModWorldExt;
use bamabi_host::{ModHost, ModLifecycleFlush, StartupCompleted};
use bamabi_host_sys::LoadOptions;
use bamabi_macros::ModComponent;
use bamabi_watcher::{
    ModHotReloadAction, ModHotReloadHistory, ModHotReloadPlugin, ModHotReloadWatcher,
};
use bevy_app::{App, PreUpdate, Update};
use bevy_ecs::message::{MessageCursor, Messages};
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::Schedules;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[repr(C)]
#[derive(Resource, ModComponent, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[mod_component(name = "watcher_test::WatchCounter")]
struct WatchCounter {
    value: u32,
}

struct TempDirGuard(PathBuf);

impl TempDirGuard {
    fn new(prefix: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("{prefix}_{}_{}", std::process::id(), id));
        std::fs::create_dir_all(&dir).expect("create temp test dir");
        Self(dir)
    }
}

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn find_c_compiler() -> Option<String> {
    if let Ok(cc) = std::env::var("CC")
        && !cc.trim().is_empty()
    {
        let cc = cc.trim().to_owned();
        let is_cl = Path::new(&cc)
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("cl") || n.eq_ignore_ascii_case("cl.exe"));
        let res = if is_cl {
            Command::new(&cc).output()
        } else {
            Command::new(&cc).arg("--version").output()
        };
        if res.is_ok() {
            return Some(cc);
        }
        return None;
    }

    #[cfg(target_os = "windows")]
    let cands = ["clang", "clang-cl", "cl.exe", "cl", "gcc", "cc"];
    #[cfg(not(target_os = "windows"))]
    let cands = ["clang", "gcc", "cc"];

    cands
        .iter()
        .copied()
        .find(|cand| {
            let is_cl = Path::new(cand)
                .file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|n| n.eq_ignore_ascii_case("cl") || n.eq_ignore_ascii_case("cl.exe"));
            let res = if is_cl {
                Command::new(cand).output()
            } else {
                Command::new(cand).arg("--version").output()
            };
            matches!(res, Ok(out) if out.status.success() || (is_cl && !out.stderr.is_empty()))
        })
        .map(str::to_owned)
}

fn compile_c_source_cdylib(src_dir: &Path, c_src: &Path, out_dylib: &Path, tag: &str) -> bool {
    let Some(cc) = find_c_compiler() else {
        eprintln!("skipping watcher integration test: no working C compiler found on PATH");
        return false;
    };

    std::fs::write(
        src_dir.join("bamabi.h"),
        bamabi_sdk_gen::render_c99_header().expect("render bamabi.h"),
    )
    .expect("write bamabi.h");

    let staging = src_dir.join(format!("{tag}_tmp_out.{}", std::env::consts::DLL_EXTENSION));
    let is_msvc = Path::new(&cc)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| {
            let s = n
                .strip_suffix(".exe")
                .or_else(|| n.strip_suffix(".EXE"))
                .unwrap_or(n);
            s.eq_ignore_ascii_case("cl") || s.eq_ignore_ascii_case("clang-cl")
        });
    let mut cmd = Command::new(&cc);
    if is_msvc {
        let obj_path = src_dir.join(format!("{tag}_tmp.obj"));
        let implib_path = src_dir.join(format!("{tag}_tmp.lib"));
        cmd.args(["/nologo", "/LD", "/std:c11", "/W4", "/WX", "/O2"])
            .arg(format!("/I{}", src_dir.display()))
            .arg(format!("/Fo{}", obj_path.display()))
            .arg(c_src)
            .arg("/link")
            .arg(format!("/IMPLIB:{}", implib_path.display()))
            .arg(format!("/OUT:{}", staging.display()));
    } else {
        cmd.args(["-std=c99", "-Wall", "-Wextra", "-Werror"])
            .arg(format!("-I{}", src_dir.display()))
            .args(["-O2", "-shared"]);
        if !cfg!(target_os = "windows") {
            cmd.arg("-fPIC");
        }
        cmd.arg(c_src).arg("-o").arg(&staging);
    }
    let out = cmd.output().expect("execute C compiler");
    assert!(
        out.status.success(),
        "C compiler failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    if std::fs::rename(&staging, out_dylib).is_err() {
        std::fs::copy(&staging, out_dylib).expect("copy compiled dylib");
        let _ = std::fs::remove_file(&staging);
    }
    true
}

fn compile_c_watch_mod_in_schedule(
    src_dir: &Path,
    out_dylib: &Path,
    mod_name: &str,
    mod_version: &str,
    schedule_macro: &str,
    delta: u32,
) -> bool {
    let c_src = src_dir.join(format!("{mod_name}.c"));
    let code = format!(
        r#"#include "bamabi.h"
#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef struct WatchCounter {{
    uint32_t value;
}} WatchCounter;

typedef struct ModState {{
    BamHostApi api;
    BamComponent counter_res;
}} ModState;

static ModState g_state;

static BamResult tick_cb(BamSystemCtx* sys_ctx, void* ud) {{
    (void)ud;
    WatchCounter* c = (WatchCounter*)g_state.api.sys_res_mut_ptr(sys_ctx, g_state.counter_res, NULL);
    if (!c) return BAM_RESULT_NULL_POINTER;
    c->value += {delta}u;
    return BAM_RESULT_OK;
}}

static const BamManifest MANIFEST = {{
    (uint32_t)sizeof(BamManifest),
    BAM_ABI_VERSION,
    "{mod_name}",
    "{mod_version}",
    0u,
    0u,
    NULL,
    NULL
}};

BAM_EXPORT const BamManifest* bamabi_manifest(void) {{
    return &MANIFEST;
}}

BAM_EXPORT BamResult bamabi_init(const BamHostApi* api, BamLoadCtx* load_ctx, BamInstance** out) {{
    if (!api || !load_ctx || !out) return BAM_RESULT_NULL_POINTER;
    g_state.api = *api;
    const BamHostCtx* hctx = api->load_host_ctx(load_ctx);
    BamResult r = api->resolve_component(hctx, "watcher_test::WatchCounter", NULL, &g_state.counter_res);
    if (r != BAM_RESULT_OK) return r;
    BamTermDesc term = bam_term_desc(g_state.counter_res, BAM_ACCESS_RESOURCE_WRITE, 0u);
    BamQueryDesc q;
    memset(&q, 0, sizeof(q));
    q.struct_size = (uint32_t)sizeof(BamQueryDesc);
    q.flags = 0u;
    q.terms = &term;
    q.term_count = 1u;
    BamSystemDesc sys;
    memset(&sys, 0, sizeof(sys));
    sys.struct_size = (uint32_t)sizeof(BamSystemDesc);
    sys.name = "{mod_name}_tick";
    sys.queries = &q;
    sys.query_count = 1u;
    sys.callback = tick_cb;
    sys.schedule = {schedule_macro};
    r = api->register_system(load_ctx, &sys);
    if (r != BAM_RESULT_OK) return r;
    *out = (BamInstance*)&g_state;
    return BAM_RESULT_OK;
}}

BAM_EXPORT void bamabi_shutdown(BamInstance* instance) {{
    (void)instance;
}}
"#
    );
    std::fs::write(&c_src, code).expect("write C mod source");
    compile_c_source_cdylib(src_dir, &c_src, out_dylib, mod_name)
}

fn compile_c_watch_mod(
    src_dir: &Path,
    out_dylib: &Path,
    mod_name: &str,
    mod_version: &str,
    delta: u32,
) -> bool {
    compile_c_watch_mod_in_schedule(
        src_dir,
        out_dylib,
        mod_name,
        mod_version,
        "BAM_SCHEDULE_UPDATE",
        delta,
    )
}

fn compile_c_startup_and_update_mod(
    src_dir: &Path,
    out_dylib: &Path,
    mod_name: &str,
    mod_version: &str,
    startup_delta: u32,
    update_delta: u32,
) -> bool {
    let c_src = src_dir.join(format!("{mod_name}_startup.c"));
    let code = format!(
        r#"#include "bamabi.h"
#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef struct WatchCounter {{
    uint32_t value;
}} WatchCounter;

typedef struct ModState {{
    BamHostApi api;
    BamComponent counter_res;
}} ModState;

static ModState g_state;

static BamResult startup_cb(BamSystemCtx* sys_ctx, void* ud) {{
    (void)ud;
    WatchCounter* c = (WatchCounter*)g_state.api.sys_res_mut_ptr(sys_ctx, g_state.counter_res, NULL);
    if (!c) return BAM_RESULT_NULL_POINTER;
    c->value += {startup_delta}u;
    return BAM_RESULT_OK;
}}

static BamResult update_cb(BamSystemCtx* sys_ctx, void* ud) {{
    (void)ud;
    WatchCounter* c = (WatchCounter*)g_state.api.sys_res_mut_ptr(sys_ctx, g_state.counter_res, NULL);
    if (!c) return BAM_RESULT_NULL_POINTER;
    c->value += {update_delta}u;
    return BAM_RESULT_OK;
}}

static const BamManifest MANIFEST = {{
    (uint32_t)sizeof(BamManifest),
    BAM_ABI_VERSION,
    "{mod_name}",
    "{mod_version}",
    0u,
    0u,
    NULL,
    NULL
}};

BAM_EXPORT const BamManifest* bamabi_manifest(void) {{
    return &MANIFEST;
}}

BAM_EXPORT BamResult bamabi_init(const BamHostApi* api, BamLoadCtx* load_ctx, BamInstance** out) {{
    if (!api || !load_ctx || !out) return BAM_RESULT_NULL_POINTER;
    g_state.api = *api;
    const BamHostCtx* hctx = api->load_host_ctx(load_ctx);
    BamResult r = api->resolve_component(hctx, "watcher_test::WatchCounter", NULL, &g_state.counter_res);
    if (r != BAM_RESULT_OK) return r;
    BamTermDesc term = bam_term_desc(g_state.counter_res, BAM_ACCESS_RESOURCE_WRITE, 0u);
    BamQueryDesc q;
    memset(&q, 0, sizeof(q));
    q.struct_size = (uint32_t)sizeof(BamQueryDesc);
    q.flags = 0u;
    q.terms = &term;
    q.term_count = 1u;

    BamSystemDesc s_sys;
    memset(&s_sys, 0, sizeof(s_sys));
    s_sys.struct_size = (uint32_t)sizeof(BamSystemDesc);
    s_sys.name = "{mod_name}_startup";
    s_sys.queries = &q;
    s_sys.query_count = 1u;
    s_sys.callback = startup_cb;
    s_sys.schedule = BAM_SCHEDULE_STARTUP;
    r = api->register_system(load_ctx, &s_sys);
    if (r != BAM_RESULT_OK) return r;

    BamSystemDesc u_sys;
    memset(&u_sys, 0, sizeof(u_sys));
    u_sys.struct_size = (uint32_t)sizeof(BamSystemDesc);
    u_sys.name = "{mod_name}_update";
    u_sys.queries = &q;
    u_sys.query_count = 1u;
    u_sys.callback = update_cb;
    u_sys.schedule = BAM_SCHEDULE_UPDATE;
    r = api->register_system(load_ctx, &u_sys);
    if (r != BAM_RESULT_OK) return r;

    *out = (BamInstance*)&g_state;
    return BAM_RESULT_OK;
}}

BAM_EXPORT void bamabi_shutdown(BamInstance* instance) {{
    (void)instance;
}}
"#
    );
    std::fs::write(&c_src, code).expect("write C startup mod source");
    compile_c_source_cdylib(src_dir, &c_src, out_dylib, mod_name)
}

fn dylib_filename(stem: &str) -> String {
    let prefix = std::env::consts::DLL_PREFIX;
    let ext = std::env::consts::DLL_EXTENSION;
    format!("{prefix}{stem}.{ext}")
}

#[test]
fn watcher_live_reloads_modified_shared_library() {
    let temp = TempDirGuard::new("bam_hot_reload_watcher");
    let mods_dir = temp.0.join("mods");
    let build_dir = temp.0.join("build");
    std::fs::create_dir_all(&mods_dir).unwrap();
    std::fs::create_dir_all(&build_dir).unwrap();

    let dylib_path = mods_dir.join(dylib_filename("watch_mod"));
    if !compile_c_watch_mod(&build_dir, &dylib_path, "watch_mod", "0.2.0", 1) {
        return;
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    // Allow OS filesystem event streams (notably macOS FSEvents) to flush initial
    // fixture-creation events before starting the watcher and before recording v2 reload.
    std::thread::sleep(Duration::from_millis(300));
    app.add_plugins(ModHotReloadPlugin::new(&mods_dir).with_debounce(Duration::from_millis(50)));
    app.insert_resource(WatchCounter::default());
    app.register_mod_resource::<WatchCounter>().unwrap();

    app.load_mod_with(&dylib_path, LoadOptions::new().with_shadow_copy(true))
        .expect("initial load");
    std::thread::sleep(Duration::from_millis(200));
    app.update();
    app.world_mut()
        .resource_mut::<ModHotReloadHistory>()
        .outcomes
        .clear();
    app.world_mut().resource_mut::<WatchCounter>().value = 0;
    app.update();
    assert_eq!(app.world().resource::<WatchCounter>().value, 1);

    std::thread::sleep(Duration::from_millis(120));
    assert!(compile_c_watch_mod(
        &build_dir,
        &dylib_path,
        "watch_mod",
        "0.2.0",
        10
    ));

    let mut reloaded = false;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(80));
        app.update();
        if app
            .world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .iter()
            .any(|o| o.action == ModHotReloadAction::Reloaded)
        {
            reloaded = true;
            break;
        }
    }
    assert!(
        reloaded,
        "ModHotReloadPlugin did not detect and reload modified shared library"
    );

    let before = app.world().resource::<WatchCounter>().value;
    app.update();
    let after = app.world().resource::<WatchCounter>().value;
    assert_eq!(
        after - before,
        10,
        "reloaded v2 system must increment WatchCounter by 10 per tick"
    );
}

#[test]
fn watcher_cascades_dag_reload_on_base_mod_rebuild_and_rejects_broken_semver_edit() {
    let temp = TempDirGuard::new("bam_hot_reload_dag");
    let mods_dir = temp.0.join("mods");
    let base_dir = mods_dir.join("base_mod");
    let addon_dir = mods_dir.join("addon_mod");
    let build_dir = temp.0.join("build");
    std::fs::create_dir_all(&base_dir).unwrap();
    std::fs::create_dir_all(&addon_dir).unwrap();
    std::fs::create_dir_all(&build_dir).unwrap();

    let base_dylib = base_dir.join(dylib_filename("base_mod"));
    let addon_dylib = addon_dir.join(dylib_filename("addon_mod"));

    std::fs::write(
        base_dir.join("mod.toml"),
        "[mod]\nid = \"base_mod\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n",
    )
    .unwrap();
    std::fs::write(
        addon_dir.join("mod.toml"),
        "[mod]\nid = \"addon_mod\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n\n[dependencies]\nbase_mod = \"^1.0\"\n",
    )
    .unwrap();

    if !compile_c_watch_mod(&build_dir, &base_dylib, "base_mod", "1.0.0", 2) {
        return;
    }
    assert!(compile_c_watch_mod(
        &build_dir,
        &addon_dylib,
        "addon_mod",
        "1.0.0",
        3
    ));

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(WatchCounter::default());
    app.register_mod_resource::<WatchCounter>().unwrap();

    let report = app
        .world_mut()
        .load_mods_dir_with(&mods_dir, LoadOptions::new().with_shadow_copy(true))
        .expect("load_mods_dir");
    assert_eq!(report.loaded.len(), 2);
    assert_eq!(report.loaded[0].0, "base_mod");
    assert_eq!(report.loaded[1].0, "addon_mod");

    // Allow OS filesystem event streams (notably macOS FSEvents) to flush initial
    // fixture-creation events before starting the watcher and before recording deltas.
    std::thread::sleep(Duration::from_millis(300));
    app.add_plugins(ModHotReloadPlugin::new(&mods_dir).with_debounce(Duration::from_millis(50)));
    std::thread::sleep(Duration::from_millis(200));
    app.update();

    let mut unload_cursor = MessageCursor::<ModUnloaded>::default();
    let mut load_cursor = MessageCursor::<ModLoaded>::default();
    {
        let msgs_u = app.world().resource::<Messages<ModUnloaded>>();
        let _ = unload_cursor.read(msgs_u).count();
        let msgs_l = app.world().resource::<Messages<ModLoaded>>();
        let _ = load_cursor.read(msgs_l).count();
        app.world_mut()
            .resource_mut::<ModHotReloadHistory>()
            .outcomes
            .clear();
        app.world_mut().resource_mut::<WatchCounter>().value = 0;
    }

    app.update();
    assert_eq!(app.world().resource::<WatchCounter>().value, 5); // 2 + 3

    // Rebuild ONLY `base_mod` on disk with delta = 20 -> cascading reload must reload
    // `addon_mod` -> `base_mod` (unload) then `base_mod` -> `addon_mod` (load)!
    std::thread::sleep(Duration::from_millis(120));
    assert!(compile_c_watch_mod(
        &build_dir,
        &base_dylib,
        "base_mod",
        "1.0.0",
        20
    ));

    let mut reloaded = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(80));
        app.update();
        if app
            .world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .iter()
            .any(|o| o.action == ModHotReloadAction::Reloaded)
        {
            reloaded = true;
            break;
        }
    }
    assert!(
        reloaded,
        "ModHotReloadWatcher must cascade reload when base_mod changes"
    );

    let unloaded_order: Vec<String> = {
        let msgs = app.world().resource::<Messages<ModUnloaded>>();
        unload_cursor.read(msgs).map(|m| m.name.clone()).collect()
    };
    let loaded_order: Vec<String> = {
        let msgs = app.world().resource::<Messages<ModLoaded>>();
        load_cursor.read(msgs).map(|m| m.name.clone()).collect()
    };
    assert_eq!(unloaded_order, vec!["addon_mod", "base_mod"]);
    assert_eq!(loaded_order, vec!["base_mod", "addon_mod"]);

    let before = app.world().resource::<WatchCounter>().value;
    app.update();
    let after = app.world().resource::<WatchCounter>().value;
    assert_eq!(
        after - before,
        23,
        "cascaded reload must run v2 base_mod (20) + addon_mod (3)"
    );

    // Introducing a SemVer mismatch in `addon_mod/mod.toml` must fail safely with a
    // `Failed` outcome while keeping both active mods loaded and running!
    std::thread::sleep(Duration::from_millis(120));
    std::fs::write(
        addon_dir.join("mod.toml"),
        "[mod]\nid = \"addon_mod\"\nversion = \"1.0.0\"\n\n[dependencies]\nbase_mod = \"^9.0\"\n",
    )
    .unwrap();

    let mut saw_failure = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(80));
        app.update();
        if app
            .world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .iter()
            .any(|o| matches!(o.action, ModHotReloadAction::Failed(_)))
        {
            saw_failure = true;
            break;
        }
    }
    assert!(
        saw_failure,
        "invalid mod.toml SemVer edit must record Failed outcome"
    );
    assert_eq!(app.world().resource::<ModHost>().len(), 2);
    let before_after_err = app.world().resource::<WatchCounter>().value;
    app.update();
    let after_after_err = app.world().resource::<WatchCounter>().value;
    assert_eq!(after_after_err - before_after_err, 23);
}

#[test]
fn delete_then_recreate_within_debounce_window_does_not_unload() {
    let temp = TempDirGuard::new("bam_delete_recreate");
    let mods_dir = temp.0.join("mods");
    let build_dir = temp.0.join("build");
    std::fs::create_dir_all(&mods_dir).unwrap();
    std::fs::create_dir_all(&build_dir).unwrap();

    let dylib_path = mods_dir.join(dylib_filename("recreate_mod"));
    let v2_staged = build_dir.join(dylib_filename("recreate_mod_v2"));
    if !compile_c_watch_mod(&build_dir, &dylib_path, "recreate_mod", "0.1.0", 1) {
        return;
    }
    assert!(compile_c_watch_mod(
        &build_dir,
        &v2_staged,
        "recreate_mod",
        "0.2.0",
        10
    ));

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(WatchCounter::default());
    app.register_mod_resource::<WatchCounter>().unwrap();
    app.load_mod_with(&dylib_path, LoadOptions::new().with_shadow_copy(true))
        .expect("initial load");

    // Flush initial FSEvents before starting watcher and clearing history.
    std::thread::sleep(Duration::from_millis(300));
    app.add_plugins(ModHotReloadPlugin::new(&mods_dir).with_debounce(Duration::from_millis(350)));
    std::thread::sleep(Duration::from_millis(450));
    app.update();
    app.world_mut()
        .resource_mut::<ModHotReloadHistory>()
        .outcomes
        .clear();
    app.world_mut().resource_mut::<WatchCounter>().value = 0;

    app.update();
    assert_eq!(app.world().resource::<WatchCounter>().value, 1);

    // Delete and recreate well inside the 350ms debounce + grace window using pre-compiled v2.
    std::fs::remove_file(&dylib_path).expect("remove v1 dylib");
    std::thread::sleep(Duration::from_millis(25));
    app.update();
    std::fs::copy(&v2_staged, &dylib_path).expect("recreate v2 dylib");

    let mut reloaded = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(80));
        app.update();
        if app
            .world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .iter()
            .any(|o| o.action == ModHotReloadAction::Reloaded)
        {
            reloaded = true;
            break;
        }
    }
    assert!(reloaded, "recreated dylib must trigger Reloaded");

    let history = &app.world().resource::<ModHotReloadHistory>().outcomes;
    assert!(
        !history
            .iter()
            .any(|o| o.action == ModHotReloadAction::Unloaded),
        "delete-then-recreate within debounce window must not emit Unloaded: {history:?}"
    );
    assert_eq!(app.world().resource::<ModHost>().len(), 1);

    let before = app.world().resource::<WatchCounter>().value;
    app.update();
    let after = app.world().resource::<WatchCounter>().value;
    assert_eq!(after - before, 10);
}

#[test]
fn with_unload_on_delete_false_keeps_mod_loaded() {
    let temp = TempDirGuard::new("bam_unload_on_delete_false");
    let mods_dir = temp.0.join("mods");
    let build_dir = temp.0.join("build");
    std::fs::create_dir_all(&mods_dir).unwrap();
    std::fs::create_dir_all(&build_dir).unwrap();

    let dylib_path = mods_dir.join(dylib_filename("keep_loaded_mod"));
    if !compile_c_watch_mod(&build_dir, &dylib_path, "keep_loaded_mod", "0.1.0", 7) {
        return;
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(WatchCounter::default());
    app.register_mod_resource::<WatchCounter>().unwrap();
    app.load_mod_with(&dylib_path, LoadOptions::new().with_shadow_copy(true))
        .expect("initial load");

    std::thread::sleep(Duration::from_millis(300));
    app.add_plugins(
        ModHotReloadPlugin::new(&mods_dir)
            .with_debounce(Duration::from_millis(50))
            .with_unload_on_delete(false),
    );
    std::thread::sleep(Duration::from_millis(200));
    app.update();
    app.world_mut()
        .resource_mut::<ModHotReloadHistory>()
        .outcomes
        .clear();
    app.world_mut().resource_mut::<WatchCounter>().value = 0;

    std::fs::remove_file(&dylib_path).expect("delete dylib on disk");
    for _ in 0..16 {
        std::thread::sleep(Duration::from_millis(60));
        app.update();
    }

    assert_eq!(app.world().resource::<ModHost>().len(), 1);
    let history = &app.world().resource::<ModHotReloadHistory>().outcomes;
    assert!(
        !history
            .iter()
            .any(|o| o.action == ModHotReloadAction::Unloaded
                || matches!(o.action, ModHotReloadAction::Failed(_))),
        "with_unload_on_delete(false) must neither unload nor fail: {history:?}"
    );

    let before = app.world().resource::<WatchCounter>().value;
    app.update();
    let after = app.world().resource::<WatchCounter>().value;
    assert_eq!(after - before, 7);
}

#[test]
fn non_recursive_mode_ignores_nested_dirs() {
    let temp = TempDirGuard::new("bam_non_recursive");
    let mods_dir = temp.0.join("mods");
    let nested_dir = mods_dir.join("nested");
    let build_dir = temp.0.join("build");
    std::fs::create_dir_all(&nested_dir).unwrap();
    std::fs::create_dir_all(&build_dir).unwrap();

    let nested_dylib_staged = build_dir.join(dylib_filename("nested_mod"));
    let top_dylib_staged = build_dir.join(dylib_filename("top_mod"));
    if !compile_c_watch_mod(&build_dir, &nested_dylib_staged, "nested_mod", "0.1.0", 5) {
        return;
    }
    assert!(compile_c_watch_mod(
        &build_dir,
        &top_dylib_staged,
        "top_mod",
        "0.1.0",
        9
    ));

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(WatchCounter::default());
    app.register_mod_resource::<WatchCounter>().unwrap();

    std::thread::sleep(Duration::from_millis(250));
    app.add_plugins(
        ModHotReloadPlugin::new(&mods_dir)
            .with_recursive(false)
            .with_debounce(Duration::from_millis(50)),
    );
    std::thread::sleep(Duration::from_millis(200));
    app.update();

    let nested_dylib = nested_dir.join(dylib_filename("nested_mod"));
    std::fs::copy(&nested_dylib_staged, &nested_dylib).expect("copy nested dylib");

    for _ in 0..14 {
        std::thread::sleep(Duration::from_millis(60));
        app.update();
    }

    assert!(
        app.world().resource::<ModHost>().is_empty(),
        "non-recursive watcher must ignore mods in nested directories"
    );
    assert!(
        app.world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .is_empty()
    );
    assert_eq!(app.world().resource::<WatchCounter>().value, 0);

    // Top-level directory changes are still picked up by the non-recursive watcher.
    let top_dylib = mods_dir.join(dylib_filename("top_mod"));
    std::fs::copy(&top_dylib_staged, &top_dylib).expect("copy top-level dylib");

    let mut top_loaded = false;
    for _ in 0..30 {
        std::thread::sleep(Duration::from_millis(80));
        app.update();
        if app
            .world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .iter()
            .any(|o| o.action == ModHotReloadAction::Loaded)
        {
            top_loaded = true;
            break;
        }
    }
    assert!(
        top_loaded,
        "non-recursive watcher must still load top-level dylib"
    );
    assert_eq!(app.world().resource::<ModHost>().len(), 1);
    let before = app.world().resource::<WatchCounter>().value;
    app.update();
    assert_eq!(app.world().resource::<WatchCounter>().value - before, 9);
}

#[test]
fn broken_mod_toml_edit_produces_failed_and_keeps_old_mod() {
    let temp = TempDirGuard::new("bam_broken_toml");
    let mods_dir = temp.0.join("mods");
    let cfg_dir = mods_dir.join("cfg_mod");
    let build_dir = temp.0.join("build");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::create_dir_all(&build_dir).unwrap();

    let cfg_dylib = cfg_dir.join(dylib_filename("cfg_mod"));
    let cfg_toml = cfg_dir.join("mod.toml");
    std::fs::write(
        &cfg_toml,
        "[mod]\nid = \"cfg_mod\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n",
    )
    .unwrap();
    if !compile_c_watch_mod(&build_dir, &cfg_dylib, "cfg_mod", "1.0.0", 4) {
        return;
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(WatchCounter::default());
    app.register_mod_resource::<WatchCounter>().unwrap();

    let report = app
        .world_mut()
        .load_mods_dir_with(&mods_dir, LoadOptions::new().with_shadow_copy(true))
        .expect("load cfg_mod");
    assert_eq!(report.loaded.len(), 1);

    std::thread::sleep(Duration::from_millis(300));
    app.add_plugins(ModHotReloadPlugin::new(&mods_dir).with_debounce(Duration::from_millis(50)));
    std::thread::sleep(Duration::from_millis(200));
    app.update();
    app.world_mut()
        .resource_mut::<ModHotReloadHistory>()
        .outcomes
        .clear();
    app.world_mut().resource_mut::<WatchCounter>().value = 0;

    app.update();
    assert_eq!(app.world().resource::<WatchCounter>().value, 4);

    // Edit `mod.toml` to syntactically invalid TOML.
    std::thread::sleep(Duration::from_millis(120));
    std::fs::write(&cfg_toml, "[mod]\nid = \"unclosed\n").unwrap();

    let mut saw_failed = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(80));
        app.update();
        if app
            .world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .iter()
            .any(|o| matches!(o.action, ModHotReloadAction::Failed(_)))
        {
            saw_failed = true;
            break;
        }
    }
    assert!(
        saw_failed,
        "broken mod.toml edit must record ModHotReloadAction::Failed"
    );
    assert_eq!(app.world().resource::<ModHost>().len(), 1);
    let before = app.world().resource::<WatchCounter>().value;
    app.update();
    assert_eq!(app.world().resource::<WatchCounter>().value - before, 4);
}

#[test]
fn watcher_runs_in_mod_lifecycle_flush_and_reloads_pre_update_mod() {
    let temp = TempDirGuard::new("bam_pre_update_flush");
    let mods_dir = temp.0.join("mods");
    let build_dir = temp.0.join("build");
    std::fs::create_dir_all(&mods_dir).unwrap();
    std::fs::create_dir_all(&build_dir).unwrap();

    let dylib_path = mods_dir.join(dylib_filename("pre_update_mod"));
    if !compile_c_watch_mod_in_schedule(
        &build_dir,
        &dylib_path,
        "pre_update_mod",
        "0.1.0",
        "BAM_SCHEDULE_PRE_UPDATE",
        1,
    ) {
        return;
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(WatchCounter::default());
    app.register_mod_resource::<WatchCounter>().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    app.add_plugins(ModHotReloadPlugin::new(&mods_dir).with_debounce(Duration::from_millis(50)));

    // Watcher system lives in `ModLifecycleFlush` (not `PreUpdate`), alongside the deferred flush system.
    let flush_len = app
        .world()
        .resource::<Schedules>()
        .get(ModLifecycleFlush)
        .expect("ModLifecycleFlush schedule exists")
        .systems_len();
    assert!(
        flush_len >= 2,
        "expected ModLifecycleFlush to hold both lifecycle flush and watcher poller, got {flush_len}"
    );
    let pre_update_len_before = app
        .world()
        .resource::<Schedules>()
        .get(PreUpdate)
        .map_or(0, |s| s.systems_len());
    assert_eq!(
        pre_update_len_before, 0,
        "PreUpdate must not contain watcher systems"
    );

    app.load_mod_with(&dylib_path, LoadOptions::new().with_shadow_copy(true))
        .expect("load PreUpdate mod");
    std::thread::sleep(Duration::from_millis(200));
    app.update();
    app.world_mut()
        .resource_mut::<ModHotReloadHistory>()
        .outcomes
        .clear();
    app.world_mut().resource_mut::<WatchCounter>().value = 0;

    app.update();
    assert_eq!(app.world().resource::<WatchCounter>().value, 1);

    // Modify the PreUpdate mod -> ModLifecycleFlush reloads it cleanly without ScheduleLeased or UAF!
    std::thread::sleep(Duration::from_millis(120));
    assert!(compile_c_watch_mod_in_schedule(
        &build_dir,
        &dylib_path,
        "pre_update_mod",
        "0.2.0",
        "BAM_SCHEDULE_PRE_UPDATE",
        10,
    ));

    let mut reloaded = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(80));
        app.update();
        if app
            .world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .iter()
            .any(|o| o.action == ModHotReloadAction::Reloaded)
        {
            reloaded = true;
            break;
        }
    }
    assert!(
        reloaded,
        "PreUpdate mod must hot-reload in ModLifecycleFlush"
    );
    assert!(
        !app.world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .iter()
            .any(|o| matches!(o.action, ModHotReloadAction::Failed(_))),
        "PreUpdate mod hot-reload must not hit ScheduleLeased/Failed: {:?}",
        app.world().resource::<ModHotReloadHistory>().outcomes
    );

    let before = app.world().resource::<WatchCounter>().value;
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(app.world().resource::<WatchCounter>().value - before, 30);

    // Deleting the PreUpdate mod also unloads cleanly in ModLifecycleFlush with zero UAF.
    std::fs::remove_file(&dylib_path).expect("delete PreUpdate mod");
    let mut unloaded = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(80));
        app.update();
        if app
            .world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .iter()
            .any(|o| o.action == ModHotReloadAction::Unloaded)
        {
            unloaded = true;
            break;
        }
    }
    assert!(
        unloaded,
        "PreUpdate mod must unload cleanly in ModLifecycleFlush"
    );
    assert!(app.world().resource::<ModHost>().is_empty());

    let after_unload = app.world().resource::<WatchCounter>().value;
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(app.world().resource::<WatchCounter>().value, after_unload);
}

#[test]
fn sync_on_start_loads_mod_with_startup_system_cleanly_before_first_update() {
    let temp = TempDirGuard::new("bam_sync_on_start_startup");
    let mods_dir = temp.0.join("mods");
    let startup_dir = mods_dir.join("startup_mod");
    let build_dir = temp.0.join("build");
    std::fs::create_dir_all(&startup_dir).unwrap();
    std::fs::create_dir_all(&build_dir).unwrap();

    let dylib_path = startup_dir.join(dylib_filename("startup_mod"));
    std::fs::write(
        startup_dir.join("mod.toml"),
        "[mod]\nid = \"startup_mod\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n",
    )
    .unwrap();
    if !compile_c_startup_and_update_mod(&build_dir, &dylib_path, "startup_mod", "1.0.0", 100, 1) {
        return;
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(WatchCounter::default());
    app.register_mod_resource::<WatchCounter>().unwrap();

    app.add_plugins(
        ModHotReloadPlugin::new(&mods_dir)
            .with_debounce(Duration::from_millis(50))
            .with_sync_on_start(true),
    );

    // Loaded synchronously during Plugin::build (while Startup is not leased), so Startup system
    // is scheduled in Startup and has not run yet.
    assert_eq!(app.world().resource::<ModHost>().len(), 1);
    assert_eq!(app.world().resource::<WatchCounter>().value, 0);
    assert!(
        app.world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .iter()
            .any(|o| o.action == ModHotReloadAction::Loaded)
    );
    assert!(!app.world().resource::<StartupCompleted>().has_run());

    // First frame runs Startup (+100) and Update (+1).
    app.update();
    assert_eq!(
        app.world().resource::<WatchCounter>().value,
        101,
        "outcomes after frame 1: {:?}",
        app.world().resource::<ModHotReloadHistory>().outcomes
    );
    assert!(app.world().resource::<StartupCompleted>().has_run());

    // Second frame runs only Update (+1).
    app.update();
    assert_eq!(app.world().resource::<WatchCounter>().value, 102);

    // Hot-reload after Startup completed runs new Startup immediately (+200) and Update (+10).
    app.world_mut()
        .resource_mut::<ModHotReloadHistory>()
        .outcomes
        .clear();
    std::thread::sleep(Duration::from_millis(120));
    assert!(compile_c_startup_and_update_mod(
        &build_dir,
        &dylib_path,
        "startup_mod",
        "1.1.0",
        200,
        10,
    ));

    let mut reloaded = false;
    for _ in 0..40 {
        std::thread::sleep(Duration::from_millis(80));
        app.update();
        if app
            .world()
            .resource::<ModHotReloadHistory>()
            .outcomes
            .iter()
            .any(|o| o.action == ModHotReloadAction::Reloaded)
        {
            reloaded = true;
            break;
        }
    }
    assert!(
        reloaded,
        "startup_mod must hot-reload after StartupCompleted"
    );

    // Flush any trailing OS FSEvents metadata event from the dylib replace before sampling a pure Update tick.
    std::thread::sleep(Duration::from_millis(300));
    app.update();

    let after_reload = app.world().resource::<WatchCounter>().value;
    app.update();
    let after_next_tick = app.world().resource::<WatchCounter>().value;
    assert_eq!(
        after_next_tick - after_reload,
        10,
        "outcomes: {:?}",
        app.world().resource::<ModHotReloadHistory>().outcomes
    );
    assert!(
        after_reload >= 102 + 200,
        "reloaded late Startup system must have fired (+200), value={after_reload}"
    );
}

#[test]
fn schedule_leased_defers_dag_sync_and_retries_next_frame_without_failed() {
    let temp = TempDirGuard::new("bam_dag_schedule_leased");
    let mods_dir = temp.0.join("mods");
    let base_dir = mods_dir.join("base_mod");
    let addon_dir = mods_dir.join("addon_mod");
    let build_dir = temp.0.join("build");
    std::fs::create_dir_all(&base_dir).unwrap();
    std::fs::create_dir_all(&addon_dir).unwrap();
    std::fs::create_dir_all(&build_dir).unwrap();

    std::fs::write(
        base_dir.join("mod.toml"),
        "[mod]\nid = \"base_mod\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n",
    )
    .unwrap();
    std::fs::write(
        addon_dir.join("mod.toml"),
        "[mod]\nid = \"addon_mod\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n\n[dependencies]\nbase_mod = \"^1.0\"\n",
    )
    .unwrap();

    if !compile_c_watch_mod(
        &build_dir,
        &base_dir.join(dylib_filename("base_mod")),
        "base_mod",
        "1.0.0",
        2,
    ) {
        return;
    }
    assert!(compile_c_watch_mod(
        &build_dir,
        &addon_dir.join(dylib_filename("addon_mod")),
        "addon_mod",
        "1.0.0",
        3,
    ));

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(WatchCounter::default());
    app.register_mod_resource::<WatchCounter>().unwrap();

    let watcher = ModHotReloadWatcher::watch_with_options(
        &mods_dir,
        Duration::from_millis(50),
        true,
        LoadOptions::new().with_shadow_copy(true),
    )
    .expect("create watcher");

    // Simulate `Update` schedule currently leased while `sync_dir` runs.
    let leased_update = app
        .world_mut()
        .resource_mut::<Schedules>()
        .remove_temporarily(Update)
        .expect("Update schedule exists");

    let initial_outcomes = watcher.sync_dir(app.world_mut());
    assert!(
        initial_outcomes.is_empty(),
        "ScheduleLeased during sync_dir must defer both base_mod and addon_mod without Failed, got {initial_outcomes:?}"
    );
    assert!(app.world().resource::<ModHost>().is_empty());

    // Return `Update`, then retry on the next tick via `process_pending_reloads`.
    app.world_mut()
        .resource_mut::<Schedules>()
        .reinsert(leased_update);

    let retry_outcomes = watcher.process_pending_reloads(app.world_mut());
    assert_eq!(
        retry_outcomes
            .iter()
            .filter(|o| o.action == ModHotReloadAction::Loaded)
            .count(),
        2,
        "both deferred DAG mods must load on retry: {retry_outcomes:?}"
    );
    assert_eq!(app.world().resource::<ModHost>().len(), 2);

    app.update();
    assert_eq!(app.world().resource::<WatchCounter>().value, 5);
}
