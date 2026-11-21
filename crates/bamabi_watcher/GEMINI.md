# Agent Guide: `bamabi_watcher`

Crate guide for **`bamabi_watcher`** (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1+`) — debounced filesystem hot-reload watcher for native Bevy mods.

Documentation index: [`README.md`](README.md) · [`ARCHITECTURE.md`](ARCHITECTURE.md) · [`TESTS.md`](TESTS.md).

---

## 1. Crate Identity
- **Name:** `bamabi_watcher` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Monitors mod directories recursively via `notify-debouncer-full`, validates `mod.toml` SemVer DAG constraints, optionally loads initial mods synchronously in `Plugin::build` (`with_sync_on_start`), and drives cascading reverse/forward topological reloads and unloads in Bevy `ModLifecycleFlush`.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root. Zero `unsafe` code.
- **Allowed Dependencies:** `bamabi_host_sys`, `bamabi_host`, `bevy_app`, `bevy_ecs`, `notify-debouncer-full`, `semver`, `tracing` (plus `bamabi_{macros,sys,sdk_gen}` in `[dev-dependencies]`).
- **Keep `notify` Isolated Here:** Filesystem watching (`notify-debouncer-full`) belongs strictly in this crate so `bamabi_host` stays lean and headless.

---

## 3. Owned Surface (`src/lib.rs`, `src/watcher.rs`)
- **`ModHotReloadPlugin` & `ModHotReloadWatcher`:** Configures watched directories, debounce timing, `with_sync_on_start` synchronous initial `sync_dir` execution in `Plugin::build`, and automatic cascading unload/reload ordering across dependent mods in `ModLifecycleFlush`.
- **Transient Retry & Disconnect Resilience:** Retries `ModLoadError::ScheduleLeased` (and `ModDagError::LoadFailed` wrapping `ScheduleLeased`) automatically across polls without emitting `ModHotReloadAction::Failed`, suppresses echo events via `sync_fingerprints`, and handles `mpsc::TryRecvError::Disconnected` idempotently (one `tracing::error!` + one `ModHotReloadOutcome`).
- **Outcome History:** `ModHotReloadHistory`, `ModHotReloadOutcome`, and `ModHotReloadAction` track chronological load, reload, unload, and validation failure records as a Bevy resource.

---

## 4. Strict Engineering Rules
1. **No Watcher State Corruption:** Never leave half-reloaded dependency subgraphs; rely on `bamabi_host`'s DAG loader and transactional rollback on init failure.
2. **Safe-Point Bevy Loop (`ModLifecycleFlush`):** Drain debounced filesystem events cleanly in `ModLifecycleFlush` without blocking Bevy frame execution on file-watcher I/O and without mutating leased schedules.
3. **Zero Non-Test Panics:** Handle watcher channel disconnects (`TryRecvError::Disconnected`), poisoned mutexes, missing manifests, transient `ScheduleLeased` retries, and OS file-replace races via `tracing` + `pending_retries` + `ModHotReloadHistory` instead of `.unwrap()`.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --all -- --check
cargo clippy -p bamabi_watcher --all-targets --all-features -- -D warnings
cargo test -p bamabi_watcher --all-features
RUSTDOCFLAGS="-D warnings" cargo doc -p bamabi_watcher --no-deps --all-features
typos
```

---

## 6. Workspace Coordination
1. **Standalone `bamabi-core` Member:** `bamabi_watcher` is a Layer 2 workspace member whose shared dependencies and lints live in `../../Cargo.toml` (with optional local sibling overrides via `bamabi-dev-overlay/overlay.py`).
2. **Self-Contained C99 Watcher Tests:** Integration tests render `bamabi.h` via `bamabi_sdk_gen::render_c99_header()` into temp directories with zero sibling repository dependencies.
3. **Pairing with `cargo-bam`:** Keep reload triggers compatible with `cargo bam watch --out-dir` (`bamabi-tools`) atomic `.sig`-before-dylib staging.
