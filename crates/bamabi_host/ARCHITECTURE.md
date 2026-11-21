# `bamabi_host` Architecture & Implementation Guide

`bamabi_host` (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1`) is the **`#![forbid(unsafe_code)]`, slim headless Bevy ECS host crate** in Layer 2 ([`bamabi-core`](../../README.md)).

---

## 1. Crate Role & Headless Isolation Boundaries

1. **Zero `unsafe` Code:** [`src/lib.rs`](src/lib.rs) declares `#![forbid(unsafe_code)]` and `#![warn(missing_docs)]`. All raw pointer/FFI/dynamic-linker code stays sealed in [`bamabi_host_sys`](../bamabi_host_sys/README.md).
2. **Strict Headless Bevy Dependencies:** [`Cargo.toml`](Cargo.toml) permits only `bevy_ecs`, `bevy_app`, `bevy_platform`, and optional `bevy_diagnostic` (`diagnostics`), plus `bamabi_sys`, `bamabi_manifest`, `bamabi_host_sys`, `bamabi_macros`, `toml`, `semver`, and `tracing`. It **never** pulls in `bevy_render`, `bevy_reflect`, `bevy_scene`, `bevy_asset`, `bevy_winit`, `bevy_window`, `wgpu`, or `notify`.
3. **Zero Non-Test Panics & Companion Decoupling:** `src/` keeps non-test `.unwrap()`, `.expect()`, `panic!`, `unreachable!`, `todo!`, and `#[allow(...)]` at zero. Rendering, reflection, assets, scenes, and file watching plug in externally through `ModUnloadHooks` and `bamabi_host_sys` traits.

### Feature Flags

| Feature | Dependencies Enabled | Purpose |
|---|---|---|
| `strict-ffi-checks` | `bamabi_host_sys/strict-ffi-checks` | Enables hot-path FFI handle magic/generation validation even in release builds. |
| `test-utils` | `bamabi_host_sys/test-utils` | Exposes mock `LoadedMod` registration helpers for test crates. |
| `diagnostics` | `bamabi_host_sys/diagnostics`, `dep:bevy_diagnostic` | Publishes per-mod and aggregate `bamabi/*` metrics to Bevy `DiagnosticsStore`. |
| `signature-verification` | `bamabi_host_sys/signature-verification` | Enables pre-`dlopen` `.sig` / `.bam_sig` / `.bampack` / `SSHKRL` verification without enabling `bamabi_trust/keygen` (`ssh-key/getrandom`). |
| `keygen` | `signature-verification`, `bamabi_host_sys/keygen` | Opt-in feature exposing OpenSSH Ed25519 keypair generation and certificate issuance (`keygen` module at crate root, plus `GeneratedKeypair`, `IssuedCertificate`, `generate_ed25519_keypair`, `issue_ed25519_certificate_openssh`, and `write_openssh_keypair` at crate root and `prelude`). |

---

## 2. Module-by-Module Map

| Module | Path | Contents |
|---|---|---|
| **Crate root** | [`src/lib.rs`](src/lib.rs) | `#![forbid(unsafe_code)]`, flat re-exports of host APIs + `bamabi_host_sys` types + `signature-verification` & `keygen` trust items, and `prelude`. |
| **Plugin & Flush** | [`src/plugin.rs`](src/plugin.rs) | `AbiModPlugin`, `ConfiguredAbiModPlugin`, `ModLifecycleFlush`, `ensure_mod_lifecycle_flush_schedule`, `ensure_startup_tracker`. |
| **App extension** | [`src/app_ext.rs`](src/app_ext.rs) | `ModAppExt` ergonomic `App` wrapper delegating to `ModWorldExt` (plus `compile_fail` doctest proving `ResMut<ModHost>` cannot unload). |
| **World extension** | [`src/world_ext.rs`](src/world_ext.rs) | `ModWorldExt`, `ModUnloadHookFn`, `ModUnloadHooks`, `register_mod_unload_hook_in_world`, `load_mod*`, `unload_mod`, `unload_all_mods`, `reload_mod*`, `load_mods_dir*`, `evict_revoked_mods`. |
| **Commands extension** | [`src/commands_ext.rs`](src/commands_ext.rs) | `ModCommandsExt` (`load_mod`, `load_mod_with`, `unload_mod`, `reload_mod`, `reload_mod_with`), `PendingModLifecycleOps`, `is_safe_to_run_lifecycle_op_now`, `flush_pending_mod_lifecycle_ops`. |
| **Schedules & sets** | [`src/schedule_sets.rs`](src/schedule_sets.rs) | `leased_schedules(world)`, `prevalidate_mod_schedules_and_ordering`, `configure_guest_system_in_world`, `configure_weak_set_chain`, `schedule_mod_systems_in_world`, `run_late_startup_systems_in_world`. |
| **SemVer DAG** | [`src/mod_dag.rs`](src/mod_dag.rs) | `ModDependencyGraph`, `ModDagNode`, `DagResolution`, `DirectoryLoadReport`, `ModDagError`, `ModDagErrorKind` (`as_str()`), 13 structured error accessors, `discover_mod_manifests`, `resolve_dependency_dag`. |
| **Budgets** | [`src/budgets.rs`](src/budgets.rs) | `ModBudgetPolicy`, `ModBudgetLimits`, `ModBudgetBreach`, `ModBudgetBreachKind`, `ModBudgetSnapshot`, `ModBudgetTelemetry`, `ModBudgetExt`, `LatencyWindow` (`p50_us`/`p99_us` watchdog). |
| **Diagnostics** | [`src/diagnostics.rs`](src/diagnostics.rs) | `#[cfg(feature = "diagnostics")]` `flush_mod_diagnostics` publishing `bamabi/total/*` and `bamabi/mod/<name>/*` metrics to `DiagnosticsStore`. |
| **Events** | [`src/events.rs`](src/events.rs) | Re-exports `LoadedModInfo`, `ModLoaded`, `ModUnloaded`, `ModSystemFaulted`, `ModSystemError`, `SystemInitError`, and `VerifiedModSignature`. |

---

## 3. Core Lifecycle & Safety Architecture

### 3.1 `ModLifecycleFlush` & Leased-Schedule Contract (`ScheduleLeased`)

While Bevy executes a schedule (e.g. `Update`, `FixedUpdate`, `PreUpdate`, `Last`), that schedule is temporarily removed (*leased*) from `World`'s `Schedules` resource. Modifying or unmapping a mod whose systems target a currently leased schedule would panic, miss set excision, or risk use-after-`dlclose`. `bamabi_host` enforces a strict two-layer defense:

1. **Dedicated Safe-Point Schedule (`ModLifecycleFlush`):**
   - `ensure_mod_lifecycle_flush_schedule(app)` (called by `AbiModPlugin` and `ModHotReloadPlugin`) idempotently initializes `ModLifecycleFlush`, inserts it immediately after `Last` in `MainScheduleOrder` (or pushes it if `Last` is absent), initializes `PendingModLifecycleOps`, and registers `flush_pending_mod_lifecycle_ops` into `ModLifecycleFlush`.
   - Because guest mods cannot target `ModLifecycleFlush` via built-in `ScheduleId`s, while `ModLifecycleFlush` executes only `Main` and `ModLifecycleFlush` are leased.
2. **`leased_schedules(world)` & `LoadedMod::registered_schedule_labels()`:**
   - [`leased_schedules(world)`](src/schedule_sets.rs) inspects `Schedules::get_temporarily_removed()`.
   - [`LoadedMod::registered_schedule_labels()`](../bamabi_host_sys/src/loader/mod_host.rs) yields every `InternedScheduleLabel` where the mod registered guest systems or scoped system sets.
3. **`ModCommandsExt` Deferral Rule (`commands_ext.rs`):**
   - Commands queued via `Commands::load_mod*`, `unload_mod`, or `reload_mod*` check `is_safe_to_run_lifecycle_op_now(world)` when `PendingModLifecycleOps` is present:
     - **Safe condition:** `leased ⊆ {ModLifecycleFlush, Main}` **and** `(ModLifecycleFlush ∈ leased || Main ∉ leased)`.
     - Whenever any other schedule (e.g. `Update`, `FixedUpdate`, `PreUpdate`) is leased, or `Main` is running outside `ModLifecycleFlush`, the closure is pushed onto `PendingModLifecycleOps` and executed FIFO during `ModLifecycleFlush`.
     - If a queued op itself leases a non-`Main`, non-`ModLifecycleFlush` schedule during `flush_pending_mod_lifecycle_ops`, remaining ops are re-queued ahead of any newly appended items and retried on the next `ModLifecycleFlush`.
4. **Direct `ModWorldExt` Guards (`ModLoadError::ScheduleLeased`):**
   - **Load (`load_mod*`):** `prevalidate_mod_schedules_and_ordering` checks every non-late-startup system's resolved `InternedScheduleLabel` against `leased_schedules(world)`. If any target schedule is leased, it rolls back via `rollback_and_cleanup_reload` and returns `Err(ModLoadError::ScheduleLeased { id, schedule })`.
   - **Unload / Reload (`unload_mod`, `reload_mod*`):** `ensure_cascade_schedules_not_leased` resolves the full reverse-dependency cascade `[root_id, ...dependents]` and intersects all mods' `registered_schedule_labels()` with `leased_schedules(world)` *before* saving state or unloading any mod. Any conflict aborts atomically with `Err(ModLoadError::ScheduleLeased { id: offending_id, schedule })`.
5. **`unload_all_mods()` Skip-Leased Semantics (`world_ext.rs`):**
   - Iterates loaded mods in LIFO order. For each mod whose cascade overlaps a currently leased schedule (or returns an unload error), `unload_all_mods()` emits a `tracing::warn!` and **skips** that mod rather than aborting or panicking, so unleased mods unload cleanly while `ModHost` may remain non-empty. `ModDependencyGraph::clear()` only runs if `ModHost::is_empty()`.

### 3.2 `ModDagErrorKind` & Structured Error Inspection (`mod_dag.rs`)

`load_mods_dir`, `discover_mod_manifests`, and `resolve_dependency_dag` surface structured diagnostics via [`ModDagError`](src/mod_dag.rs) (14 variants) and fieldless [`ModDagErrorKind`](src/mod_dag.rs):

- **`ModDagErrorKind::as_str(self) -> &'static str`** returns stable `snake_case` tags: `"io"`, `"toml_parse"`, `"missing_mod_name"`, `"invalid_version"`, `"invalid_abi_version"`, `"incompatible_abi_version"`, `"invalid_dependency_req"`, `"duplicate_mod_name"`, `"missing_library"`, `"missing_dependency"`, `"incompatible_dependency_version"`, `"cycle_quarantined"`, `"load_failed"`, `"mod_not_found"`.
- **13 Structured Accessors on `ModDagError`:**
  - `kind(&self) -> ModDagErrorKind`
  - `path(&self) -> Option<&Path>` (returns `second_path` for `DuplicateModName`)
  - `related_paths(&self) -> Vec<&Path>` (`[first_path, second_path]` for `DuplicateModName`)
  - `mod_name(&self) -> Option<&str>`
  - `dependency(&self) -> Option<&str>`
  - `required_version(&self) -> Option<&semver::VersionReq>`
  - `found_version(&self) -> Option<&semver::Version>` (`found` for `IncompatibleDependencyVersion`; `host_abi` for `IncompatibleAbiVersion`)
  - `cycle_members(&self) -> &[String]`
  - `load_error(&self) -> Option<&ModLoadError>`
  - `mod_id(&self) -> Option<ModId>`
  - `is_cycle(&self) -> bool`
  - `is_io(&self) -> bool` (checks both `ModDagError::Io` and the `Error::source()` chain for nested `std::io::Error`, e.g., `LoadFailed` wrapping `ShadowCopy` or `MissingSignatureFile`)
  - `is_recoverable(&self) -> bool` (`false` for directory-aborting `Io` and lookup-specific `ModNotFound`; `true` for the 12 per-mod directory diagnostics buffered into `DirectoryLoadReport::errors`)

### 3.3 Trust Parity & `keygen` Gate

With `--features signature-verification`, [`src/lib.rs`](src/lib.rs) re-exports `ModTrustPolicy`, `VerifiedModSignature`, `KrlBuilder`, `BamPackBuilder`, `VerifiedBamPack`, `BamPackExtractLimits`, `BamPackExtractError`, `bampack`, `embedded_sig`, `ssh_key`, `LineEnding`, `Algorithm`, `Certificate`, `Fingerprint`, `HashAlg`, `PrivateKey`, `PublicKey`, and signing/verification helpers from `bamabi_host_sys` without pulling in `ssh-key/getrandom`. Enabling opt-in `--features keygen` (`["signature-verification", "bamabi_host_sys/keygen"]`) additionally exposes `GeneratedKeypair`, `IssuedCertificate`, `generate_ed25519_keypair`, `issue_ed25519_certificate_openssh`, and `write_openssh_keypair` in both the crate root and `prelude` (plus the `keygen` module at the crate root).

---

## 4. Setup, Testing & Benchmarks

- **Headless check:** `cargo tree -p bamabi_host --all-features | grep -E 'bevy_render|bevy_reflect|bevy_scene|bevy_asset|\bnotify\b|bevy_winit|bevy_window|wgpu|glutin'` must print nothing.
- **Tests & benchmarks:** See [`TESTS.md`](TESTS.md) for all `7` `[[test]]` suites, `8` unit test suites, doctests, `chunk_throughput` (9 groups), and `v03_subsystem_bench` (8 ABI `0.3.0` subsystem benchmarks).
