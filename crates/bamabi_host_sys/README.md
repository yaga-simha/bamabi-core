# `bamabi_host_sys`

Low-level host FFI execution engine, registry, and dynamic library loader for `bamabi` (`ABI 0.3.0`, `v0.3.0`, Layer 2 in [`bamabi-core`](../../README.md)).

## Documentation Map

- **Crate Architecture & Implementation:** [`ARCHITECTURE.md`](ARCHITECTURE.md) — module map, safety boundaries, 68/68 `HOST_API` execution, `DROP_PAGES` reclamation, leased-schedule guards, checked binary parsers, `signature-verification` parity, and `ffi/validation.rs` reflection pool.
- **Test & Verification Guide:** [`TESTS.md`](TESTS.md) — complete inventory of unit tests (`102` tests across `27` unit suites), `9` integration binaries (including the `3` Loom `DROP_PAGES` models and parser `proptest` fuzzing), Miri checks, and coverage status.
- **Workspace Guides:** [`../../docs/ARCHITECTURE.md`](../../docs/ARCHITECTURE.md) · [`../../TESTS.md`](../../TESTS.md) · [`GEMINI.md`](GEMINI.md)

## Overview

Isolates all host-side `unsafe` code behind sealed `pub(crate)` `ffi/` submodules and documented safety invariants (`#![deny(improper_ctypes_definitions, clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]`) so [`bamabi_host`](../bamabi_host/README.md) and all companion/facade crates enforce `#![forbid(unsafe_code)]`:

- **`HOST_API` (`BamHostApi`)**: All **68/68** live `extern "C"` function-pointer slots implemented end-to-end (including all 18 ABI `0.3.0` slots in [`v03_ext`](src/ffi/v03_ext.rs) and asset/scene trait backends in [`assets_scene`](src/ffi/assets_scene.rs)).
- **`ModRegistry`**: Generation-tracked copy-on-write (`Arc<RegistrySnapshot>`) handle, UUID, `"bam.v0.*"` symbol, relation, declarative `BamFieldDesc`, and VTable registry.
- **`GuestSystem`, `ModScheduleSetRegistry` & `ModScheduleSetRewirePass`**: Implements Bevy's `System` trait over `BamSystemDesc`, populates `FilteredAccessSet<ComponentId>`, supports Bevy `0.20` `.chain()` / `.chain_weak()` / `.before_weak()` / `.after_weak()` decoupled schedule sets, schedule-only hot-rewiring, `NON_SEND` main-thread pinning, leased-schedule skip guards, and Bevy `0.20.1` `FallbackErrorHandler` fault bridging for both systems (`ErrorContext::System`) and observers (`ErrorContext::Observer`).
- **`HostIter`, `CascadeHierarchyCache` & Zero-Arena Parallelism**: Zero-copy `Table` column slices, batched `SparseSet` indirect row pointers, non-destructive `iter_count`, lazy per-`(World, RelationComponentId)` `CascadeHierarchyCache` (`iter_up_cascade` with Brent cycle guard), resource change-detection ticks (`BamChangeTicks`), `par_for_each_chunk`, and zero-arena `par_for_each_chunk_readonly`.
- **`HostCommands`, `CommandArena` & `ModRelationGraph`**: Per-system bump-allocated deferred command queue with bounded arena/spawn quota hooks, LIFO `cmd_checkpoint` / `cmd_rollback` transactions, typed entity relationships (`EXCLUSIVE`, `SYMMETRIC`, `ACYCLIC` / `CYCLE_DETECTED`, `CASCADE_DELETE`, `CLEANUP_REMOVE`, plus opt-in self-referential edges on non-acyclic bindings), and `EntityWorldMut` application (`ComponentHooks`, `#[require(...)]`, `ChildOf` / `Children`).
- **`DROP_PAGES`, `WorldDropGuard`, & `DynamicComponentHookMap`**: Wait-free `ArcSwapOption` paged drop trampolines (up to `4,096` slots) with two-condition reclamation plus 5-hook lifecycle, `clone_fn`, and `required_init_descs` dispatch preventing use-after-`dlclose`.
- **`validation`** ([`src/ffi/validation.rs`](src/ffi/validation.rs)): Safe host-side witness field introspection/migration, declarative `BamFieldDesc` `ReflectFromPtr` / `ReflectComponent` / `ReflectResource` slot pools (`slot_core` / `slot_type_info` with panic-free `UninitializedBamFieldReflectSlot` fallback), and cross-`World` (`SubApp`) component/resource extraction.
- **`ModHost`, Zero-Copy Reload, `migrate_component_column`, `BamSymbolTable` & `ModNamedObjects`**: Shadow-copy library loading (`LoadOptions`), checked-arithmetic ELF/Mach-O/PE residency and CodeView `.pdb` parsers, `LoadedMod::registered_schedule_labels()`, `ModLoadError::ScheduleLeased` / `unload_all_mods_in_world` skip-leased guards, transactional init rollback, deterministic 5-step unload using `despawn_all_where` (`Allow<Disabled>`), zero-copy `schema_fingerprint` column retention, zero-per-entity-allocation `migrate_component_column`, monotonic reload-stable UTF-8 symbol interning, and persistent named objects.
- **Optional `signature-verification`** ([`src/loader/signing.rs`](src/loader/signing.rs)): Full non-`keygen` parity with `bamabi_trust` (`.sig`, `.bam_sig`, `.bampack`, CA/SSHKRL), adding `ModTrustPolicy` delegates (`add_krl_signer_openssh`, `add_krl_signer_file`, `with_krl_signer_principal`, `verify_bampack_bytes`, `verify_bampack_file`), all 16 `Deref` getters, `VerifiedModSignature` owned/borrowed conversions, and `bampack` / `embedded_sig` / `ssh_key` re-exports. Note that `bamabi_trust::keygen` items remain upstream-gated because `bamabi_trust/keygen` (`ssh-key/getrandom`) is not enabled by `signature-verification` in [`Cargo.toml`](Cargo.toml).

Application authors should depend on [`bamabi`](https://github.com/yaga-simha/bamabi) (or [`bamabi_host`](../bamabi_host/README.md) for headless ECS hosts) rather than using this `-sys` crate directly.

## Standalone Usage

```toml
[dependencies]
bamabi_host_sys = { git = "https://github.com/yaga-simha/bamabi-core.git", branch = "main" }

# Optional pre-load Ed25519 signature & .bampack verification:
# bamabi_host_sys = { git = "https://github.com/yaga-simha/bamabi-core.git", branch = "main", features = ["signature-verification"] }
```

## Verification

See [`TESTS.md`](TESTS.md) for the complete test, Loom, Miri, and coverage breakdown.

```bash
cargo test -p bamabi_host_sys --all-features
cargo test -p bamabi_host_sys --test loom_drop_trampoline --release
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p bamabi_host_sys --lib
cargo clippy -p bamabi_host_sys --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p bamabi_host_sys --no-deps --all-features
```

## License

Dual-licensed under `MIT OR Apache-2.0` ([`LICENSE-MIT`](../../LICENSE-MIT) and [`LICENSE-APACHE`](../../LICENSE-APACHE)).
