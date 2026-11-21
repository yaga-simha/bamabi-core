# Testing, Formal Concurrency & Benchmarking (`bamabi-core`)

`bamabi-core` (Layer 2) enforces an 8-tier verification pyramid locally and in [`.github/workflows/ci.yml`](.github/workflows/ci.yml) / [`.github/workflows/nightly.yml`](.github/workflows/nightly.yml).

---

## 1. Quick Commands

```bash
# 1. Static checks: fmt, clippy, rustdoc, headless tree check, typos, cargo-deny
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
typos
cargo deny --all-features check advisories licenses bans sources

# 2. Full Rust test suite (default, signature-verification, all-features, doctests)
cargo test --workspace --all-features
cargo test --doc --all-features

# 3. Loom concurrency models, Miri UB checks, benchmarks, and coverage (>= 85%)
cargo test -p bamabi_host_sys --test loom_drop_trampoline --release
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p bamabi_host_sys --lib
cargo bench -p bamabi_host --bench chunk_throughput -- --test
cargo bench -p bamabi_host --bench v03_subsystem_bench -- --test
cargo llvm-cov --no-report --all-features -p bamabi_host_sys -p bamabi_host -p bamabi_watcher
cargo llvm-cov report --ignore-filename-regex '(crates/bamabi_watcher/|(tests|examples|xtask|benches)/|\.cargo/)' --fail-under-lines 85
```

---

## 2. Verification Pyramid

### Tier 1: Formatting, Isolation & Static Lints

1. **`cargo fmt --all -- --check`** — enforces Rust `2024` formatting across all workspace crates, unit suites, integration suites, and benchmarks.
2. **`#![forbid(unsafe_code)]` audit:** Verifies `#![forbid(unsafe_code)]` is present in [`crates/bamabi_host/src/lib.rs`](crates/bamabi_host/src/lib.rs) and [`crates/bamabi_watcher/src/lib.rs`](crates/bamabi_watcher/src/lib.rs).
3. **Strict headless isolation check (`bamabi_host`):** Both commands MUST produce zero output:
   ```bash
   cargo tree -p bamabi_host --edges normal --all-features | grep -E 'bevy_render|bevy_reflect|bevy_scene|bevy_asset|\bnotify\b|bevy_winit|bevy_window|wgpu|glutin'
   cargo tree -p bamabi_host --all-features | grep -E 'bevy_render|bevy_reflect|bevy_scene|bevy_asset|\bnotify\b|bevy_winit|bevy_window|wgpu|glutin'
   ```
   CI also checks `Cargo.lock` for forbidden heavy Bevy crates and audits against `&mut ResMut<` anti-patterns in `bamabi_host/src` and `bamabi_watcher/src`.
4. **Clippy, Source Audits & Rustdoc across feature matrices:**
   CI also checks `crates/*/src` for zero `#[allow]` / `#![allow]` attributes and zero non-test `.unwrap()` / `.expect()` / `panic!` / `unreachable!` / `todo!` calls:
   ```bash
   cargo clippy --workspace --all-targets -- -D warnings
   cargo clippy -p bamabi_host_sys -p bamabi_host --features signature-verification --all-targets -- -D warnings
   cargo clippy -p bamabi_host_sys --features bridge-reflect --all-targets -- -D warnings
   cargo clippy --workspace --all-targets --all-features -- -D warnings
   RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
   RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
   ```

### Tier 2: Complete Test Inventory per Crate (`439` total tests with `--all-features`)

No tests in `bamabi-core` carry `#[ignore]`; optional tests gate on `--features signature-verification` or `--features diagnostics`, and [`soak_10k_reloads`](crates/bamabi_host/tests/soak_10k_reloads.rs) scales its iteration count via `BAM_SOAK_CYCLES` (`250` default, `10000` in CI Linux `--release`).

#### `bamabi_host_sys` (`149` tests — [`crates/bamabi_host_sys/TESTS.md`](crates/bamabi_host_sys/TESTS.md))

| Suite / Binary | Command | Tests | Scope |
|---|---|---|---|
| **Unit (`--lib`)** (`27` files in `tests/unit/`) | `cargo test -p bamabi_host_sys --lib --all-features` | `102` | `drop_trampoline`, `ffi/*` (incl. `validation` reflection pool), `registry/*`, `schedule`, `system/*`, `loader/*` (incl. `shared_lib` `proptest` fuzzers & `signing` parity) |
| `ffi_v03_coverage` | `cargo test -p bamabi_host_sys --test ffi_v03_coverage --all-features` | `7` | Exhaustive ABI `0.3.0`, `assets_scene`, `commands*`, `iter*`, `observers_ext`, `hierarchy_ext`, `system/*` edge paths |
| `host_sys_integration_tests` | `cargo test -p bamabi_host_sys --test host_sys_integration_tests --all-features` | `10` | World-scoped `last_error` & symbols, handle poisoning, `WorldDropGuard`, `SparseSet`, `thiserror` chains |
| `loom_drop_trampoline` | `cargo test -p bamabi_host_sys --test loom_drop_trampoline --release` | `3` | Exhaustive atomic interleaving proofs for `DROP_PAGES` (incl. 3rd ABA generation-reuse model) |
| `signature_loader_security_tests` | `cargo test -p bamabi_host_sys --test signature_loader_security_tests --features signature-verification` | `3` | `0o700` non-symlink shadow dir, oversized `.sig`, zero leaked shadows on `dlopen`/init failure |
| `v03_cascade_par` | `cargo test -p bamabi_host_sys --test v03_cascade_par` | `4` | 50k-entity `CascadeHierarchyCache`, `iter_up_cascade`, zero-arena `par_for_each_chunk_readonly` |
| `v03_migration_symbols` | `cargo test -p bamabi_host_sys --test v03_migration_symbols` | `6` | 100k zero-copy reload & `8B->16B` migration, `required_init_descs`, `BamSymbolTable`, `ModNamedObjects` |
| `v03_relations` | `cargo test -p bamabi_host_sys --test v03_relations` | `7` | `EXCLUSIVE`, `SYMMETRIC`, `ACYCLIC`, `CASCADE_DELETE`, `CLEANUP_REMOVE`, checkpoints |
| `v03_schedules_error_bridge` | `cargo test -p bamabi_host_sys --test v03_schedules_error_bridge` | `6` | Decoupled schedule sets (`.chain()`/`.chain_weak()`), `<400us` hot-rewire, `FallbackErrorHandler` |
| `zero_alloc_commands` | `cargo test -p bamabi_host_sys --test zero_alloc_commands` | `1` | `CountingAllocator` proof of zero steady-state heap allocations in `CommandArena` + `cmd_spawn` |

#### `bamabi_host` (`268` tests + `2` benches — [`crates/bamabi_host/TESTS.md`](crates/bamabi_host/TESTS.md))

| Suite / Binary | Command | Tests | Scope |
|---|---|---|---|
| **Unit (`--lib`)** (`8` files in `tests/unit/`) | `cargo test -p bamabi_host --lib --all-features` | `15` | `app_ext`, `budgets`, `commands_ext` (`ModLifecycleFlush` deferral), `mod_dag` (`ModDagErrorKind` + 13 accessors), `plugin`, `schedule_sets`, `world_ext` |
| `ffi` (`tests/ffi/main.rs`) | `cargo test -p bamabi_host --test ffi --all-features` | `121` | `10` submodules: FFI boundaries, chunk/commands, filters (`Allow`/`Has`/`Or`), C99 VTable, `SparseSet`, UUID parity |
| `scheduling` (`tests/scheduling/main.rs`) | `cargo test -p bamabi_host --test scheduling --all-features` | `47` | `6` submodules: parallel conflicts, `proptest` Bevy differential parity, multi-query, ordering sets, `diagnostics` |
| `lifecycle` (`tests/lifecycle/main.rs`) | `cargo test -p bamabi_host --test lifecycle --all-features` | `59` | `11` submodules: `leased_schedule_soundness`, `drop_trampolines`, hot-reload, rollback, `mod_dag`, `schema_evolution`, `signature_verification` (`11`, feature-gated) |
| `e2e` (`tests/e2e/main.rs`) | `cargo test -p bamabi_host --test e2e --all-features` | `12` | Asymmetric Rust & ISO C99 shared-library end-to-end execution |
| `mod_budgets` | `cargo test -p bamabi_host --test mod_budgets --all-features` | `9` | Arena caps, cross-system per-frame spawn caps, `p99_us` auto-quarantine, telemetry & diagnostics |
| `schedule_shuffle_fuzz` | `cargo test -p bamabi_host --test schedule_shuffle_fuzz --all-features` | `2` | 5-mod DAG across `256` Bevy `ScheduleBuildSettings::shuffle_seed` permutations under `LogLevel::Error` |
| `soak_10k_reloads` | `BAM_SOAK_CYCLES=10000 cargo test -p bamabi_host --test soak_10k_reloads --release` | `1` | `250` default cycles (`10,000` with `BAM_SOAK_CYCLES=10000`): zero-copy + 500 migrations + `DROP_PAGES` recycling + `< 2.0 ms` p99 / `< 32 MiB` RSS drift |
| **Doctests** | `cargo test --doc -p bamabi_host --all-features` | `2` | `src/lib.rs` (`no_run`) + `src/app_ext.rs` (`compile_fail` proving `ResMut<ModHost>` cannot unload) |

#### `bamabi_watcher` (`22` tests — [`crates/bamabi_watcher/TESTS.md`](crates/bamabi_watcher/TESTS.md))

| Suite / Binary | Command | Tests | Scope |
|---|---|---|---|
| **Unit (`--lib`)** (`tests/unit/watcher_tests.rs`) | `cargo test -p bamabi_watcher --lib --all-features` | `13` | Mock `mpsc` drain, `TryRecvError::Disconnected` once-semantics, `ScheduleLeased` deferral, atomic-rename grace window, `sync_fingerprints`, poisoned mutex recovery |
| `hot_reload` (`tests/hot_reload.rs`) | `cargo test -p bamabi_watcher --test hot_reload --all-features` | `9` | `ModLifecycleFlush` polling, synchronous `with_sync_on_start`, `ScheduleLeased` DAG retry, cascading reload/rollback, non-recursive & delete guards |

### Tier 3: Excluded Asymmetric Rust & C99/C11 Guest Fixtures

Two standalone excluded guest crates (`tests/asymmetric_rust_mod`, `tests/fixtures/panic_abort_mod`) and the ISO C99 fixture (`tests/asymmetric_c_mod`) test true cross-toolchain, cross-optimization, and `panic = "abort"` boundaries:

```bash
cargo check --manifest-path tests/asymmetric_rust_mod/Cargo.toml
cargo check --manifest-path tests/fixtures/panic_abort_mod/Cargo.toml
make -C tests/asymmetric_c_mod
```

CI (`asymmetry-rust`, `asymmetry-c`, `sanitizers`) builds guests across Rust `1.95.0` / `stable` / `nightly`, `-O0`/`-O3`, `unwind`/`abort`, `gcc`/`clang`/`cl.exe`, and C99 UBSan (`-fsanitize=undefined -fno-sanitize-recover=all`), passing `BAM_FIXTURE_DIR` to host suites.

### Tier 4: Property-Based Fuzzing & Nightly Deep Verification

- **Checked binary-parser `proptest` fuzzing** (`crates/bamabi_host_sys/tests/unit/shared_lib_tests.rs`): 4 property tests asserting panic-free determinism, prefix-truncation monotonicity, and strictly bounded RSDS/checksum writes across ELF32/ELF64, Mach-O32/64/Fat, and PE32/PE32+:
  ```bash
  PROPTEST_CASES=2048 cargo test -p bamabi_host_sys --lib shared_lib::tests::proptest_shared_lib_parsers
  ```
- **Bevy scheduler differential `proptest`** (`crates/bamabi_host/tests/scheduling/differential_parity.rs`): verifies access compatibility, self-conflicts, iteration order, and change-tick parity against native Bevy systems.
- **Nightly CI ([`.github/workflows/nightly.yml`](.github/workflows/nightly.yml)):** Runs ThreadSanitizer (`-Zsanitizer=thread -Zbuild-std` on `scheduling` & `lifecycle`), Big-Endian Miri (`s390x-unknown-linux-gnu`), and `RUSTFLAGS="-Zrandomize-layout"` with `PROPTEST_CASES=2048`.

### Tier 5: Loom Concurrency & Miri Soundness

- **Loom (`DROP_PAGES` 3-model verification):**
  ```bash
  cargo test -p bamabi_host_sys --test loom_drop_trampoline --release
  ```
  Covers: (1) concurrent publish vs. drop vs. `WorldDropGuard::drop`, (2) two concurrent drops vs. `WorldDropGuard::drop`, and (3) the 3rd **ABA generation-reuse model** (`loom_slot_reuse_aba_stale_reader_never_invokes_new_generation_drop`).
- **Miri (`-Zmiri-tree-borrows`):**
  ```bash
  MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p bamabi_host_sys --lib
  MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p bamabi_host_sys --lib --features bridge-reflect
  ```
  `cfg!(miri)` adaptations automatically serialize parallel strata/chunks and bound symbol-table pages / `proptest` case counts (`4` cases under `cfg!(miri)`) so Miri validates `HandleHeader`, `UnsafeCell` snapshots, `OwningPtr` / `MaybeUninit` moves, `schema_migration`, and `BamFieldReflectSlot` cleanly.

### Tier 6: Benchmarks (`chunk_throughput` & `v03_subsystem_bench`)

`crates/bamabi_host` ships two zero-dependency Criterion-compatible `[[bench]]` harnesses (`harness = false`):

- **`chunk_throughput`** (`crates/bamabi_host/benches/chunk_throughput.rs`, `#![expect(clippy::cast_ptr_alignment)]`): 9 core ECS/FFI throughput suites (`dense_iterate`, `changed_filter`, `command_burst`, `sparse_query`, `spawn_with_4`, `query_filters`, `batch_commands`, `summary_tick`, `entity_children`).
- **`v03_subsystem_bench`** (`crates/bamabi_host/benches/v03_subsystem_bench.rs`): 8 ABI `0.3.0` subsystem suites (`relations`, `intern_symbol`, `iter_count`, `checkpoint_rollback`, `migrate_column`, `par_readonly`, `up_cascade`, `schedule_set_rewire`).

Supported CLI arguments for both binaries:
- `--bench` (default): full entity scales (`1k`/`10k`/`100k`), `5` warmup rounds, `30` measured rounds.
- `--quick`: full entity scales, `3` warmup rounds, `12` measured rounds.
- `--test`: CI smoke mode scaling item counts to `<= 256` (`1` warmup / `2` rounds in `chunk_throughput`; `0` warmup / `1` round in `v03_subsystem_bench`).
- `<substring>`: positional filter (any non-`-` argument) matching benchmark IDs (e.g., `relations`, `dense_iterate`).

```bash
cargo bench -p bamabi_host --bench chunk_throughput -- --test
cargo bench -p bamabi_host --bench v03_subsystem_bench -- --test
cargo bench -p bamabi_host --bench v03_subsystem_bench -- --quick relations
```

### Tier 7: Line Coverage Gate (`cargo llvm-cov >= 85%`)

All prior `bamabi_host_sys` and `bamabi_host` exclusions have been removed from the CI gate. Run:

```bash
cargo llvm-cov --no-report --all-features -p bamabi_host_sys -p bamabi_host -p bamabi_watcher
cargo llvm-cov report \
  --ignore-filename-regex '(crates/bamabi_watcher/|(tests|examples|xtask|benches)/|\.cargo/)' \
  --fail-under-lines 85
```

Post-hardening coverage measurements:
- **CI gate (`--ignore-filename-regex '(crates/bamabi_watcher/|(tests|examples|xtask|benches)/|\.cargo/)'`):** **`93.36%`** (`19,371 / 20,748` lines covered; every source file in `bamabi_host_sys/src/` and `bamabi_host/src/` is `>= 85%`, ranging from `85.37%` in `shared_lib.rs` up to `100.00%`).
- **All measured files (no crate exclusions):** **`92.65%`** (`20,094 / 21,689` lines covered).
- **Why `crates/bamabi_watcher/` remains excluded:** `crates/bamabi_watcher/src/watcher.rs` is at **`76.38%`** (`721 / 944` lines) due to OS `notify` / filesystem watcher event-loop branches (`inotify`/`FSEvents`/`ReadDirectoryChanges` platform error paths).

### Tier 8: Spellcheck & Supply-Chain Audit

```bash
typos
cargo deny --all-features check advisories licenses bans sources
```

---

## 3. Per-Crate Test Catalogs

- [`crates/bamabi_host_sys/TESTS.md`](crates/bamabi_host_sys/TESTS.md) — `102` unit tests, `9` integration binaries (incl. 3 Loom ABA models & 4 parser proptests), and Miri coverage.
- [`crates/bamabi_host/TESTS.md`](crates/bamabi_host/TESTS.md) — `15` unit tests, `7` `[[test]]` suites (incl. `leased_schedule_soundness`, `schedule_shuffle_fuzz`, `soak_10k_reloads`), `2` doctests, and `chunk_throughput` / `v03_subsystem_bench`.
- [`crates/bamabi_watcher/TESTS.md`](crates/bamabi_watcher/TESTS.md) — `13` unit tests, `9` `hot_reload` integration tests, and CI coverage exclusion details.
