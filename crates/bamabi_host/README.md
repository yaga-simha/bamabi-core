# `bamabi_host`

Safe (`#![forbid(unsafe_code)]`), **slim headless** Bevy ECS (`v0.20.1`) integration crate for `bamabi` (`ABI 0.3.0`, `v0.3.0`, Layer 2 in [`bamabi-core`](../../README.md)).

`bamabi_host` intentionally pulls in **zero** `bevy_render`, `bevy_reflect`, `bevy_scene`, `bevy_asset`, or `notify` dependencies, making it ideal for headless dedicated servers, simulations, and CLI tools while providing the idiomatic Bevy plugin, budgets, extension traits, schedule/set bindings, `mod.toml` SemVer DAG loading, and lifecycle messages:

- **`AbiModPlugin`**: Initializes `ModRegistry`, `ModHost`, `ModQuarantine`, `ModScheduleMap`, `ModVisibleSets`, `ModUnloadHooks`, `ModBudgetPolicy` / `ModBudgetTelemetry`, `WorldDropGuard`, and `Messages<ModLoaded>` / `Messages<ModUnloaded>` / `Messages<ModSystemFaulted>` / `Messages<ModBudgetBreach>`.
- **`ModBudgetPolicy` & `ModBudgetTelemetry`**: Enforces global and per-mod `CommandArena` byte caps (`CAPACITY_EXCEEDED`), atomic per-frame spawn quotas across all spawn commands, and sliding-window `p50_us`/`p99_us` frame-time watchdogs with auto-quarantine and `ModBudgetExt` (`App`/`World`) configuration.
- **`ModAppExt`, `ModWorldExt` & `ModUnloadHooks`**: Exposes Bevy components (`ComponentOptions` including `with_summary_tick()` validated against `StorageType::Table`), singleton resources (`register_mod_resource`), buffered POD and non-POD messages (`register_mod_message`, `register_mod_non_pod_message`), triggerable `Event` / `EntityEvent` types, custom schedules (`ScheduleId::custom`), `"host::<name>"` visible `SystemSet`s, `load_mods_dir` / `ModDependencyGraph` DAG resolution via `bamabi_manifest` (in `bamabi-tools`), Bevy `0.20.1` `despawn_all_where` cleanup (including `Allow<Disabled>` mod entities), and extensible `ModUnloadHooks` for decoupled companion cleanup.
- **`ModCommandsExt`**: Queues deferred mod loads, reloads, and unloads safely via Bevy `Commands`.
- **`ModQuarantine`**: Supports runtime inspection and `reenable(mod_id, system_name)` recovery for quarantined guest systems.
- **Optional Feature Integrations**: `strict-ffi-checks`, `diagnostics` (aggregate & per-mod `bevy_diagnostic` counters including budget breaches, p99 time, spawns, and arena peak bytes), and `signature-verification` (`ModTrustPolicy` + `evict_revoked_mods` backed by `bamabi_trust` in [`bamabi-tools`](https://github.com/yaga-simha/bamabi-tools)). Optional Bevy subsystem bridges (and curated presets), render/material extraction, and dynamic reflection/savegames/telemetry live in Layer 3 ([`bamabi`](https://github.com/yaga-simha/bamabi): `bamabi_bridges`, `bamabi_render`, `bamabi_reflect`), while hot-reload watching lives in [`bamabi_watcher`](../bamabi_watcher/README.md) (all re-exported by canonical `bamabi`).

All `unsafe` FFI internals, Bevy `System` bridging, `HostIter` chunk iteration, `CommandArena` buffers, and `libloading` calls live in [`bamabi_host_sys`](../bamabi_host_sys/README.md).

## Standalone Usage

For slim headless Bevy ECS servers or simulations that do not need rendering/reflection/bridges (full Bevy games should typically depend on [`bamabi`](https://github.com/yaga-simha/bamabi)):

```toml
[dependencies]
bamabi_host = { git = "https://github.com/yaga-simha/bamabi-core.git", branch = "main" }

# Optional diagnostics + Ed25519 signature verification:
# bamabi_host = { git = "https://github.com/yaga-simha/bamabi-core.git", branch = "main", features = ["diagnostics", "signature-verification"] }
```

## Verification

```bash
cargo test -p bamabi_host --all-features
cargo clippy -p bamabi_host --all-targets --all-features -- -D warnings
cargo tree -p bamabi_host --all-features | grep -E "bevy_render|bevy_reflect|bevy_scene|bevy_asset|notify"   # must print nothing
RUSTDOCFLAGS="-D warnings" cargo doc -p bamabi_host --no-deps --all-features
```

## License

Dual-licensed under `MIT OR Apache-2.0` ([`LICENSE-MIT`](../../LICENSE-MIT) and [`LICENSE-APACHE`](../../LICENSE-APACHE)).
