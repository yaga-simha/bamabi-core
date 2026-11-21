# `bamabi_watcher` Architecture & Implementation Guide

`bamabi_watcher` (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1`) is the **`#![forbid(unsafe_code)]` debounced filesystem hot-reload watcher** in Layer 2 ([`bamabi-core`](../../README.md)).

---

## 1. Crate Role & Isolation Boundary

1. **Isolated `notify` Dependency:** `notify-debouncer-full` lives exclusively in `bamabi_watcher` ([`Cargo.toml`](Cargo.toml)) so [`bamabi_host`](../bamabi_host/README.md) remains lean and free of OS filesystem watcher threads.
2. **Zero `unsafe` Code:** [`src/lib.rs`](src/lib.rs) enforces `#![forbid(unsafe_code)]` and `#![warn(missing_docs)]`.
3. **Zero Non-Test Panics:** Mutex acquisitions recover gracefully on poison (`unwrap_or_else(PoisonError::into_inner)`), channel disconnects emit bounded diagnostics instead of panicking, and broken manifests or binaries fail cleanly into [`ModHotReloadHistory`](src/watcher.rs).

---

## 2. Module Map

| Module | Path | Contents |
|---|---|---|
| **Crate root** | [`src/lib.rs`](src/lib.rs) | `#![forbid(unsafe_code)]`, `#![warn(missing_docs)]`, crate re-exports, and `prelude`. |
| **Watcher & plugin** | [`src/watcher.rs`](src/watcher.rs) | `ModHotReloadPlugin`, `ModHotReloadWatcher`, `ModHotReloadHistory`, `ModHotReloadOutcome`, `ModHotReloadAction`, `FileFingerprint`, DAG sync, and retry/disconnect state machines. |
| **Unit tests** | [`tests/unit/watcher_tests.rs`](tests/unit/watcher_tests.rs) | 13 unit tests using mock `mpsc` channels, poisoned mutexes, and synthetic C99 `.cdylib` fixtures. |
| **Integration tests** | [`tests/hot_reload.rs`](tests/hot_reload.rs) | 9 end-to-end filesystem watcher & Bevy `App` schedule tests. |

---

## 3. Execution Lifecycle & Resilience Guarantees

### 3.1 `ModLifecycleFlush` Execution & Synchronous `sync_on_start`

- **`ModHotReloadPlugin::build` (`src/watcher.rs`):**
  1. Initializes `ModHotReloadHistory` and calls `ensure_mod_lifecycle_flush_schedule(app)` so `ModLifecycleFlush` is registered after `Last` in `MainScheduleOrder`.
  2. Starts `ModHotReloadWatcher::watch_with_options(...)` (forcing `shadow_copy = true` on `LoadOptions`).
  3. **Synchronous `sync_on_start`:** When `ModHotReloadPlugin::with_sync_on_start(true)` is enabled, `build` calls `watcher.sync_dir(app.world_mut())` **synchronously during plugin build**—before any frame schedules are leased—so mods with `ScheduleId::STARTUP` systems schedule into `Startup` cleanly before the first `app.update()` and initial outcomes append directly to `ModHotReloadHistory`.
  4. Inserts `ModHotReloadWatcher` as a `Resource` and schedules `poll_hot_reload_watcher_system` into `ModLifecycleFlush` (never `PreUpdate`, which previously ran while `PreUpdate` itself was leased out of `Schedules`!).

### 3.2 `ScheduleLeased` Transient Retry Semantics

Even inside `ModLifecycleFlush` (or during manual `process_pending_reloads` / `sync_dir` calls), a target schedule might still be leased externally (e.g. inside `World::try_schedule_scope`). `bamabi_watcher` classifies transient leased errors via `is_schedule_leased_load_error` and `is_schedule_leased_dag_error`:
- When `world.load_mod_with`, `world.reload_mod_from`, `world.unload_mod`, or `load_unloaded_dag_mods` returns `ModLoadError::ScheduleLeased` (or `ModDagError::LoadFailed` wrapping `ScheduleLeased`), the affected path is pushed into `PendingRetries` (`retry_changes` / `retry_deletes`) and dependent mods in the same batch (`deferred_mods` / `deferred_in_batch`) are likewise deferred.
- **No false `Failed` outcomes:** Transient `ScheduleLeased` retries are logged at `tracing::debug!` and **not** recorded as `ModHotReloadAction::Failed` in `ModHotReloadHistory`; they automatically retry on subsequent `poll_events_inner` ticks once the leased schedule returns to `Schedules`.

### 3.3 `TryRecvError::Disconnected` Handling

When the underlying `notify` debouncer thread exits or its `mpsc::Sender` drops, `poll_events_inner(record_disconnect)` observes `mpsc::TryRecvError::Disconnected`:
- Logs `"watcher channel disconnected"` **exactly once** via `tracing::error!` guarded by `disconnected_logged: AtomicBool`.
- Records **exactly one** `ModHotReloadOutcome { path: watch_dir, mod_id: None, action: ModHotReloadAction::Failed("watcher channel disconnected") }` guarded by `disconnected_outcome_recorded: AtomicBool` when `record_disconnect == true` (used by `process_pending_reloads` and `poll_hot_reload_watcher_system`), ensuring `ModHotReloadHistory` captures the failure even if an earlier `poll_events()` / `poll_changed_paths()` call noticed the disconnect first.

### 3.4 Debounce Grace Window & Echo Suppression

- **Atomic-rename grace window (`pending_deletes`):** Deletions wait `self.debounce` before firing `unload_mod`, canceling cleanly into a reload if a compiler/linker atomically recreates the `.dylib`/`.so`/`.dll` or `mod.toml` within the grace window.
- **Sync fingerprint suppression (`sync_fingerprints`):** Paths loaded or reloaded by `sync_dir` / `apply_hot_reload_events` record a 2-second `FileFingerprint` (`modified`, `len`, and Unix `ino`) so delayed OS FSEvents for the same unchanged file do not trigger duplicate reloads, while genuinely modified files invalidate the fingerprint immediately.

---

## 4. Setup & Verification

See [`TESTS.md`](TESTS.md) for all 22 unit and integration tests and coverage status (`76.38%`).
