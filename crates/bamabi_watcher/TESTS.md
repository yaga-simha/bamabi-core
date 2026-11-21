# `bamabi_watcher` Test & Verification Guide

This document inventories all unit and integration tests in `bamabi_watcher` and documents its CI coverage status.

---

## 1. Suite Overview & Commands

| Layer | Location | Count | Command |
|---|---|---|---|
| **Unit tests (`--lib`)** | [`tests/unit/watcher_tests.rs`](tests/unit/watcher_tests.rs) | `18` tests | `cargo test -p bamabi_watcher --lib --all-features` |
| **Integration tests** | [`tests/hot_reload.rs`](tests/hot_reload.rs) | `9` tests | `cargo test -p bamabi_watcher --test hot_reload --all-features` |
| **Doctests** | `src/` | `0` tests | `cargo test --doc -p bamabi_watcher --all-features` |

> **Total:** `18` unit + `9` integration = `27` tests (`0` `#[ignore]`). Tests automatically render `bamabi.h` via `bamabi_sdk_gen::render_c99_header()` and compile temporary C99 shared libraries when a C compiler (`CC`, `clang`, `gcc`, `cc`, or `cl.exe`) is available on `PATH`.

---

## 2. Unit Tests ([`tests/unit/watcher_tests.rs`](tests/unit/watcher_tests.rs), `18` tests)

1. **`apply_hot_reload_events_handles_corrupt_dylib_and_sidecar_dag_errors`** — verifies corrupt standalone `.dylib` batch dedup, external loaded mod satisfaction in `validate_dir_dag_for_reload`, sidecar `mod.toml` cycle rejection, and corrupt `.dylib` replacement failure.
2. **`apply_hot_reload_events_handles_manifest_reload_leased_failure_and_delete`** — verifies `mod.toml` reload deferral under `ScheduleLeased`, live `mod.toml` reload with batch dedup, syntax/corrupt binary error handling, and `mod.toml` deletion unloading.
3. **`debounce_event_result_err_does_not_panic_and_continues_draining`** — verifies `notify` `Err(Vec<Error>)` batches warn and continue processing valid events.
4. **`disconnected_channel_after_poll_events_still_records_outcome_once_in_flush_schedule`** — verifies `ModHotReloadHistory` still receives the single `"watcher channel disconnected"` failure in `ModLifecycleFlush` even when `poll_events()` drained the disconnect first.
5. **`disconnected_channel_logs_once_and_records_failed_outcome_once`** — verifies `TryRecvError::Disconnected` logs `tracing::error!` once and records `ModHotReloadAction::Failed` once across repeated polls.
6. **`is_dylib_path_recognizes_native_extensions_case_insensitively`** — checks `.so`, `.dylib`, `.dll`, and uppercase variants while rejecting unrelated files.
7. **`is_manifest_path_matches_exact_mod_toml_filename`** — verifies exact `mod.toml` matching.
8. **`is_relevant_event_kind_filters_creates_and_modifies`** — verifies create/modify event filtering vs. non-mutating access events.
9. **`is_watchable_mod_path_accepts_dylibs_and_manifests_only`** — restricts watched paths to native shared libraries and `mod.toml`.
10. **`pending_deletes_grace_window_cancels_unload_on_recreate_and_fires_after_expiry`** — validates atomic-rename grace-window cancellation vs. permanent delete expiry.
11. **`poisoned_mutexes_recover_without_panicking`** — recovers cleanly when `rx`, `pending_deletes`, `pending_retries`, or `sync_fingerprints` mutexes are poisoned.
12. **`poll_events_filters_and_system_edge_paths`** — verifies irrelevant event filtering, `with_unload_on_delete(false)`, `requeue_pending` validation/dedup, and `poll_hot_reload_watcher_system` missing-resource, lazy-history, and `ScheduleLeased` paths.
13. **`public_builders_and_plugin_error_paths`** — verifies `ModHotReloadWatcher::watch`, `watch_with_options` error propagation, `poll_changed_paths`, `ModHotReloadPlugin::with_load_options`, and `Plugin::build` failure logging.
14. **`same_path_compares_syntactic_and_canonical_paths`** — verifies path equality across canonicalizable and synthetic paths.
15. **`schedule_leased_defers_reload_and_unload_until_schedule_returns`** — proves `ScheduleLeased` errors on reload and unload defer into `pending_retries` without recording `Failed` and succeed once the leased schedule returns.
16. **`schedule_leased_predicates_match_direct_and_dag_wrapped_errors`** — validates `is_schedule_leased_load_error` and `is_schedule_leased_dag_error`.
17. **`sync_dir_and_validate_dir_dag_cover_error_and_cascade_paths`** — tests missing `watch_dir`, broken `mod.toml` parse errors, unsatisfied dependencies, missing libraries, cascade dependency skips, corrupt binaries, and cycles in `load_unloaded_dag_mods` / `validate_dir_dag_for_reload`.
18. **`sync_dir_fingerprint_suppresses_stale_event_but_allows_modified_file`** — validates `sync_fingerprints` echo suppression vs. real content edits.

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

## 4. CI Coverage Status (`92.27%`)

In `.github/workflows/ci.yml`, `crates/bamabi_watcher/` is included in the workspace `cargo llvm-cov` gate (`--ignore-filename-regex '(tests|examples|xtask|benches)/|\.cargo/' --fail-under-lines 85`) with zero crate exclusions:
- `crates/bamabi_watcher/src/watcher.rs` measures **`92.27%`** line coverage (`871 / 944` lines), well above the `85%` threshold.
- Combined workspace line coverage across `bamabi_host_sys`, `bamabi_host`, and `bamabi_watcher` is **`93.34%`** (`20,376 / 21,829` lines).
