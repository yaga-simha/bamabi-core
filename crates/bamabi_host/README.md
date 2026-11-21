# `bamabi_host`

Safe (`#![forbid(unsafe_code)]`), **slim headless** Bevy ECS (`v0.20.1`) integration crate for `bamabi` (`ABI 0.3.0`, `v0.3.0`, Layer 2 in [`bamabi-core`](../../README.md)).

## Documentation Map

- **Crate Architecture & Implementation:** [`ARCHITECTURE.md`](ARCHITECTURE.md) — module map, headless isolation boundary, `ModLifecycleFlush` & leased-schedule safety contract (`ScheduleLeased`), `ModDagErrorKind` & accessors, budgets/diagnostics, and `signature-verification` parity.
- **Test & Benchmark Guide:** [`TESTS.md`](TESTS.md) — complete inventory of unit tests (`15`), `7` `[[test]]` integration suites (`ffi`, `scheduling`, `lifecycle`, `e2e`, `mod_budgets`, `schedule_shuffle_fuzz`, `soak_10k_reloads`), doctests, and `chunk_throughput` / `v03_subsystem_bench` benchmarks.
- **Workspace Guides:** [`../../docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) · [`../../TESTS.md`](../../TESTS.md) · [`GEMINI.md`](GEMINI.md)

## Overview

`bamabi_host` intentionally pulls in **zero** `bevy_render`, `bevy_reflect`, `bevy_scene`, `bevy_asset`, or `notify` dependencies, making it ideal for headless dedicated servers, simulations, and CLI tools while providing the idiomatic Bevy plugin, budgets, extension traits, schedule/set bindings, `mod.toml` SemVer DAG loading, and lifecycle messages:

- **`AbiModPlugin` & `ModLifecycleFlush`**: Initializes `ModRegistry`, `ModHost`, `ModQuarantine`, `ModScheduleMap`, `ModVisibleSets`, `ModUnloadHooks`, `ModBudgetPolicy` / `ModBudgetTelemetry`, `WorldDropGuard`, and `Messages<ModLoaded>` / `Messages<ModUnloaded>` / `Messages<ModSystemFaulted>` / `Messages<ModBudgetBreach>`. Installs the dedicated safe-point schedule [`ModLifecycleFlush`](src/plugin.rs) after `Last` in `MainScheduleOrder` via `ensure_mod_lifecycle_flush_schedule(app)` to drain deferred `ModCommandsExt` lifecycle operations cleanly.
- **`ModBudgetPolicy` & `ModBudgetTelemetry`**: Enforces global and per-mod `CommandArena` byte caps (`CAPACITY_EXCEEDED`), atomic per-frame spawn quotas across all spawn commands, and sliding-window `p50_us`/`p99_us` frame-time watchdogs with auto-quarantine and `ModBudgetExt` (`App`/`World`) configuration.
- **`ModAppExt`, `ModWorldExt` & `ModUnloadHooks`**: Exposes Bevy components (`ComponentOptions` including `with_summary_tick()` validated against `StorageType::Table`), singleton resources (`register_mod_resource`), buffered POD and non-POD messages (`register_mod_message`, `register_mod_non_pod_message`), triggerable `Event` / `EntityEvent` types, custom schedules (`ScheduleId::custom`), `"host::<name>"` visible `SystemSet`s, `load_mods_dir` / `ModDependencyGraph` DAG resolution via `bamabi_manifest` (with `ModDagErrorKind::as_str()` and 13 structured `ModDagError` accessors), Bevy `0.20.1` `despawn_all_where` cleanup (including `Allow<Disabled>` mod entities), leased-schedule safety guards (`leased_schedules`, `ModLoadError::ScheduleLeased`, skip-leased `unload_all_mods()`), and extensible `ModUnloadHooks` for decoupled companion cleanup.
- **`ModCommandsExt`**: Queues deferred mod loads, reloads, and unloads safely via Bevy `Commands`, automatically deferring execution into `PendingModLifecycleOps` (drained in `ModLifecycleFlush`) whenever any frame/mod schedule (or `Main` outside `ModLifecycleFlush`) is leased out of `Schedules`.
- **`ModQuarantine`**: Supports runtime inspection and `reenable(mod_id, system_name)` recovery for quarantined guest systems.
- **Optional Feature Integrations**: `strict-ffi-checks`, `diagnostics` (aggregate & per-mod `bevy_diagnostic` counters including budget breaches, p99 time, spawns, and arena peak bytes), `signature-verification` (`ModTrustPolicy` + `evict_revoked_mods` + full non-`keygen` `bamabi_trust` re-exports including `bampack`, `embedded_sig`, and `ssh_key`), and `keygen` (implies `signature-verification` and enables `bamabi_trust/keygen`, re-exporting `keygen`, `GeneratedKeypair`, `IssuedCertificate`, `generate_ed25519_keypair`, `issue_ed25519_certificate_openssh`, and `write_openssh_keypair`). Optional Bevy subsystem bridges (and curated presets), render/material extraction, and dynamic reflection/savegames/telemetry live in Layer 3 ([`bamabi`](https://github.com/yaga-simha/bamabi): `bamabi_bridges`, `bamabi_render`, `bamabi_reflect`), while hot-reload watching lives in [`bamabi_watcher`](../bamabi_watcher/README.md) (all re-exported by canonical `bamabi`).

All `unsafe` FFI internals, Bevy `System` bridging, `HostIter` chunk iteration, `CommandArena` buffers, and `libloading` calls live in [`bamabi_host_sys`](../bamabi_host_sys/README.md).

## Standalone Usage

For slim headless Bevy ECS servers or simulations that do not need rendering/reflection/bridges (full Bevy games should typically depend on [`bamabi`](https://github.com/yaga-simha/bamabi)):

```toml
[dependencies]
bamabi_host = { git = "https://github.com/yaga-simha/bamabi-core.git", branch = "main" }

# Optional diagnostics + Ed25519 signature verification:
# bamabi_host = { git = "https://github.com/yaga-simha/bamabi-core.git", branch = "main", features = ["diagnostics", "signature-verification"] }
```

## Verification & Benchmarks

See [`TESTS.md`](TESTS.md) for the complete per-suite test and benchmark inventory.

```bash
cargo test -p bamabi_host --all-features
cargo bench -p bamabi_host --bench chunk_throughput -- --test
cargo bench -p bamabi_host --bench v03_subsystem_bench -- --test
cargo clippy -p bamabi_host --all-targets --all-features -- -D warnings
cargo tree -p bamabi_host --all-features | grep -E "bevy_render|bevy_reflect|bevy_scene|bevy_asset|notify"   # must print nothing
RUSTDOCFLAGS="-D warnings" cargo doc -p bamabi_host --no-deps --all-features
```

## License

Dual-licensed under `MIT OR Apache-2.0` ([`LICENSE-MIT`](../../LICENSE-MIT) and [`LICENSE-APACHE`](../../LICENSE-APACHE)).
