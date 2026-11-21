# `bamabi_watcher`

Safe (`#![forbid(unsafe_code)]`) debounced filesystem hot-reload watcher for `bamabi` (`ABI 0.3.0`, `v0.3.0`, Layer 2 in [`bamabi-core`](../../README.md)).

## Documentation Map

- **Crate Architecture & Implementation:** [`ARCHITECTURE.md`](ARCHITECTURE.md) — module map, `ModLifecycleFlush` scheduling, synchronous `sync_on_start`, `ScheduleLeased` transient retry logic, `TryRecvError::Disconnected` handling, and atomic-rename / echo suppression.
- **Test & Verification Guide:** [`TESTS.md`](TESTS.md) — complete inventory of `18` unit tests (`watcher_tests.rs`), `9` integration tests (`hot_reload.rs`), and CI coverage status (`92.27%`, enforced in the `>= 85%` gate).
- **Workspace Guides:** [`../../docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) · [`../../TESTS.md`](../../TESTS.md) · [`GEMINI.md`](GEMINI.md)

## Overview

- **`ModHotReloadPlugin` / `ModHotReloadWatcher`:** Recursively monitors a mods directory for native shared libraries (`.so`, `.dylib`, `.dll`) and `mod.toml` manifests via `notify-debouncer-full` (pairing seamlessly with `cargo bam watch --out-dir`), optionally performs an initial synchronous `sync_dir` load during `Plugin::build` (`with_sync_on_start(true)`), validates SemVer dependency DAG constraints, and executes cascading reverse/forward topological hot-reloads and unloads in Bevy [`ModLifecycleFlush`](../bamabi_host/src/plugin.rs) (`ensure_mod_lifecycle_flush_schedule`).
- **Leased-Schedule Retry & Channel Disconnect Handling:** Automatically defers transient [`ModLoadError::ScheduleLeased`](../bamabi_host_sys/src/loader/error.rs) errors into `pending_retries` across frames without emitting spurious failures, and surfaces watcher channel disconnects (`TryRecvError::Disconnected`) once via `tracing::error!` and a single [`ModHotReloadAction::Failed`](src/watcher.rs) outcome.
- **`ModHotReloadHistory` / `ModHotReloadOutcome` / `ModHotReloadAction`:** Records chronological load, reload, unload, and validation failure outcomes as a Bevy resource.

## Standalone Usage

```toml
[dependencies]
bamabi_watcher = { git = "https://github.com/yaga-simha/bamabi-core.git", branch = "main" }
```

## Verification

```bash
cargo test -p bamabi_watcher --all-features
cargo clippy -p bamabi_watcher --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p bamabi_watcher --no-deps --all-features
```

## License

Dual-licensed under `MIT OR Apache-2.0` ([`LICENSE-MIT`](../../LICENSE-MIT) and [`LICENSE-APACHE`](../../LICENSE-APACHE)).
