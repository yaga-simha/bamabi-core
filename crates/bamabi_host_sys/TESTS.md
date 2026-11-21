# `bamabi_host_sys` Test & Verification Guide

This document inventories every unit test suite, integration test binary, Loom concurrency model, property-based fuzzer, and Miri/coverage check in `bamabi_host_sys`.

---

## 1. Suite Overview & Commands

| Layer | Location | Count | Command |
|---|---|---|---|
| **Unit tests (`--lib`)** | `27` `#[path = ...]` files in [`tests/unit/`](tests/unit/) | `102` tests | `cargo test -p bamabi_host_sys --lib --all-features` |
| **ABI 0.3.0 / FFI coverage** | [`tests/ffi_v03_coverage.rs`](tests/ffi_v03_coverage.rs) | `7` tests | `cargo test -p bamabi_host_sys --test ffi_v03_coverage --all-features` |
| **Core host_sys integration** | [`tests/host_sys_integration_tests.rs`](tests/host_sys_integration_tests.rs) | `10` tests | `cargo test -p bamabi_host_sys --test host_sys_integration_tests --all-features` |
| **Loom concurrency models** | [`tests/loom_drop_trampoline.rs`](tests/loom_drop_trampoline.rs) | `3` models | `cargo test -p bamabi_host_sys --test loom_drop_trampoline --release` |
| **Signature & OS security** | [`tests/signature_loader_security_tests.rs`](tests/signature_loader_security_tests.rs) | `3` tests (`signature-verification`) | `cargo test -p bamabi_host_sys --test signature_loader_security_tests --features signature-verification` |
| **Cascade & parallel RO** | [`tests/v03_cascade_par.rs`](tests/v03_cascade_par.rs) | `4` tests | `cargo test -p bamabi_host_sys --test v03_cascade_par` |
| **Migration, symbols & req** | [`tests/v03_migration_symbols.rs`](tests/v03_migration_symbols.rs) | `6` tests | `cargo test -p bamabi_host_sys --test v03_migration_symbols` |
| **Typed entity relations** | [`tests/v03_relations.rs`](tests/v03_relations.rs) | `7` tests | `cargo test -p bamabi_host_sys --test v03_relations` |
| **Schedule sets & error bridge** | [`tests/v03_schedules_error_bridge.rs`](tests/v03_schedules_error_bridge.rs) | `6` tests | `cargo test -p bamabi_host_sys --test v03_schedules_error_bridge` |
| **Zero-alloc commands** | [`tests/zero_alloc_commands.rs`](tests/zero_alloc_commands.rs) | `1` test | `cargo test -p bamabi_host_sys --test zero_alloc_commands` |
| **Doctests** | `src/` | `0` tests | `cargo test --doc -p bamabi_host_sys --all-features` |
| **Miri UB checks** | `--lib` (default & `bridge-reflect`) | `102` unit tests | `MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p bamabi_host_sys --lib` |

> **Total with `--all-features`:** `102` unit tests + `47` integration tests = `149` tests (`0` `#[ignore]`). Under `cfg!(miri)`, tests in `iter.rs`, `hierarchy_ext.rs`, `symbols.rs`, and `shared_lib_tests.rs` automatically adjust thread pools, page limits, and `proptest` case counts so Miri executes cleanly without OS-thread or timeout failures.

---

## 2. Unit Test Suites (`tests/unit/`, `102` tests)

Every source module links its unit tests cleanly via `#[cfg(test)] #[path = "..."]`:

| Source Module | Unit Test File | Tests | What It Verifies |
|---|---|---|---|
| `src/drop_trampoline.rs` | [`drop_trampoline_tests.rs`](tests/unit/drop_trampoline_tests.rs) | `2` | `5,000`-UUID slab recycling with generation bumps; persistent-value deferred reclamation until `World::drop`. |
| `src/ffi/assets_scene.rs` | [`assets_scene_tests.rs`](tests/unit/assets_scene_tests.rs) | `1` | Null/alignment/poisoned-handle/`NONE`/`INVALID_ENTITY`/`UNSUPPORTED` guards and mock `HostAssetBackend`/`HostSceneBackend` paths. |
| `src/ffi/commands.rs` | [`commands_tests.rs`](tests/unit/commands_tests.rs) | `3` | Command quota arena/spawn caps + rewind, FIFO `queue_custom`, stale drop-handle rejection on recycled slots. |
| `src/ffi/commands_ext.rs` | [`commands_ext_tests.rs`](tests/unit/commands_ext_tests.rs) | `1` | Extended guards and execution for `spawn_persistent`, `spawn_batch`, `despawn_all`, `insert_if_new`. |
| `src/ffi/hierarchy_ext.rs` | [`hierarchy_ext_tests.rs`](tests/unit/hierarchy_ext_tests.rs) | `11` | `CascadeHierarchyCache` strata reuse/invalidation, Brent cycle guards, `iter_up_cascade`, `sys_entity_children`, `FfiChildOf`, custom `RelationshipTarget`. |
| `src/ffi/iter_ext.rs` | [`iter_ext_tests.rs`](tests/unit/iter_ext_tests.rs) | `2` | `iter_read_removed` guards/batching; `par_for_each_chunk_readonly` dense/sparse permissions & error propagation. |
| `src/ffi/observers_ext.rs` | [`observers_ext_tests.rs`](tests/unit/observers_ext_tests.rs) | `6` | `ADD`/`INSERT`/`DISCARD`/`REMOVE`/`DESPAWN`/`READY` observers, mutable & propagating observers, cycle guard, `FallbackErrorHandler` (`ErrorContext::Observer`). |
| `src/ffi/register.rs` | [`register_tests.rs`](tests/unit/register_tests.rs) | `10` | 5 lifecycle hooks, `clone_fn`, `required_init_descs`, `SUMMARY_TICK` validation, recycled drop slots, opt-in self-referential relations. |
| `src/ffi/relations.rs` | [`relations_tests.rs`](tests/unit/relations_tests.rs) | `7` | `EXCLUSIVE`/`SYMMETRIC`/`ACYCLIC`/`CASCADE_DELETE`/`CLEANUP_REMOVE`, `RelationFlushPolicy`, self-referential non-acyclic bindings, checkpoint/rollback. |
| `src/ffi/v03_ext.rs` | [`v03_ext_tests.rs`](tests/unit/v03_ext_tests.rs) | `5` | `log_diagnostic`, UUID/layout introspection, `iter_count`, `register_system_handle`, `cmd_checkpoint`/`rollback`, null/bounds/size guards. |
| `src/ffi/validation.rs` | [`validation_tests.rs`](tests/unit/validation_tests.rs), [`validation_coverage_tests.rs`](tests/unit/validation_coverage_tests.rs) | `4` + `5` | `BamFieldDesc` validation, `ReflectFromPtr`/`ReflectComponent`/`ReflectResource` under Miri, panic-free uninitialized slot fallback, cross-world extraction, witness export/apply. |
| `src/loader/error.rs` | [`loader_error_coverage_tests.rs`](tests/unit/loader_error_coverage_tests.rs) | `3` | ABI SemVer compatibility matrix and `ModLoadError` `Display`/`Error::source` formatting (including `ScheduleLeased`). |
| `src/loader/mod_host.rs` | [`mod_host_tests.rs`](tests/unit/mod_host_tests.rs) | `1` | `excise_and_unload_mod_in_world` cleans mod-owned `IsResource` entities while preserving host resources. |
| `src/loader/schema_migration.rs` | [`schema_migration_tests.rs`](tests/unit/schema_migration_tests.rs), [`schema_migration_coverage_tests.rs`](tests/unit/schema_migration_coverage_tests.rs) | `3` + `2` | `migrate_component_column` (`8B -> 16B`, ZST, `SparseSet`, rollback), zero-copy fingerprint retention, dynamic resources & errors under Miri. |
| `src/loader/shared_lib.rs` | [`shared_lib_tests.rs`](tests/unit/shared_lib_tests.rs) | `8` | ELF64/ELF32/Mach-O/PE32/PE32+ residency & `.pdb` `RSDS` patching, oversized/truncated regression, and **4 `proptest` parser fuzzers** proving panic-free determinism and bounded writes. |
| `src/loader/signing.rs` | [`signing_tests.rs`](tests/unit/signing_tests.rs) | `9` | `signature-verification` only: `ModTrustPolicy` delegates (`add_krl_signer_*`, `with_krl_signer_principal`, `verify_bampack_*`), `Deref` getters, `VerifiedModSignature` `&`/owned round-trips, `bampack`/`embedded_sig`/`ssh_key` parity. |
| `src/loader/symbols.rs` | [`symbols_tests.rs`](tests/unit/symbols_tests.rs) | `4` | Sharded lock-free `BamSymbolTable` deduplication and `ModNamedObjects` reload persistence + `World` cleanup. |
| `src/loader/transaction.rs` | [`transaction_tests.rs`](tests/unit/transaction_tests.rs) | `1` | `LoadTransaction::drop` reclaims fresh slots across `1,100` failed inits in one live `World`. |
| `src/registry/bridge.rs` | [`bridge_coverage_tests.rs`](tests/unit/bridge_coverage_tests.rs) | `1` | `HostWireBridge`, `HostStringBridge`, resource/event/relationship shims and error paths. |
| `src/registry/catalog.rs` | [`catalog_coverage_tests.rs`](tests/unit/catalog_coverage_tests.rs) | `2` | `TargetEntity`, `ComponentOptions`, snapshot eviction/restore, collision errors, message/event/hierarchy edges. |
| `src/schedule.rs` | [`schedule_coverage_tests.rs`](tests/unit/schedule_coverage_tests.rs) | `3` | `ModScheduleMap`, `ModVisibleSets`, `ModScheduleSetRewirePass`, and leased-schedule skip guards. |
| `src/system/access_builder.rs` | [`access_builder_tests.rs`](tests/unit/access_builder_tests.rs) | `3` | `FilteredAccessSet` conflict diagnostics and persistent message/removed cursors across reloads. |
| `src/system/error_policy.rs` | [`error_policy_tests.rs`](tests/unit/error_policy_tests.rs) | `2` | `ModErrorPolicy`, `ModQuarantine`, `ActiveGuestFaultGuard`, `ModBudgetRuntime` delegation. |
| `src/system/guest_system.rs` | [`guest_system_tests.rs`](tests/unit/guest_system_tests.rs) | `2` | Summary-tick skip accounting, watchdog quarantine, FFI guards, random entity access, `Or<...>` groups. |
| `src/system/query_group.rs` | [`query_group_tests.rs`](tests/unit/query_group_tests.rs) | `1` | `CachedQueryGroup` drop guards and empty archetype updates. |

---

## 3. Integration & Loom Concurrency Suites (`47` tests across `9` binaries)

1. **[`ffi_v03_coverage.rs`](tests/ffi_v03_coverage.rs) (`7` tests):** Exhaustive positive and adversarial coverage of ABI `0.3.0` slots, `assets_scene`, `commands`/`commands_ext`, `hierarchy_ext`, `iter`/`iter_ext`, `observers_ext`/`register`, and `system/*`.
2. **[`host_sys_integration_tests.rs`](tests/host_sys_integration_tests.rs) (`10` tests):** World-isolated `last_error` & symbols, `HOST_API` slots, handle header poisoning, `WorldDropGuard` capacity & rollback, `SparseSet` batching & `untracked` ticks, `thiserror` source chains.
3. **[`loom_drop_trampoline.rs`](tests/loom_drop_trampoline.rs) (`3` exhaustive Loom models):**
   - `loom_concurrent_publish_drop_and_world_close_reclaims_exactly_once` — hot-reload snapshot swap vs. concurrent `drop_trampoline` vs. `WorldDropGuard::drop`.
   - `loom_two_concurrent_value_drops_vs_world_close_never_double_reclaims` — two concurrent table value drops racing `WorldDropGuard::drop`.
   - `loom_slot_reuse_aba_stale_reader_never_invokes_new_generation_drop` — 3rd **ABA generation-reuse model** proving a stale generation-1 `drop_trampoline` reader cannot observe or invoke a recycled generation-2 drop function when `DROP_CONTROL.free_list` reassigns the slot.
4. **[`signature_loader_security_tests.rs`](tests/signature_loader_security_tests.rs) (`3` tests, `--features signature-verification`):** Enforces directory-input rejection, empty-binary / oversized-`.sig` / custom-`.sig` bounds, zero leaked shadow files on `dlopen`/init failure, and Unix `0o700` non-symlink/non-world-writable shadow directory enforcement.
5. **[`v03_cascade_par.rs`](tests/v03_cascade_par.rs) (`4` tests):** 50k-entity 6-depth `CascadeHierarchyCache` topological walk, cache reuse/invalidation, `iter_up_cascade`, and zero-arena `par_for_each_chunk_readonly` multicore parity (using a custom `GlobalAlloc` counter).
6. **[`v03_migration_symbols.rs`](tests/v03_migration_symbols.rs) (`6` tests):** 100k-entity zero-copy `schema_fingerprint` retention, 100k-entity zero-per-entity-alloc `migrate_component_column` (`8B -> 16B`), `required_init_descs`, sharded `BamSymbolTable`, and persistent `ModNamedObjects`.
7. **[`v03_relations.rs`](tests/v03_relations.rs) (`7` tests):** `ACYCLIC` DAGs, `SYMMETRIC`, `EXCLUSIVE` reparenting, `CASCADE_DELETE`/`CLEANUP_REMOVE` deep chains & cycles, 16-byte aligned payloads, and scheduled guest execution.
8. **[`v03_schedules_error_bridge.rs`](tests/v03_schedules_error_bridge.rs) (`6` tests):** `configure_schedule_set` / `mount_system_in_schedule` (`.chain()` / `.chain_weak()` / `.before_weak()` / `.after_weak()`), schedule-only hot-rewire `< 400 us`, and `FallbackErrorHandler` / `report_guest_fault`.
9. **[`zero_alloc_commands.rs`](tests/zero_alloc_commands.rs) (`1` test):** `CountingAllocator` proof that steady-state `GuestSystem` + `CommandArena` + batched `cmd_spawn` frames perform zero heap allocations.

---

## 4. Miri, Proptest & Coverage Notes

- **Miri (`-Zmiri-tree-borrows`):** Run both default and `bridge-reflect` features:
  ```bash
  MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p bamabi_host_sys --lib
  MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p bamabi_host_sys --lib --features bridge-reflect
  ```
- **Proptest scaling (`shared_lib_tests.rs`):** Scale parser fuzzing beyond default case counts with `PROPTEST_CASES=2048 cargo test -p bamabi_host_sys --lib shared_lib::tests::proptest_shared_lib_parsers`.
- **Line coverage:** Every source file in `bamabi_host_sys/src/` exceeds the `85%` CI gate (`85.37%`–`100.00%`; zero exclusions remain in `.github/workflows/ci.yml` for `bamabi_host_sys`).
