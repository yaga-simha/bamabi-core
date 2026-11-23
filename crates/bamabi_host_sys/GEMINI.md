# Agent Guide: `bamabi_host_sys`

Crate guide for **`bamabi_host_sys`** (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1+`) — the low-level host FFI execution engine, CoW registry, chunk/sparse iterators, command arena, and dynamic loader.

Documentation index: [`README.md`](README.md) · [`ARCHITECTURE.md`](ARCHITECTURE.md) · [`TESTS.md`](TESTS.md).

---

## 1. Crate Identity
- **Name:** `bamabi_host_sys` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Isolates ALL host-side `unsafe` FFI and Bevy ECS raw storage access behind documented safety contracts so `bamabi_host`, `bamabi_watcher`, companion crates, and `bamabi` enforce `#![forbid(unsafe_code)]`.

---

## 2. Safety & Dependency Boundaries
- **`unsafe` Policy:** Enforces `unsafe_op_in_unsafe_fn = "deny"`, `improper_ctypes_definitions = "deny"`, `#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]`. Every `unsafe` block MUST have a `// SAFETY:` comment; every `unsafe fn` MUST have `# Safety` docs. Keep inline `tests/unit/*_tests.rs` cleanly separated via `#[path = ...]`.
- **Allowed Dependencies:** `bamabi_sys`, `bevy_ecs`, `bevy_app`, `bevy_platform`, `bevy_tasks`, `libloading`, `nonmax`, `arc-swap`, `thiserror`, `tracing`, plus strictly optional `bamabi_trust` (`signature-verification`) and `bevy_reflect` + `bevy_ecs/bevy_reflect` (`bridge-reflect`). **NEVER** add `bevy_render`, `bevy_scene`, `bevy_asset`, or `notify`.

---

## 3. Owned Surface & Feature Flags
- **Features:** `strict-ffi-checks`, `test-utils`, `diagnostics`, `signature-verification`, `keygen` (implies `signature-verification` + `bamabi_trust/keygen`), `bridge-reflect`.
- **73/73 `HOST_API` (`ffi/`):** Sealed `pub(crate)` FFI submodules (`host_api`, `iter`, `iter_ext`, `commands`, `commands_ext`, `register`, `relations`, `hierarchy_ext`, `observers_ext`, `assets_scene`, `proc_address`, `v03_ext`, `validation`), consolidated `handle_header.rs` (`AlignedBuf`), `ffi_try!`, `HostIter` (`iter_next_chunk_batch`, `iter_column_ticks`, `BAM_TERM_FLAG_{CHANGED,ADDED}`) + `CascadeHierarchyCache` + zero-arena `par_for_each_chunk_readonly`, `cmd_clone_entity`/`cmd_insert_resource`/`cmd_remove_resource`, `16`-query `param_set_group` & `run_if_ctx`, and panic/expect-free `bridge-reflect` `BamFieldReflectSlot` pool (`UninitializedBamFieldReflectSlot` fallback in `ffi/validation.rs`).
- **Registry & Drop Safety (`registry/`, `drop_trampoline.rs`, `sync_ptr.rs`):** Generation-tracked `Arc<RegistrySnapshot>` `ModRegistry`, `ComponentOptions::with_summary_tick()` (auto-inheriting `T::HAS_SUMMARY_TICK` in `ComponentOptions::<T>::new()`), `ModRelationGraph` (`allows_self_referential()`, opt-in `__bam_rel_bind` self-referential hook), `BamSymbolTable`, `ModNamedObjects`, `DROP_PAGES` (`ArcSwapOption` wait-free drop trampolines) & `WorldDropGuard`.
- **Scheduling, Commands & Loader (`system/`, `schedule.rs`, `loader/`):** `GuestSystem` with exact `FilteredAccessSet<ComponentId>`, `ModScheduleSetRegistry` + `ModScheduleSetRewirePass` (`ScheduleSetConfig`, Bevy `0.20` `.chain_weak()`) with leased-schedule skip guards (`mount_mod_systems_in_world`, `rewire_mod_schedule_sets_in_world`, `ensure_mod_schedule_set_pass_if_schedules_present`), observer & system fault routing through Bevy `0.20.1` `FallbackErrorHandler` (`ErrorContext::Observer` / `ErrorContext::System`), `CommandArena` (`cmd_checkpoint`/`cmd_rollback`), `ModHost` shadow-copy loading, checked-arithmetic ELF/Mach-O/PE parsers (`shared_lib.rs`), `LoadedMod::registered_schedule_labels()`, `ModLoadError::ScheduleLeased` / `unload_all_mods_in_world` skip-leased guards, transactional rollback, `despawn_all_where`, and `migrate_component_column`.
- **Trust Parity (`loader/signing.rs`):** With `signature-verification`, exposes full non-`keygen` `bamabi_trust` parity (`add_krl_signer_openssh`, `add_krl_signer_file`, `with_krl_signer_principal`, `verify_bampack_bytes`, `verify_bampack_file`, 16 `Deref` getters, `VerifiedModSignature` `&` conversions, and `bampack`/`embedded_sig`/`ssh_key`/`LineEnding`/`PublicKey`/`PrivateKey`/`Certificate`/`Algorithm`/`Fingerprint`/`HashAlg`/`BamPackExtract*` re-exports) without enabling `ssh-key/getrandom`. Enabling opt-in `keygen` (`["signature-verification", "bamabi_trust/keygen"]`) additionally exposes `keygen`, `GeneratedKeypair`, `IssuedCertificate`, `generate_ed25519_keypair`, `issue_ed25519_certificate_openssh`, and `write_openssh_keypair`.

---

## 4. Strict Engineering Rules
1. **ABI & Allocator Isolation:** Host allocations are freed only by the host; guest allocations only by the guest. Never change `BamHostApi` (`73` slots, `592` B 64-bit / `300` B 32-bit) or `ABI 0.3.0` wire layouts.
2. **Zero Non-Test Panics & No `#[allow]`:** Maintain zero `.unwrap()` / `.expect()` / `panic!` / `unreachable!` / `todo!` and zero `#[allow(...)]` suppressions in `src/`.
3. **Miri, Proptest & Loom Parity:** Any change to `drop_trampoline`, `sync_ptr`, `handle_header`, `ffi/*`, `loader/shared_lib.rs`, or `loader/schema_migration.rs` MUST pass Miri (`-Zmiri-tree-borrows`), parser `proptest` fuzzing (`shared_lib_tests.rs`), and all 3 models in `loom_drop_trampoline` (including `loom_slot_reuse_aba_stale_reader_never_invokes_new_generation_drop`).

---

## 5. Standalone Verification Commands

```bash
cargo fmt --all -- --check
cargo clippy -p bamabi_host_sys --all-targets --all-features -- -D warnings
cargo clippy -p bamabi_host_sys --all-targets --no-default-features -- -D warnings
BAM_FIXTURE_DIR="/path/to/fixtures" cargo test -p bamabi_host_sys --all-features
cargo test -p bamabi_host_sys --test loom_drop_trampoline --release
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p bamabi_host_sys --lib
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p bamabi_host_sys --lib --features bridge-reflect
RUSTDOCFLAGS="-D warnings" cargo doc -p bamabi_host_sys --no-deps --all-features
typos
```

---

## 6. Workspace Coordination
1. **Standalone `bamabi-core` Member:** `bamabi_host_sys` is a Layer 2 workspace member whose shared dependencies and lints live in `../../Cargo.toml` (pointing to Layer 0 [`bamabi-wire`](https://github.com/yaga-simha/bamabi-wire) and Layer 1 [`bamabi-tools`](https://github.com/yaga-simha/bamabi-tools) via GitHub git URLs, with optional local sibling overrides via `bamabi-dev-overlay/overlay.py`).
2. **Self-Contained Test Fixtures:** Integration tests (`host_sys_integration_tests`, `signature_loader_security_tests`, etc.) resolve compiled guest fixtures from `BAM_FIXTURE_DIR` if set, and otherwise build C99 and Rust fixtures automatically into `target/fixtures/` using `bamabi_sdk_gen`, vendored `tests/asymmetric_c_mod/include/generated/testbed_components.h`, and `https://github.com/yaga-simha/bamabi-wire.git` (gating sibling `../bamabi-wire` behind explicit `BAM_WIRE_DIR` / `BAM_INCLUDE_DIR` / `BAM_USE_SIBLING_WIRE` env vars or an active `../.cargo/config.toml` `bamabi-wire` overlay).
3. **Downstream Consumers:** Consumed by `bamabi_host` and `bamabi_watcher` in `bamabi-core`, and Layer 3 (`bamabi`, `bamabi_bridges`, `bamabi_render`, `bamabi_reflect`).
