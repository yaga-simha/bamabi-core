# `bamabi_host_sys` Architecture & Implementation Guide

`bamabi_host_sys` (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1`) is the **low-level host FFI execution engine, CoW registry, chunk/sparse iterator, command arena, and dynamic shared-library loader** in Layer 2 ([`bamabi-core`](../../README.md)).

---

## 1. Crate Role & Safety / Dependency Boundaries

`bamabi_host_sys` enforces a strict single-boundary `unsafe` architecture in [`Cargo.toml`](Cargo.toml) and [`src/lib.rs`](src/lib.rs):

1. **All Host `unsafe` Isolated Here:** Crates above `bamabi_host_sys` (`bamabi_host`, `bamabi_watcher`, and Layer 3 `bamabi`, `bamabi_bridges`, `bamabi_render`, `bamabi_reflect`) enforce `#![forbid(unsafe_code)]`. Within `bamabi_host_sys`, crate lints enforce `unsafe_op_in_unsafe_fn = "deny"`, `improper_ctypes_definitions = "deny"`, `#![warn(missing_docs)]`, and `#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]`.
2. **Sealed `ffi/` Submodules:** [`src/ffi/mod.rs`](src/ffi/mod.rs) marks all 13 FFI submodules `pub(crate)`, re-exporting only the audited crate-root surface in [`src/lib.rs`](src/lib.rs).
3. **Zero Heavy Bevy or File-Watcher Dependencies:** `bamabi_host_sys` depends strictly on `bevy_ecs`, `bevy_app`, `bevy_platform`, `bevy_tasks`, `bamabi_sys`, `libloading`, `nonmax`, `arc-swap`, `thiserror`, `tracing`, plus optional `bamabi_trust` (`signature-verification` / `keygen`) and `bevy_reflect` + `bevy_ecs/bevy_reflect` (`bridge-reflect`). It **never** depends on `bevy_render`, `bevy_scene`, `bevy_asset`, or `notify`.
4. **Zero Non-Test Panics:** Non-test `src/` paths keep `.unwrap()`, `.expect()`, `panic!`, `unreachable!`, `todo!`, and `#[allow(...)]` at zero.

### Feature Flags

| Feature | Dependencies Enabled | Purpose |
|---|---|---|
| `strict-ffi-checks` | — | Enables hot-path [`HandleHeader`] magic/generation checks on `HostIter`, `HostCommands`, and `HostSystemCtx` even in release builds (`cfg!(any(debug_assertions, feature = "strict-ffi-checks"))`). |
| `test-utils` | — | Exposes mock `LoadedMod::register_mock_for_test` helpers for downstream test crates. |
| `diagnostics` | — | Enables `ModDiagnosticsCollector` and `ModDiagnosticsSample` per-system telemetry snapshots. |
| `signature-verification` | `dep:bamabi_trust` | Enables [`loader::signing`] (`ModTrustPolicy`, `VerifiedModSignature`, pre-`dlopen` `.sig` / `.bam_sig` / `.bampack` / `SSHKRL` verification). Does **not** enable `bamabi_trust/keygen` (`ssh-key/getrandom`). |
| `keygen` | `signature-verification`, `bamabi_trust/keygen` | Opt-in feature exposing OpenSSH Ed25519 keypair generation and certificate issuance (`keygen`, `GeneratedKeypair`, `IssuedCertificate`, `generate_ed25519_keypair`, `issue_ed25519_certificate_openssh`, `write_openssh_keypair`). |
| `bridge-reflect` | `dep:bevy_reflect`, `bevy_ecs/bevy_reflect` | Enables declarative `BamFieldDesc` `ReflectFromPtr` / `ReflectComponent` / `ReflectResource` registration via `ffi/validation.rs`. |

---

## 2. Module-by-Module Map

| Module | Path | Contents |
|---|---|---|
| **Crate root** | [`src/lib.rs`](src/lib.rs) | `#![warn(missing_docs)]`, `#![deny(...)]` safety lints, flat public re-exports of audited host primitives and entrypoints. |
| **FFI sub-layer** | [`src/ffi/mod.rs`](src/ffi/mod.rs) | Sealed (`pub(crate)`) host FFI modules implementing all 73 `BamHostApi` slots and bridge helpers. |
| ↳ *Host table* | [`src/ffi/host_api.rs`](src/ffi/host_api.rs) | Static 73-slot `HOST_API: BamHostApi` verified at compile time via `has_all_slots()`. |
| ↳ *Registration* | [`src/ffi/register.rs`](src/ffi/register.rs) | `LoadContext` (`BamLoadCtx`), `host_register_component`, `host_resolve_component`, `host_register_system`, `host_register_observer`, `DynamicComponentHookMap`. |
| ↳ *Iteration* | [`src/ffi/iter.rs`](src/ffi/iter.rs) | `HostIter` (`BamIter`), `QueryPlan`, `QueryTermMeta`, `TickFilterTerm`, dense table slices, `SparseRowScratch`, `Messages<M>`, `host_res_ptr`/`host_res_ticks`, `host_par_for_each_chunk`, `host_iter_next_chunk_batch` (Slot 68), `host_iter_column_ticks` (Slot 69). |
| ↳ *Extended iter* | [`src/ffi/iter_ext.rs`](src/ffi/iter_ext.rs) | `host_iter_read_removed` (`RemovedComponents<T>`) and `par_for_each_chunk_readonly_impl`. |
| ↳ *Commands* | [`src/ffi/commands.rs`](src/ffi/commands.rs) | `HostCommands`, `HostCommandsStorage`, bump-allocated `CommandArena`, `CommandQuotaLimits`, `host_cmd_{spawn_empty,spawn,despawn,insert,insert_default,remove,trigger}`. |
| ↳ *Extended cmds* | [`src/ffi/commands_ext.rs`](src/ffi/commands_ext.rs) | `host_cmd_spawn_persistent`, `host_cmd_spawn_batch`, `host_cmd_despawn_all`, `host_cmd_insert_if_new`, `host_cmd_clone_entity` (Slot 70), `host_cmd_insert_resource` (Slot 71), `host_cmd_remove_resource` (Slot 72). |
| ↳ *Hierarchy* | [`src/ffi/hierarchy_ext.rs`](src/ffi/hierarchy_ext.rs) | `host_sys_entity_children`, `CascadeHierarchyCache`, `host_iter_up_cascade` (Brent cycle guard). |
| ↳ *Observers* | [`src/ffi/observers_ext.rs`](src/ffi/observers_ext.rs) | `ModObserverState`, `guest_observer_runner`, `host_cmd_trigger_ex`, `host_cmd_observe`. |
| ↳ *Relations* | [`src/ffi/relations.rs`](src/ffi/relations.rs) | `ModRelationGraph`, `RelationFlushPolicy`, `RelationFlushError`, `check_relation_acyclic`, lifecycle `on_despawn`/`on_remove` cascade hooks. |
| ↳ *Assets & scenes* | [`src/ffi/assets_scene.rs`](src/ffi/assets_scene.rs) | `HostAssetBackend`, `HostSceneBackend`, `host_asset_{load,get_load_state,release}`, `host_cmd_spawn_scene`. |
| ↳ *Symbol lookup* | [`src/ffi/proc_address.rs`](src/ffi/proc_address.rs) | `host_get_proc_address` (`"bam.v0.*"` world-scoped symbol resolution). |
| ↳ *ABI 0.3 ext* | [`src/ffi/v03_ext.rs`](src/ffi/v03_ext.rs) | Remaining ABI `0.3.0` slots (`log_diagnostic`, `report_guest_fault`, UUID/layout lookup, `register_relation`, `intern_symbol`, `bind_named_object`, `iter_count`, `iter_up_cascade`, `par_for_each_chunk_readonly`, `register_system_handle`, `configure_schedule_set`, `mount_system_in_schedule`, `cmd_checkpoint`/`cmd_rollback`, `migrate_component_column`). |
| ↳ *Validation & reflect* | [`src/ffi/validation.rs`](src/ffi/validation.rs) | Witness / `BamFieldDesc` introspection & live `World` read/write, cross-`World` extraction, and `bridge-reflect` `BamFieldReflectSlot` pool. |
| **Handle headers** | [`src/handle_header.rs`](src/handle_header.rs) | `#[repr(C)]` `HandleHeader { magic, generation }`, `POISONED_MAGIC`, `HostContext` (`UnsafeCell` snapshot for Miri Tree Borrows), `AlignedBuf`, `ffi_try!`. |
| **Drop trampolines** | [`src/drop_trampoline.rs`](src/drop_trampoline.rs) | Lazy 64x64 `DROP_PAGES` (`4,096` slots), `ArcSwapOption` wait-free reads, `WorldDropGuard` two-condition reclamation & generation recycling. |
| **Sync pointers** | [`src/sync_ptr.rs`](src/sync_ptr.rs) | Narrow `Send`/`Sync` wrappers: `SymbolAddress`, `GuestUserData`, `GuestInstancePtr`, `GuestFnPtr<F>`. |
| **Registry** | [`src/registry/`](src/registry/mod.rs) | `ModRegistry` CoW `Arc<RegistrySnapshot>` (`catalog.rs`), safe `HostWireBridge`/`HostStringBridge` adapters (`bridge.rs`), `ComponentCollisionError` (`collision.rs`). |
| **Systems** | [`src/system/`](src/system/mod.rs) | `GuestSystem` (`guest_system.rs`), `FilteredAccessSet` builder (`access_builder.rs`), `ModErrorPolicy` / `ModQuarantine` / `ModBudgetRuntime` / `FallbackErrorHandler` (`error_policy.rs`), `BamQuerySpecOwned` / `PersistentMessageCursors` (`query_group.rs`). |
| **Schedules** | [`src/schedule.rs`](src/schedule.rs) | `ModSystemSet`, `ModNamedSystemSet`, `ModScheduleMap`, `ModVisibleSets`, `StartupCompleted`, `ModScheduleSetRegistry`, `ModScheduleSetRewirePass`, leased-schedule skip guards. |
| **Loader** | [`src/loader/`](src/loader/mod.rs) | `ModHost` & `LoadedMod` (`mod_host.rs`), `ModLoadError` (`error.rs`), `LoadOptions` & checked ELF/Mach-O/PE parsers (`shared_lib.rs`), `LoadTransaction` (`transaction.rs`), `BamSymbolTable` / `ModNamedObjects` (`symbols.rs`), `schema_migration.rs`, and `signing.rs`. |

---

## 3. Core Subsystems & Hardening Details

### 3.1 `HOST_API` (73/73 Slots) & `HandleHeader` Safety

- `HOST_API` in [`src/ffi/host_api.rs`](src/ffi/host_api.rs) populates all `73` `Option<unsafe extern "C" fn(...)>` slots (`27` core `0.2`, `23` extended `0.2.x`, and `23` `0.3.0` slots, including Slots `68..=72`: `iter_next_chunk_batch`, `iter_column_ticks`, `cmd_clone_entity`, `cmd_insert_resource`, `cmd_remove_resource`; `592` B on 64-bit / `300` B on 32-bit), asserted at compile time by `HOST_API.has_all_slots()`.
- System descriptors support up to `16` queries (`MAX_QUERIES_PER_SYSTEM = 16`), `BamQueryDesc::param_set_group` mutual exclusion (`BAM_QUERY_FLAG_PARAM_SET`), `BAM_TERM_FLAG_CHANGED`/`BAM_TERM_FLAG_ADDED`, and `BamSystemDesc::{run_if_ctx, run_if_query, run_if_ctx_user_data}` read-only run-condition evaluation before advancing `last_run`.
- Every opaque host handle (`HostContext`, `LoadContext`, `HostSystemCtx`, `HostIter`, `HostCommands`) begins with `HandleHeader { magic, generation }`. Drop guards overwrite `magic` with `POISONED_MAGIC` (`0xDEAD_BEEF`) and `generation` with `0`.
- `HostContext::snapshot` wraps its `Arc<RegistrySnapshot>` in `UnsafeCell` so updating the snapshot pointer during `bamabi_init` component registration preserves valid Tree Borrows provenance across raw `BamHostCtx*` pointers under Miri (`-Zmiri-tree-borrows`).

### 3.2 `DROP_PAGES` Reclamation & ABA Generation Guard

[`src/drop_trampoline.rs`](src/drop_trampoline.rs) stores foreign `ComponentDropFn` callbacks in lazily allocated pages (`MAX_DROP_PAGES = 64` × `DROP_PAGE_SIZE = 64` = `4,096` slots), avoiding heap cost for empty worlds. Each slot pairs:
- An `ArcSwapOption<DropSlotSnapshot>` holding `(GuestFnPtr<ComponentDropFn>, Arc<AtomicBool>)`, read wait-free on entity drop.
- A single `AtomicUsize` lifecycle word packing `(SLOT_IN_USE_BIT | SLOT_CLOSING_BIT | live_values)` alongside a per-slot `AtomicU64` generation counter.

Because `WorldDropGuard::drop` (`fetch_xor(SLOT_IN_USE_BIT | SLOT_CLOSING_BIT)`) and `drop_trampoline::<SLOT>` (`fetch_sub(1)`) serialize on one atomic word, the last side to observe zero transitions `SLOT_CLOSING -> SLOT_FREE` and increments `generation` before returning the slot to `DROP_CONTROL`'s free list. Exhaustively verified in [`tests/loom_drop_trampoline.rs`](tests/loom_drop_trampoline.rs), including a third ABA model proving a stale generation-1 reader cannot invoke a recycled generation-2 drop callback.

### 3.3 Leased-Schedule Soundness in `loader/mod_host.rs` & `schedule.rs`

To prevent panics or use-after-`dlclose` when mod lifecycle hooks or system mounts run while a Bevy schedule is temporarily leased out of `Schedules` (`World::run_schedule` / `try_schedule_scope`):
- `LoadedMod` records all `(InternedScheduleLabel, ModNamedSystemSet)` pairs attached by the mod and exposes [`LoadedMod::registered_schedule_labels()`](src/loader/mod_host.rs) (`impl Iterator<Item = InternedScheduleLabel> + '_`).
- `excise_and_unload_mod_inner` (backing `excise_and_unload_mod_in_world` and `excise_and_unload_mod_for_reload_in_world`) checks `Schedules::get_temporarily_removed()` against the target mod's `registered_schedule_labels()`. If any target schedule is leased, it returns `Err(ModLoadError::ScheduleLeased { id, schedule })` before disarming `is_active` or unmapping the library.
- `unload_all_mods_in_world` iterates loaded mods in LIFO order and logs a `tracing::warn!` while skipping any mod whose unload returns an error (such as `ScheduleLeased`), leaving unaffected mods unloaded and leased mods safely intact.
- [`src/schedule.rs`](src/schedule.rs) guards `ensure_mod_schedule_set_pass_if_schedules_present`, `mount_mod_systems_in_world`, and `rewire_mod_schedule_sets_in_world`: leased target schedules are skipped with `tracing::warn!` / `tracing::error!` instead of mutating an absent `Schedules` slot.

### 3.4 Checked-Arithmetic Binary Parsers (`loader/shared_lib.rs`)

[`src/loader/shared_lib.rs`](src/loader/shared_lib.rs) provides:
- Shadow-copy staging (`<stem>.<pid>.<seq>.<ext>`) with Windows `.pdb` sidecar cloning and PE CodeView `RSDS` path rewriting (`patch_pe_codeview_pdb_path`, clearing the PE `CheckSum`).
- Dynamic-linker residency pre-checks (`inspect_library_residency` / `inspect_library_residency_file`) inspecting ELF64/ELF32 (`DF_1_NODELETE`, `DF_STATIC_TLS`, `PT_TLS`), Mach-O thin/fat (`__thread_vars` / `S_THREAD_LOCAL_*`, `MH_NO_REEXPORTED_DYLIBS`), and PE32/PE32+ images.
- All offset, stride, section count, and RVA-to-file-offset arithmetic uses `checked_add`, `checked_mul`, `usize::try_from`, and bounded `.get(...)` slicing so malformed or truncated binaries never wrap, panic, or read out of bounds (verified by `proptest` fuzzing in [`tests/unit/shared_lib_tests.rs`](tests/unit/shared_lib_tests.rs)).

### 3.5 Trust API Parity & `keygen` Gate (`loader/signing.rs`)

When `signature-verification` is enabled, [`src/loader/signing.rs`](src/loader/signing.rs) wraps `bamabi_trust::ModTrustPolicy` as a Bevy `Resource` and `VerifiedModSignature` as a Bevy `Component` (with `From` conversions for both owned and `&` references):
- Exposes explicit delegates for every builder and verifier method, including `add_krl_signer_openssh`, `add_krl_signer_file`, `with_krl_signer_principal`, `verify_bampack_bytes`, and `verify_bampack_file`.
- Implements `Deref<Target = bamabi_trust::ModTrustPolicy>` so all 16 read-only getters (`krl_signer_principal`, `trusted_ca_count`, `trusted_public_key_count`, `trusted_krl_signer_count`, `min_krl_version`, `applied_krl_version`, `is_cert_serial_revoked`, `is_cert_key_id_revoked`, `enforcement`, `is_enforced`, `namespace`, `required_principal`, `validity_policy`, `max_binary_bytes`, `binds_manifest_name_to_principal`, `has_trust_anchors`) are reachable directly.
- Re-exports `bampack`, `embedded_sig`, `ssh_key`, `LineEnding`, `Algorithm`, `Certificate`, `Fingerprint`, `HashAlg`, `PrivateKey`, `PublicKey`, `DEFAULT_KRL_SIGNER_PRINCIPAL`, `MAX_BAMPACK_PATH_LEN`, `MAX_BAMPACK_PATH_DEPTH`, `DEFAULT_MAX_BAMPACK_UNCOMPRESSED_BYTES`, `BamPackExtractLimits`, `BamPackExtractError`, `validate_bampack_entry_path`, and `parse_bampack_entries_unverified` at `loader::signing`, `loader`, and crate root.
- **Opt-in `keygen` gate:** `signature-verification` alone intentionally omits `bamabi_trust/keygen` (`ssh-key/getrandom`). Enabling the opt-in `keygen` feature (`["signature-verification", "bamabi_trust/keygen"]` in [`Cargo.toml`](Cargo.toml)) additionally re-exports `keygen`, `GeneratedKeypair`, `IssuedCertificate`, `generate_ed25519_keypair`, `issue_ed25519_certificate_openssh`, and `write_openssh_keypair`.

### 3.6 Panic-Free `bridge-reflect` Slot Pool (`ffi/validation.rs`)

Under `--features bridge-reflect`, [`src/ffi/validation.rs`](src/ffi/validation.rs) provides `register_bam_field_reflect_type` using a fixed pool of `BamFieldReflectSlot<SLOT, SIZE>` ZST/byte-buffer types so dynamic `BamFieldDesc` components gain full `ReflectFromPtr`, `ReflectComponent`, and `ReflectResource` support without witness VTables. All slot metadata lookups use fallible `slot_core` / `slot_type_info` helpers returning `Option` and falling back to an internal `UninitializedBamFieldReflectSlot` `TypeInfo` / `ApplyError::MismatchedTypes` instead of `.expect()` panics.

---

## 4. Setup, Building, Testing & Performance Notes

- **Edition / MSRV:** Rust edition `2024`; workspace `rust-version = "1.95"`.
- **Verification commands:** See [`TESTS.md`](TESTS.md) for the complete per-test inventory, Loom/Miri/proptest commands, and coverage table.
- **Hot-path performance & cursor conservation:** `HostIter` operates directly on Bevy `Table` column pointers (`SummaryTick` table skipping in $O(1)$), `CommandArena` reuses 64-byte-aligned `MaybeUninit<u8>` slabs with zero steady-state heap allocations (verified by [`tests/zero_alloc_commands.rs`](tests/zero_alloc_commands.rs)), and `ModRegistry` publishes wait-free `Arc<RegistrySnapshot>` snapshots (`arc-swap`). Message and `RemovedComponents` batch drains (`msg_read_batch_impl`, `msg_read_batch_non_pod_impl`, and `host_iter_read_removed`) share [`drain_cursor_bounded`](src/registry/catalog.rs) and [`clamp_batch_max_count`](src/registry/catalog.rs) (`isize::MAX / elem_size` bound, ZST-safe) so stateful cursors never advance past the final item written.
