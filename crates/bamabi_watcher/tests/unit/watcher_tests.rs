use super::*;
use bamabi_host::app_ext::ModAppExt;
use bamabi_host::plugin::AbiModPlugin;
use bamabi_macros::ModComponent;
use bevy_app::{App, Update};
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::Schedules;
use notify_debouncer_full::DebouncedEvent;
use notify_debouncer_full::notify::event::{
    AccessKind, CreateKind, DataChange, MetadataKind, ModifyKind, RemoveKind, RenameMode,
};
use notify_debouncer_full::notify::{Error as NotifyError, Event, EventKind};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

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

#[derive(Default)]
struct LogMatchCounter {
    needle: &'static str,
    hits: Arc<AtomicUsize>,
}

struct MessageVisitor<'a> {
    needle: &'static str,
    matched: &'a mut bool,
}

impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" && format!("{value:?}").contains(self.needle) {
            *self.matched = true;
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" && value.contains(self.needle) {
            *self.matched = true;
        }
    }
}

impl tracing::Subscriber for LogMatchCounter {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut matched = false;
        let mut visitor = MessageVisitor {
            needle: self.needle,
            matched: &mut matched,
        };
        event.record(&mut visitor);
        if matched {
            self.hits.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn enter(&self, _span: &tracing::span::Id) {}

    fn exit(&self, _span: &tracing::span::Id) {}
}

#[repr(C)]
#[derive(Resource, ModComponent, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[mod_component(name = "watcher_unit::UnitCounter")]
struct UnitCounter {
    value: u32,
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

fn compile_c_unit_mod(
    src_dir: &Path,
    out_dylib: &Path,
    mod_name: &str,
    mod_version: &str,
    delta: u32,
) -> bool {
    let Some(cc) = find_c_compiler() else {
        eprintln!("skipping watcher unit test requiring C compiler: no working compiler on PATH");
        return false;
    };

    std::fs::write(
        src_dir.join("bamabi.h"),
        bamabi_sdk_gen::render_c99_header().expect("render bamabi.h"),
    )
    .expect("write bamabi.h");
    let c_src = src_dir.join(format!("{mod_name}.c"));
    let code = format!(
        r#"#include "bamabi.h"
#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef struct UnitCounter {{
    uint32_t value;
}} UnitCounter;

typedef struct ModState {{
    BamHostApi api;
    BamComponent counter_res;
}} ModState;

static ModState g_state;

static BamResult tick_cb(BamSystemCtx* sys_ctx, void* ud) {{
    (void)ud;
    UnitCounter* c = (UnitCounter*)g_state.api.sys_res_mut_ptr(sys_ctx, g_state.counter_res, NULL);
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
    BamResult r = api->resolve_component(hctx, "watcher_unit::UnitCounter", NULL, &g_state.counter_res);
    if (r != BAM_RESULT_OK) return r;
    BamTermDesc term = bam_term_desc(g_state.counter_res, BAM_ACCESS_RESOURCE_WRITE, 0u);
    BamQueryDesc q;
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
    sys.schedule = BAM_SCHEDULE_UPDATE;
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
    std::fs::write(&c_src, code).expect("write C fixture source");

    let staging = src_dir.join(format!(
        "{mod_name}_tmp_out.{}",
        std::env::consts::DLL_EXTENSION
    ));
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
        let obj_path = src_dir.join(format!("{mod_name}_tmp.obj"));
        let implib_path = src_dir.join(format!("{mod_name}_tmp.lib"));
        cmd.args(["/nologo", "/LD", "/std:c11", "/W4", "/WX", "/O2"])
            .arg(format!("/I{}", src_dir.display()))
            .arg(format!("/Fo{}", obj_path.display()))
            .arg(&c_src)
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
        cmd.arg(&c_src).arg("-o").arg(&staging);
    }
    let out = cmd.output().expect("run C compiler");
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

fn dylib_filename(stem: &str) -> String {
    let prefix = std::env::consts::DLL_PREFIX;
    let ext = std::env::consts::DLL_EXTENSION;
    format!("{prefix}{stem}.{ext}")
}

#[test]
fn is_dylib_path_recognizes_native_extensions_case_insensitively() {
    assert!(is_dylib_path(Path::new("mods/libfoo.so")));
    assert!(is_dylib_path(Path::new("mods/libfoo.dylib")));
    assert!(is_dylib_path(Path::new("mods/foo.dll")));
    assert!(is_dylib_path(Path::new("mods/libfoo.SO")));
    assert!(is_dylib_path(Path::new("mods/libfoo.Dylib")));
    assert!(is_dylib_path(Path::new("mods/foo.DLL")));

    assert!(!is_dylib_path(Path::new("mods/mod.toml")));
    assert!(!is_dylib_path(Path::new("mods/libfoo.dylib.sig")));
    assert!(!is_dylib_path(Path::new("mods/libfoo.rs")));
    assert!(!is_dylib_path(Path::new("mods/libfoo")));
    assert!(!is_dylib_path(Path::new("")));
}

#[test]
fn is_manifest_path_matches_exact_mod_toml_filename() {
    assert!(is_manifest_path(Path::new("mod.toml")));
    assert!(is_manifest_path(Path::new("mods/base_mod/mod.toml")));

    assert!(!is_manifest_path(Path::new("mods/base_mod/Cargo.toml")));
    assert!(!is_manifest_path(Path::new("mods/base_mod/MOD.TOML")));
    assert!(!is_manifest_path(Path::new("mods/base_mod/mod.toml.bak")));
    assert!(!is_manifest_path(Path::new("mods/base_mod")));
    assert!(!is_manifest_path(Path::new("")));
}

#[test]
fn is_watchable_mod_path_accepts_dylibs_and_manifests_only() {
    assert!(is_watchable_mod_path(Path::new("mods/a/liba.so")));
    assert!(is_watchable_mod_path(Path::new("mods/a/liba.dylib")));
    assert!(is_watchable_mod_path(Path::new("mods/a/a.dll")));
    assert!(is_watchable_mod_path(Path::new("mods/a/mod.toml")));

    assert!(!is_watchable_mod_path(Path::new("mods/a/liba.dylib.sig")));
    assert!(!is_watchable_mod_path(Path::new("mods/a/Cargo.toml")));
    assert!(!is_watchable_mod_path(Path::new("mods/a/main.c")));
    assert!(!is_watchable_mod_path(Path::new("mods/a/README.md")));
    assert!(!is_watchable_mod_path(Path::new("")));
}

#[test]
fn is_relevant_event_kind_filters_creates_and_modifies() {
    assert!(is_relevant_event_kind(EventKind::Create(CreateKind::File)));
    assert!(is_relevant_event_kind(EventKind::Create(CreateKind::Any)));
    assert!(is_relevant_event_kind(EventKind::Modify(ModifyKind::Data(
        DataChange::Content
    ))));
    assert!(is_relevant_event_kind(EventKind::Modify(
        ModifyKind::Metadata(MetadataKind::WriteTime)
    )));
    assert!(is_relevant_event_kind(EventKind::Modify(ModifyKind::Name(
        RenameMode::To
    ))));
    assert!(is_relevant_event_kind(EventKind::Modify(ModifyKind::Any)));

    assert!(!is_relevant_event_kind(EventKind::Create(
        CreateKind::Folder
    )));
    assert!(!is_relevant_event_kind(EventKind::Remove(RemoveKind::File)));
    assert!(!is_relevant_event_kind(EventKind::Remove(RemoveKind::Any)));
    assert!(!is_relevant_event_kind(EventKind::Access(AccessKind::Read)));
    assert!(!is_relevant_event_kind(EventKind::Any));
    assert!(!is_relevant_event_kind(EventKind::Other));
}

#[test]
fn same_path_compares_syntactic_and_canonical_paths() {
    let temp = TempDirGuard::new("bam_same_path");
    let sub = temp.0.join("nested");
    std::fs::create_dir_all(&sub).expect("create nested dir");
    let file = temp.0.join("libmod.dylib");
    std::fs::write(&file, b"stub").expect("write dummy file");

    let dotdot = sub.join("..").join("libmod.dylib");
    assert!(same_path(&file, &file));
    assert!(same_path(&file, &dotdot));

    let other = temp.0.join("other.dylib");
    std::fs::write(&other, b"stub2").expect("write second dummy file");
    assert!(!same_path(&file, &other));

    let missing_a = temp.0.join("missing_a.dylib");
    let missing_b = temp.0.join("missing_b.dylib");
    assert!(same_path(&missing_a, &missing_a));
    assert!(!same_path(&missing_a, &missing_b));
    assert!(!same_path(&file, &missing_a));
}

#[test]
fn schedule_leased_predicates_match_direct_and_dag_wrapped_errors() {
    let leased = ModLoadError::ScheduleLeased {
        id: ModId(1),
        schedule: "Update".to_string(),
    };
    assert!(is_schedule_leased_load_error(&leased));

    let not_found = ModLoadError::ModNotFound { id: ModId(1) };
    assert!(!is_schedule_leased_load_error(&not_found));

    let dag_leased = ModDagError::LoadFailed {
        mod_name: "demo".to_string(),
        path: PathBuf::from("mods/demo/libdemo.dylib"),
        source: Box::new(ModLoadError::ScheduleLeased {
            id: ModId(2),
            schedule: "PreUpdate".to_string(),
        }),
    };
    assert!(is_schedule_leased_dag_error(&dag_leased));

    let dag_other = ModDagError::LoadFailed {
        mod_name: "demo".to_string(),
        path: PathBuf::from("mods/demo/libdemo.dylib"),
        source: Box::new(ModLoadError::ModNotFound { id: ModId(2) }),
    };
    assert!(!is_schedule_leased_dag_error(&dag_other));
    assert!(!is_schedule_leased_dag_error(&ModDagError::ModNotFound {
        id: ModId(2)
    }));
}

#[test]
fn disconnected_channel_logs_once_and_records_failed_outcome_once() {
    let temp = TempDirGuard::new("bam_watcher_disconnect");
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    drop(tx);

    let watcher = ModHotReloadWatcher::from_receiver_for_test(
        &temp.0,
        Duration::from_millis(50),
        rx,
        LoadOptions::default(),
    );
    let mut world = World::new();

    let hits = Arc::new(AtomicUsize::new(0));
    let subscriber = LogMatchCounter {
        needle: "watcher channel disconnected",
        hits: Arc::clone(&hits),
    };

    tracing::subscriber::with_default(subscriber, || {
        let first = watcher.process_pending_reloads(&mut world);
        assert_eq!(
            first,
            vec![ModHotReloadOutcome {
                path: temp.0.clone(),
                mod_id: None,
                action: ModHotReloadAction::Failed("watcher channel disconnected".to_string()),
            }]
        );

        let second = watcher.process_pending_reloads(&mut world);
        assert!(second.is_empty(), "disconnect outcome must emit only once");

        let (changed, deleted) = watcher.poll_events();
        assert!(changed.is_empty());
        assert!(deleted.is_empty());
    });

    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "disconnect error must log exactly once"
    );
}

#[test]
fn disconnected_channel_after_poll_events_still_records_outcome_once_in_flush_schedule() {
    let temp = TempDirGuard::new("bam_watcher_disconnect_flush");
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    drop(tx);

    let watcher = ModHotReloadWatcher::from_receiver_for_test(
        &temp.0,
        Duration::from_millis(50),
        rx,
        LoadOptions::default(),
    );

    let hits = Arc::new(AtomicUsize::new(0));
    let subscriber = LogMatchCounter {
        needle: "watcher channel disconnected",
        hits: Arc::clone(&hits),
    };

    tracing::subscriber::with_default(subscriber, || {
        // Polling events directly logs once, without suppressing the single history outcome.
        assert_eq!(watcher.poll_events(), (Vec::new(), Vec::new()));
        assert_eq!(watcher.poll_events(), (Vec::new(), Vec::new()));

        let mut app = App::new();
        ensure_mod_lifecycle_flush_schedule(&mut app);
        app.init_resource::<ModHotReloadHistory>();
        app.insert_resource(watcher);
        app.add_systems(ModLifecycleFlush, poll_hot_reload_watcher_system);

        for _ in 0..3 {
            app.update();
        }

        let history = app.world().resource::<ModHotReloadHistory>();
        assert_eq!(
            history.outcomes,
            vec![ModHotReloadOutcome {
                path: temp.0.clone(),
                mod_id: None,
                action: ModHotReloadAction::Failed("watcher channel disconnected".to_string()),
            }]
        );
    });

    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn debounce_event_result_err_does_not_panic_and_continues_draining() {
    let temp = TempDirGuard::new("bam_watcher_err_batch");
    let dylib = temp.0.join(dylib_filename("dummy"));
    std::fs::write(&dylib, b"dummy").expect("write dummy dylib");

    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    tx.send(Err(vec![NotifyError::generic("simulated watcher error")]))
        .expect("send error batch");
    tx.send(Ok(vec![DebouncedEvent::new(
        Event::new(EventKind::Create(CreateKind::File)).add_path(dylib.clone()),
        Instant::now(),
    )]))
    .expect("send follow-up create batch");

    let watcher = ModHotReloadWatcher::from_receiver_for_test(
        &temp.0,
        Duration::from_millis(50),
        rx,
        LoadOptions::default(),
    );

    let (changed, deleted) = watcher.poll_events();
    assert_eq!(changed, vec![dylib]);
    assert!(deleted.is_empty());

    tx.send(Err(vec![NotifyError::generic("second simulated error")]))
        .expect("send second error batch");
    let mut world = World::new();
    let outcomes = watcher.process_pending_reloads(&mut world);
    assert!(outcomes.is_empty());
}

#[test]
fn pending_deletes_grace_window_cancels_unload_on_recreate_and_fires_after_expiry() {
    let temp = TempDirGuard::new("bam_pending_deletes_unit");
    let dylib = temp.0.join(dylib_filename("grace_mod"));
    std::fs::write(&dylib, b"v1").expect("write v1");

    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let watcher = ModHotReloadWatcher::from_receiver_for_test(
        &temp.0,
        Duration::from_millis(80),
        rx,
        LoadOptions::default(),
    );

    // Delete file and deliver Remove event -> held in `pending_deletes`.
    std::fs::remove_file(&dylib).expect("remove v1");
    tx.send(Ok(vec![DebouncedEvent::new(
        Event::new(EventKind::Remove(RemoveKind::File)).add_path(dylib.clone()),
        Instant::now(),
    )]))
    .expect("send remove");
    assert_eq!(watcher.poll_events(), (Vec::new(), Vec::new()));

    // Recreate within grace window -> next poll promotes to changed without unload.
    std::fs::write(&dylib, b"v2").expect("recreate v2");
    assert_eq!(watcher.poll_events(), (vec![dylib.clone()], Vec::new()));

    // Delete again and wait past debounce -> emits deleted.
    std::fs::remove_file(&dylib).expect("remove v2");
    tx.send(Ok(vec![DebouncedEvent::new(
        Event::new(EventKind::Remove(RemoveKind::File)).add_path(dylib.clone()),
        Instant::now(),
    )]))
    .expect("send second remove");
    assert_eq!(watcher.poll_events(), (Vec::new(), Vec::new()));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(watcher.poll_events(), (Vec::new(), vec![dylib]));
}

#[test]
fn poisoned_mutexes_recover_without_panicking() {
    let temp = TempDirGuard::new("bam_poison_recovery");
    let (_tx, rx) = mpsc::channel::<DebounceEventResult>();
    let watcher = Arc::new(ModHotReloadWatcher::from_receiver_for_test(
        &temp.0,
        Duration::from_millis(50),
        rx,
        LoadOptions::default(),
    ));

    let w1 = Arc::clone(&watcher);
    let _ = std::thread::spawn(move || {
        let _guard = w1.rx.lock().unwrap();
        panic!("poison rx mutex");
    })
    .join();

    let w2 = Arc::clone(&watcher);
    let _ = std::thread::spawn(move || {
        let _guard = w2.pending_deletes.lock().unwrap();
        panic!("poison pending_deletes mutex");
    })
    .join();

    let w3 = Arc::clone(&watcher);
    let _ = std::thread::spawn(move || {
        let _guard = w3.pending_retries.lock().unwrap();
        panic!("poison pending_retries mutex");
    })
    .join();

    let w4 = Arc::clone(&watcher);
    let _ = std::thread::spawn(move || {
        let _guard = w4.sync_fingerprints.lock().unwrap();
        panic!("poison sync_fingerprints mutex");
    })
    .join();

    let (changed, deleted) = watcher.poll_events();
    assert!(changed.is_empty());
    assert!(deleted.is_empty());
}

#[test]
fn schedule_leased_defers_reload_and_unload_until_schedule_returns() {
    let temp = TempDirGuard::new("bam_unit_schedule_leased");
    let mods_dir = temp.0.join("mods");
    let build_dir = temp.0.join("build");
    std::fs::create_dir_all(&mods_dir).expect("create mods_dir");
    std::fs::create_dir_all(&build_dir).expect("create build_dir");

    let dylib_path = mods_dir.join(dylib_filename("leased_unit"));
    if !compile_c_unit_mod(&build_dir, &dylib_path, "leased_unit", "0.1.0", 1) {
        return;
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(UnitCounter::default());
    app.register_mod_resource::<UnitCounter>()
        .expect("register UnitCounter");

    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let watcher = ModHotReloadWatcher::from_receiver_for_test(
        &mods_dir,
        Duration::from_millis(20),
        rx,
        LoadOptions::new().with_shadow_copy(true),
    );

    // Initial standalone load while Update is leased -> deferred, zero Failed outcomes.
    let leased_update = app
        .world_mut()
        .resource_mut::<Schedules>()
        .remove_temporarily(Update)
        .expect("Update schedule exists");

    tx.send(Ok(vec![DebouncedEvent::new(
        Event::new(EventKind::Create(CreateKind::File)).add_path(dylib_path.clone()),
        Instant::now(),
    )]))
    .expect("send create");

    let outcomes = watcher.process_pending_reloads(app.world_mut());
    assert!(
        outcomes.is_empty(),
        "initial load while Update leased must defer without Failed, got {outcomes:?}"
    );
    assert!(app.world().resource::<ModHost>().is_empty());

    // Return Update -> next poll loads without needing a new filesystem event.
    app.world_mut()
        .resource_mut::<Schedules>()
        .reinsert(leased_update);
    let outcomes = watcher.process_pending_reloads(app.world_mut());
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].action, ModHotReloadAction::Loaded);

    app.update();
    assert_eq!(app.world().resource::<UnitCounter>().value, 1);

    // Recompile v2 and send Modify while Update is leased -> deferred reload, v1 remains active.
    assert!(compile_c_unit_mod(
        &build_dir,
        &dylib_path,
        "leased_unit",
        "0.2.0",
        10
    ));
    let leased_update = app
        .world_mut()
        .resource_mut::<Schedules>()
        .remove_temporarily(Update)
        .expect("Update schedule exists");
    tx.send(Ok(vec![DebouncedEvent::new(
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
            .add_path(dylib_path.clone()),
        Instant::now(),
    )]))
    .expect("send modify");

    let outcomes = watcher.process_pending_reloads(app.world_mut());
    assert!(
        outcomes.is_empty(),
        "reload while Update leased must defer without Failed, got {outcomes:?}"
    );
    app.world_mut()
        .resource_mut::<Schedules>()
        .reinsert(leased_update);

    let outcomes = watcher.process_pending_reloads(app.world_mut());
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].action, ModHotReloadAction::Reloaded);

    let before = app.world().resource::<UnitCounter>().value;
    app.update();
    assert_eq!(app.world().resource::<UnitCounter>().value - before, 10);

    // Remove dylib, expire debounce, then poll while Update is leased -> deferred unload.
    std::fs::remove_file(&dylib_path).expect("delete dylib");
    tx.send(Ok(vec![DebouncedEvent::new(
        Event::new(EventKind::Remove(RemoveKind::File)).add_path(dylib_path),
        Instant::now(),
    )]))
    .expect("send remove");
    assert!(watcher.process_pending_reloads(app.world_mut()).is_empty());
    std::thread::sleep(Duration::from_millis(40));

    let leased_update = app
        .world_mut()
        .resource_mut::<Schedules>()
        .remove_temporarily(Update)
        .expect("Update schedule exists");
    let outcomes = watcher.process_pending_reloads(app.world_mut());
    assert!(
        outcomes.is_empty(),
        "unload while Update leased must defer without Failed, got {outcomes:?}"
    );
    assert_eq!(app.world().resource::<ModHost>().len(), 1);

    app.world_mut()
        .resource_mut::<Schedules>()
        .reinsert(leased_update);
    let outcomes = watcher.process_pending_reloads(app.world_mut());
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].action, ModHotReloadAction::Unloaded);
    assert!(app.world().resource::<ModHost>().is_empty());
}

#[test]
fn sync_dir_fingerprint_suppresses_stale_event_but_allows_modified_file() {
    let temp = TempDirGuard::new("bam_sync_fp");
    let mods_dir = temp.0.join("mods");
    let sub_dir = mods_dir.join("fp_mod");
    let build_dir = temp.0.join("build");
    std::fs::create_dir_all(&sub_dir).expect("create sub_dir");
    std::fs::create_dir_all(&build_dir).expect("create build_dir");

    let dylib_path = sub_dir.join(dylib_filename("fp_mod"));
    std::fs::write(
        sub_dir.join("mod.toml"),
        "[mod]\nid = \"fp_mod\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n",
    )
    .expect("write mod.toml");
    if !compile_c_unit_mod(&build_dir, &dylib_path, "fp_mod", "1.0.0", 1) {
        return;
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.insert_resource(UnitCounter::default());
    app.register_mod_resource::<UnitCounter>()
        .expect("register UnitCounter");

    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    let watcher = ModHotReloadWatcher::from_receiver_for_test(
        &mods_dir,
        Duration::from_millis(50),
        rx,
        LoadOptions::new().with_shadow_copy(true),
    );

    let loaded = watcher.sync_dir(app.world_mut());
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].action, ModHotReloadAction::Loaded);

    // Stale event with unchanged file metadata must be suppressed.
    tx.send(Ok(vec![DebouncedEvent::new(
        Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Any)))
            .add_path(dylib_path.clone()),
        Instant::now(),
    )]))
    .expect("send stale modify");
    assert_eq!(watcher.poll_events(), (Vec::new(), Vec::new()));

    // Genuine modification updates fingerprint -> next event passes through.
    std::thread::sleep(Duration::from_millis(120));
    assert!(compile_c_unit_mod(
        &build_dir,
        &dylib_path,
        "fp_mod",
        "1.0.1",
        5
    ));
    tx.send(Ok(vec![DebouncedEvent::new(
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
            .add_path(dylib_path.clone()),
        Instant::now(),
    )]))
    .expect("send real modify");
    assert_eq!(watcher.poll_events(), (vec![dylib_path], Vec::new()));
}
