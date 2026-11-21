# `bamabi_host` Test & Benchmark Guide

This document inventories every unit test suite, integration test binary, doctest, and benchmark harness in `bamabi_host`. Note that [`Cargo.toml`](Cargo.toml) sets `autotests = false` so integration targets are declared explicitly via `[[test]]` and `[[bench]]`.

---

## 1. Suite Overview & Commands

| Layer | Target / Path | Count | Command |
|---|---|---|---|
| **Unit tests (`--lib`)** | `8` `#[path = ...]` files in [`tests/unit/`](tests/unit/) | `15` tests | `cargo test -p bamabi_host --lib --all-features` |
| **FFI boundary & bridge** | `[[test]] name = "ffi"` ([`tests/ffi/main.rs`](tests/ffi/main.rs)) | `121` tests (`10` submodules) | `cargo test -p bamabi_host --test ffi --all-features` |
| **Scheduling & differential** | `[[test]] name = "scheduling"` ([`tests/scheduling/main.rs`](tests/scheduling/main.rs)) | `47` tests (`6` submodules) | `cargo test -p bamabi_host --test scheduling --all-features` |
| **Lifecycle & soundness** | `[[test]] name = "lifecycle"` ([`tests/lifecycle/main.rs`](tests/lifecycle/main.rs)) | `59` tests (`11` submodules) | `cargo test -p bamabi_host --test lifecycle --all-features` |
| **Asymmetric C99 & Rust E2E** | `[[test]] name = "e2e"` ([`tests/e2e/main.rs`](tests/e2e/main.rs)) | `12` tests | `cargo test -p bamabi_host --test e2e --all-features` |
| **Quotas & watchdogs** | `[[test]] name = "mod_budgets"` ([`tests/mod_budgets.rs`](tests/mod_budgets.rs)) | `9` tests | `cargo test -p bamabi_host --test mod_budgets --all-features` |
| **Schedule shuffle fuzzing** | `[[test]] name = "schedule_shuffle_fuzz"` ([`tests/schedule_shuffle_fuzz.rs`](tests/schedule_shuffle_fuzz.rs)) | `2` tests | `cargo test -p bamabi_host --test schedule_shuffle_fuzz --all-features` |
| **Hot-reload soak** | `[[test]] name = "soak_10k_reloads"` ([`tests/soak_10k_reloads.rs`](tests/soak_10k_reloads.rs)) | `1` test (`250` default / `10,000` via `BAM_SOAK_CYCLES`) | `BAM_SOAK_CYCLES=10000 cargo test -p bamabi_host --test soak_10k_reloads --release` |
| **Doctests** | `src/lib.rs`, `src/app_ext.rs` | `2` (`1` `no_run` + `1` `compile_fail`) | `cargo test --doc -p bamabi_host --all-features` |
| **Core throughput bench** | `[[bench]] name = "chunk_throughput"` ([`benches/chunk_throughput.rs`](benches/chunk_throughput.rs)) | `9` groups | `cargo bench -p bamabi_host --bench chunk_throughput -- --test` |
| **ABI 0.3.0 subsystem bench** | `[[bench]] name = "v03_subsystem_bench"` ([`benches/v03_subsystem_bench.rs`](benches/v03_subsystem_bench.rs)) | `8` groups | `cargo bench -p bamabi_host --bench v03_subsystem_bench -- --test` |

> **Total with `--all-features`:** `15` unit + `251` integration + `2` doctests = `268` tests (`0` `#[ignore]`). Without `--features signature-verification`, the `11` tests in `lifecycle::signature_verification` are gated off (`48` tests in `lifecycle`), and without `--features diagnostics`, `diagnostics`-specific assertions fall back accordingly.

---

## 2. Unit Test Suites (`tests/unit/`, `15` tests)

| Source Module | Unit Test File | Tests | What It Verifies |
|---|---|---|---|
| `src/app_ext.rs` | [`app_ext_coverage_tests.rs`](tests/unit/app_ext_coverage_tests.rs) | `1` | `ModAppExt::register_mod_non_pod_message`, `set_mod_error_policy`, and `load_mods_dir*` forwarding. |
| `src/budgets.rs` | [`budgets_tests.rs`](tests/unit/budgets_tests.rs) | `1` | `LatencyWindow` sliding-window `p50_us`/`p99_us` calculation, grow/shrink resizing, and zero clamping. |
| `src/commands_ext.rs` | [`commands_ext_tests.rs`](tests/unit/commands_ext_tests.rs) | `4` | `ensure_mod_lifecycle_flush_schedule` idempotency (with/without `MainScheduleOrder`), deferral when `FixedUpdate`/`Main` is leased, drain in `ModLifecycleFlush`, and partial-flush re-queueing. |
| `src/mod_dag.rs` | [`mod_dag_tests.rs`](tests/unit/mod_dag_tests.rs), [`mod_dag_coverage_tests.rs`](tests/unit/mod_dag_coverage_tests.rs) | `4` + `2` | `ModDagErrorKind::as_str()` uniqueness, all 13 `ModDagError` structured accessors across all 14 variants (incl. `is_io` source chains & `is_recoverable`), `From<ManifestError>`, `ModDependencyGraph` rename/dependency tracking. |
| `src/plugin.rs` | [`plugin_coverage_tests.rs`](tests/unit/plugin_coverage_tests.rs) | `1` | `ConfiguredAbiModPlugin` builders and `ensure_mod_lifecycle_flush_schedule` fallback when `Last` is absent. |
| `src/schedule_sets.rs` | [`schedule_sets_coverage_tests.rs`](tests/unit/schedule_sets_coverage_tests.rs) | `1` | `prevalidate_mod_schedules_and_ordering` edges, `configure_weak_set_chain` leased-schedule refusal, and `run_late_startup_systems_in_world` edges. |
| `src/world_ext.rs` | [`world_ext_coverage_tests.rs`](tests/unit/world_ext_coverage_tests.rs) | `1` | `ModUnloadHooks` register/unregister/order and `ModWorldExt` error branches. |

---

## 3. Integration Test Binaries (`251` tests across `7` binaries)

1. **[`ffi`](tests/ffi/main.rs) (`121` tests across `10` submodules):**
   - `batch_cmds_and_messages` (`4`), `boundary_validation` (`20`), `c_vtable_interop` (`1`), `chunk_and_commands` (`34`), `engine_bridge` (`15`), `events_hierarchy_and_parallel` (`8`), `query_filters` (`7`: `Allow<Disabled>`, `Has<T>`, `Or<...>`, `RemovedComponents`), `sparse_and_resources` (`12`), `strides_and_alignment` (`19`), `uuid_parity` (`1`).
2. **[`scheduling`](tests/scheduling/main.rs) (`47` tests across `6` submodules):**
   - `conflicts_and_faults` (`14`), `differential_parity` (`8`, incl. `2` `proptest` suites comparing native Bevy vs dynamic `GuestSystem`), `multi_query_and_conditions` (`7`), `ordering_and_validation` (`10`), `sets_and_diagnostics` (`4`), `weak_ordering_and_fallback` (`4`).
3. **[`lifecycle`](tests/lifecycle/main.rs) (`59` tests across `11` submodules):**
   - `drop_trampolines` (`7`), `handshake_and_strict_load` (`4`), `host_unit_basics` (`5`), `hot_reload_and_excision` (`9`), `isolation_and_rollback` (`5`, incl. `panic = "abort"` subprocess check), `leased_schedule_soundness` (`6`: direct `ScheduleLeased` rejection on load/unload/reload/cascade, `unload_all_mods` skip-leased preservation, `ModCommandsExt` deferral from `FixedUpdate`/`Last` to `ModLifecycleFlush`, and `mount_mod_systems_in_world` skip-leased guard), `mod_dag_and_cascade` (`3`), `schema_evolution` (`2`), `scoped_cleanup_and_slab` (`4`: `2,600+` slot cycles & `Allow<Disabled>` cleanup), `signature_verification` (`11`, gated behind `--features signature-verification`), `state_hooks_and_observers` (`3`).
4. **[`e2e`](tests/e2e/main.rs) (`12` tests):** End-to-end tests compiling/loading asymmetric Rust and ISO C99 shared libraries ([`tests/asymmetric_c_mod`](../../tests/asymmetric_c_mod), [`tests/asymmetric_rust_mod`](../../tests/asymmetric_rust_mod), or `BAM_FIXTURE_DIR`).
5. **[`mod_budgets`](tests/mod_budgets.rs) (`9` tests):** `CommandArena` `CAPACITY_EXCEEDED`, cross-system per-frame spawn caps, `p99_us` auto-quarantine, `ModBudgetPolicy` override precedence, `ModBudgetTelemetry`, and `diagnostics` metric publishing.
6. **[`schedule_shuffle_fuzz`](tests/schedule_shuffle_fuzz.rs) (`2` tests):** Compiles 5 `#![forbid(unsafe_code)]` Rust guest `.cdylib` mods (`SEQUENCE`, `SEQUENCE_WEAK`, `PARALLEL`, relations, observers, reduction) and runs `256` `ScheduleBuildSettings::shuffle_seed` permutations under `LogLevel::Error` asserting bit-exact world hashes.
7. **[`soak_10k_reloads`](tests/soak_10k_reloads.rs) (`1` test):** Runs `250` cycles by default for fast CI/unit runs, or `10,000` live reload cycles when `BAM_SOAK_CYCLES=10000` is set (combining zero-copy `FastPod`, 500 bidirectional `v1 <-> v2` `migrate_component_column` flips, `DROP_PAGES` recycling past `4,096` slots, `save_state`/`restore_state`, `< 2.0 ms` release p99 latency assertion, and `< 32 MiB` steady-state RSS drift).

---

## 4. Benchmark Suites (`[[bench]]`, `harness = false`)

Both benchmark binaries share a zero-dependency Criterion-compatible CLI parser accepting `--bench` (default full run: `5` warmup / `30` rounds in `chunk_throughput`; `5` warmup / `30` rounds in `v03_subsystem_bench`), `--quick` (`3` warmup / `12` rounds), `--test` (fast CI smoke mode scaling entity counts to `<= 256` with minimal warmup/rounds), and an optional positional substring filter:

### 4.1 `chunk_throughput` ([`benches/chunk_throughput.rs`](benches/chunk_throughput.rs))

Uses `#![expect(clippy::cast_ptr_alignment)]` for host-aligned FFI column pointer casts across **9 benchmark groups**:
1. `dense_iterate` (`1k`, `10k`, `100k` entities across Native Bevy, `GuestSystem`, Rust `.cdylib`, C99 `.dylib`/`.so`)
2. `changed_filter` (`100k` entities at `1%`, `50%`, `100%` mutation rates)
3. `command_burst` (`10k` handle-resolved `host_cmd_insert` writes/frame)
4. `sparse_query` (`10k` `SparseSet` batched row iteration)
5. `spawn_with_4` (single-archetype `host_cmd_spawn` vs `spawn_empty` + `4x insert`)
6. `query_filters` (`Allow<Disabled>`, `Has<T>`, `Or<...>`)
7. `batch_commands` (`host_cmd_spawn_batch`, `host_cmd_despawn_all`)
8. `summary_tick` (`BAM_COMPONENT_FLAG_SUMMARY_TICK` at `0%`/`1%`/`100%` dirty across `100` tables)
9. `entity_children` (`host_sys_entity_children` vs Bevy `Query<&Children>`)

### 4.2 `v03_subsystem_bench` ([`benches/v03_subsystem_bench.rs`](benches/v03_subsystem_bench.rs))

Measures all **8 ABI `0.3.0` subsystems**:
1. `relations` (`10k` `cmd_insert_relation` / `cmd_remove_relation` edges + `CASCADE_DELETE` 10k chain)
2. `intern_symbol` (`10k` unique strings + `100k` repeated lookups)
3. `iter_count` (`host_iter_count` vs full chunk scan at `10k` / `100k`)
4. `checkpoint_rollback` (`10k` commands + `cmd_checkpoint` / `cmd_rollback`)
5. `migrate_column` (`migrate_component_column` `8B -> 16B` across `10k` entities)
6. `par_readonly` (`par_for_each_chunk_readonly` vs sequential at `100k`)
7. `up_cascade` (`iter_up_cascade` on depth-64 hierarchy × `1k` leaves)
8. `schedule_set_rewire` (`configure_schedule_set` + `mount_system_in_schedule` + `rewire_mod_schedule_sets_in_world`)

### Benchmark Invocation Examples

```bash
# Fast CI smoke checks (all groups, scaled <= 256 items)
cargo bench -p bamabi_host --bench chunk_throughput -- --test
cargo bench -p bamabi_host --bench v03_subsystem_bench -- --test

# Quick local release timing run (or full --bench default)
cargo bench -p bamabi_host --bench v03_subsystem_bench -- --quick
cargo bench -p bamabi_host --bench chunk_throughput -- --bench

# Substring-filtered benchmark group
cargo bench -p bamabi_host --bench v03_subsystem_bench -- --quick relations
cargo bench -p bamabi_host --bench chunk_throughput -- --quick dense_iterate
```
