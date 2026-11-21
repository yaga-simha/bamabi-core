# `bamabi_watcher` Test & Verification Guide

This document inventories all unit and integration tests in `bamabi_watcher` and documents its CI coverage status.

---

## 1. Suite Overview & Commands

| Layer | Location | Count | Command |
|---|---|---|---|
| **Unit tests (`--lib`)** | [`tests/unit/watcher_tests.rs`](tests/unit/watcher_tests.rs) | `13` tests | `cargo test -p bamabi_watcher --lib --all-features` |
| **Integration tests** | [`tests/hot_reload.rs`](tests/hot_reload.rs) | `9` tests | `cargo test -p bamabi_watcher --test hot_reload --all-features` |
| **Doctests** | `src/` | `0` tests | `cargo test --doc -p bamabi_watcher --all-features` |

> **Total:** `13` unit + `9` integration = `22` tests (`0` `#[ignore]`). Tests automatically render `bamabi.h` via `bamabi_sdk_gen::render_c99_header()` and compile temporary C99 shared libraries when a C compiler (`CC`, `clang`, `gcc`, `cc`, or `cl.exe`) is available on `PATH`.

---

## 2. Unit Tests ([`tests/unit/watcher_tests.rs`](tests/unit/watcher_tests.rs), `13` tests)

1. **`debounce_event_result_err_does_not_panic_and_continues_draining`** — verifies `notify` `Err(Vec<Error>)` batches warn and continue processing valid events.
2. **`disconnected_channel_after_poll_events_still_records_outcome_once_in_flush_schedule`** — verifies `ModHotReloadHistory` still receives the single `"watcher channel disconnected"` failure in `ModLifecycleFlush` even when `poll_events()` drained the disconnect first.
3. **`disconnected_channel_logs_once_and_records_failed_outcome_once`** — verifies `TryRecvError::Disconnected` logs `tracing::error!` once and records `ModHotReloadAction::Failed` once across repeated polls.
4. **`is_dylib_path_recognizes_native_extensions_case_insensitively`** — checks `.so`, `.dylib`, `.dll`, and uppercase variants while rejecting unrelated files.
5. **`is_manifest_path_matches_exact_mod_toml_filename`** — verifies exact `mod.toml` matching.
6. **`is_relevant_event_kind_filters_creates_and_modifies`** — verifies create/modify event filtering vs. non-mutating access events.
7. **`is_watchable_mod_path_accepts_dylibs_and_manifests_only`** — restricts watched paths to native shared libraries and `mod.toml`.
8. **`pending_deletes_grace_window_cancels_unload_on_recreate_and_fires_after_expiry`** — validates atomic-rename grace-window cancellation vs. permanent delete expiry.
9. **`poisoned_mutexes_recover_without_panicking`** — recovers cleanly when `rx`, `pending_deletes`, `pending_retries`, or `sync_fingerprints` mutexes are poisoned.
10. **`same_path_compares_syntactic_and_canonical_paths`** — verifies path equality across canonicalizable and synthetic paths.
11. **`schedule_leased_defers_reload_and_unload_until_schedule_returns`** — proves `ScheduleLeased` errors on reload and unload defer into `pending_retries` without recording `Failed` and succeed once the leased schedule returns.
12. **`schedule_leased_predicates_match_direct_and_dag_wrapped_errors`** — validates `is_schedule_leased_load_error` and `is_schedule_leased_dag_error`.
13. **`sync_dir_fingerprint_suppresses_stale_event_but_allows_modified_file`** — validates `sync_fingerprints` echo suppression vs. real content edits.

---

## 3. Integration Tests ([`tests/hot_reload.rs`](tests/hot_reload.rs), `9` tests)

1. **`broken_mod_toml_edit_produces_failed_and_keeps_old_mod`** — invalidates `mod.toml` syntax mid-run, asserts `ModHotReloadAction::Failed`, and verifies the prior mod remains active.
2. **`delete_then_recreate_within_debounce_window_does_not_unload`** — exercises atomic linker replacement inside the debounce window without unloading.
3. **`non_recursive_mode_ignores_nested_dirs`** — validates `with_recursive(false)` ignores nested subdirectories.
4. **`schedule_leased_defers_dag_sync_and_retries_next_frame_without_failed`** — tests `sync_dir` deferral on `ScheduleLeased` and clean completion on the next frame.
5. **`sync_on_start_loads_mod_with_startup_system_cleanly_before_first_update`** — verifies `with_sync_on_start(true)` synchronously loads a `Startup`-targeted mod in `Plugin::build` and executes its `Startup` system on the first frame.
6. **`watcher_cascades_dag_reload_on_base_mod_rebuild_and_rejects_broken_semver_edit`** — verifies cascading reverse-unload/forward-reload (`dep -> base`, then `base -> dep`) and SemVer constraint failure rejection.
7. **`watcher_live_reloads_modified_shared_library`** — end-to-end shared library rebuild detection and live reload.
8. **`watcher_runs_in_mod_lifecycle_flush_and_reloads_pre_update_mod`** — verifies `ModHotReloadPlugin` runs in `ModLifecycleFlush` and cleanly reloads a mod targeting `PreUpdate` (which would previously fail if polled in `PreUpdate`).
9. **`with_unload_on_delete_false_keeps_mod_loaded`** — verifies `with_unload_on_delete(false)` preserves loaded mods on disk deletion.

---

## 4. CI Coverage Status (`76.38%`)

In `.github/workflows/ci.yml`, `crates/bamabi_watcher/` is the **only** crate excluded by the coverage gate (`--ignore-filename-regex '(crates/bamabi_watcher/|(tests|examples|xtask|benches)/|\.cargo/)' --fail-under-lines 85`):
- `crates/bamabi_watcher/src/watcher.rs` measures **`76.38%`** line coverage (`721 / 944` lines), primarily due to OS `notify` / filesystem watcher event-loop branches and platform-specific `inotify`/`FSEvents`/`ReadDirectoryChanges` edge cases.
- Even without excluding `crates/bamabi_watcher/`, combined workspace line coverage is **`92.65%`** (`20,094 / 21,689` lines), well above the `85%` threshold.
