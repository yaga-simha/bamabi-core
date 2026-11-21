# Agent Guide: `bamabi_host`

Crate guide for **`bamabi_host`** (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1+`) — the safe, **slim headless** Bevy ECS host plugin crate.

Documentation index: [`README.md`](README.md) · [`ARCHITECTURE.md`](ARCHITECTURE.md) · [`TESTS.md`](TESTS.md).

---

## 1. Crate Identity
- **Name:** `bamabi_host` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Provides the ergonomic, safe Bevy ECS host API (`AbiModPlugin`, `ModLifecycleFlush`, `ModAppExt`, `ModWorldExt`, `ModCommandsExt`, `ModBudgetPolicy`, `ModUnloadHooks`) decoupled from rendering, reflection, scenes, assets, and filesystem watchers.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root. Zero `unsafe` blocks anywhere; all FFI/`libloading` internals live in `bamabi_host_sys`.
- **Headless Isolation (CRITICAL):** **NEVER** depend on `bevy_render`, `bevy_reflect`, `bevy_scene`, `bevy_asset`, or `notify` (even transitively through feature flags). Allowed Bevy deps are strictly `bevy_ecs`, `bevy_app`, `bevy_platform`, and optional `bevy_diagnostic` (`diagnostics`).

---

## 3. Owned Surface & Feature Flags
- **Features:** `strict-ffi-checks`, `test-utils`, `diagnostics` (enables `bevy_diagnostic`), `signature-verification` (delegates to `bamabi_trust` via `bamabi_host_sys`, re-exporting all non-`keygen` `bamabi_trust` items; `bamabi_trust::keygen` remains upstream-gated).
- **Plugin & Safe-Point Lifecycle (`plugin.rs`, `commands_ext.rs`, `schedule_sets.rs`, `world_ext.rs`, `app_ext.rs`, `events.rs`):**
  - `AbiModPlugin`, `ConfiguredAbiModPlugin`, `ModLifecycleFlush` (inserted after `Last` in `MainScheduleOrder`), `ensure_mod_lifecycle_flush_schedule(app)`, `leased_schedules(world)`, `ModQuarantine`, `ModVisibleSets`, `ModUnloadHooks`, and `prelude`.
  - `ModCommandsExt` defers lifecycle ops into `PendingModLifecycleOps` for `ModLifecycleFlush` whenever `!(leased ⊆ {ModLifecycleFlush, Main} && (ModLifecycleFlush ∈ leased || Main ∉ leased))`.
  - Direct `ModWorldExt` loads/unloads/reloads reject leased schedules with `ModLoadError::ScheduleLeased { id, schedule }`; `unload_all_mods()` logs a warning and skips leased mods so `ModHost` may remain non-empty.
  - `ModWorldExt::despawn_loaded_mod_entities` uses Bevy `0.20.1` `world.despawn_all_where::<&LoadedModInfo, Allow<Disabled>>(...)`.
  - `register_mod_component` validates `BAM_STORAGE_FLAG_SUMMARY_TICK` / `ComponentOptions::with_summary_tick()` against `ComponentInfo::summary_tick()` and `StorageType::Table`.
- **Quotas, Diagnostics & DAG Loading (`budgets.rs`, `diagnostics.rs`, `mod_dag.rs`):**
  - `ModBudgetPolicy`, `ModBudgetLimits`, `ModBudgetBreach`, `ModBudgetTelemetry`, `ModBudgetExt`, and Bevy diagnostic counters (`diagnostics.rs`).
  - `load_mods_dir` / `ModDependencyGraph` (`bamabi_manifest`), `ModDagErrorKind` (`as_str()`), and all 13 structured `ModDagError` accessors (`kind`, `path`, `related_paths`, `mod_name`, `dependency`, `required_version`, `found_version`, `cycle_members`, `load_error`, `mod_id`, `is_cycle`, `is_io`, `is_recoverable`).

---

## 4. Strict Engineering Rules
1. **Zero Non-Test Panics & Keep `src/` Lean:** Maintain zero non-test `.unwrap()` / `.expect()` / `panic!` / `unreachable!` / `todo!` and zero `#[allow(...)]` suppressions in `src/`.
2. **Idiomatic Bevy ECS (`0.20+`):** Prefer `Res<T>`/`&T` over mutable borrows, narrow queries with `With`/`Without`, use `despawn_all_where`, `.chain_weak()`, and `On<E>` observers cleanly.
3. **Preserve Companion Decoupling:** Extensions requiring rendering, reflection, assets, scenes, or file watching belong in `bamabi_render`, `bamabi_reflect`, `bamabi_bridges`, or `bamabi_watcher` hooked via `ModUnloadHooks` — never here.
4. **Leased-Schedule Invariant:** Never bypass `leased_schedules` / `ModLifecycleFlush` guards; any schedule mutation must skip or reject leased schedules deterministically (`ScheduleLeased`).

---

## 5. Standalone Verification Commands

```bash
cargo fmt --all -- --check
cargo clippy -p bamabi_host --all-targets --all-features -- -D warnings
cargo clippy -p bamabi_host --all-targets --no-default-features -- -D warnings
cargo tree -p bamabi_host --all-features | grep -E "bevy_render|bevy_reflect|bevy_scene|bevy_asset|notify"   # MUST print NOTHING
BAM_FIXTURE_DIR="/path/to/fixtures" cargo test -p bamabi_host --all-features
cargo bench -p bamabi_host --bench chunk_throughput -- --test
cargo bench -p bamabi_host --bench v03_subsystem_bench -- --test
RUSTDOCFLAGS="-D warnings" cargo doc -p bamabi_host --no-deps --all-features
typos
```

---

## 6. Workspace Coordination
1. **Standalone `bamabi-core` Member:** `bamabi_host` is a slim headless Layer 2 workspace member whose shared dependencies and lints live in `../../Cargo.toml` (pointing to Layer 0 `bamabi-wire` and Layer 1 `bamabi-tools` via GitHub git URLs, with optional local sibling overrides via `bamabi-dev-overlay/overlay.py`).
2. **Self-Contained Tests & Benchmarks:** Note `autotests = false` in [`Cargo.toml`](Cargo.toml); all 7 integration suites (`ffi`, `scheduling`, `lifecycle`, `e2e`, `mod_budgets`, `schedule_shuffle_fuzz`, `soak_10k_reloads`), 8 unit suites in `tests/unit/`, and 2 `[[bench]]` targets (`chunk_throughput`, `v03_subsystem_bench`) live inside `crates/bamabi_host/`. Tests honor `BAM_FIXTURE_DIR`, `BAM_INCLUDE_DIR`, and `BAM_WIRE_DIR` / sibling `../bamabi-wire` when present, and otherwise fall back automatically to `bamabi_sdk_gen`, vendored `tests/asymmetric_c_mod/include/generated/testbed_components.h`, and `https://github.com/yaga-simha/bamabi-wire.git`.
3. **Downstream Consumers:** Re-exported by `bamabi` (Layer 3 canonical facade) and consumed by `bamabi_watcher`, `bamabi_bridges`, `bamabi_render`, and `bamabi_reflect`.
