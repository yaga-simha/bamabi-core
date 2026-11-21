# Workspace Agent & Contributor Guide (`bamabi-core`)

`bamabi-core` is **Layer 2** (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1`, Rust `2024`, MSRV `1.95`) of the federated **`bamabi`** native modding framework: the **slim headless ECS host runtime + hot-reload watcher**.

---

## 1. Crate Topology & Per-Crate Guides

| Crate | Policy | Role | Crate Docs |
|---|---|---|---|
| `bamabi_host_sys` | Host `unsafe` isolated behind sealed `pub(crate)` `ffi/` | 68/68 `HOST_API` slots, `GuestSystem`, `HostIter`, `CommandArena`, `DROP_PAGES`, `ModRegistry`, `ModHost`, `signing` | [`README`](crates/bamabi_host_sys/README.md) · [`ARCHITECTURE`](crates/bamabi_host_sys/ARCHITECTURE.md) · [`TESTS`](crates/bamabi_host_sys/TESTS.md) · [`GEMINI`](crates/bamabi_host_sys/GEMINI.md) |
| `bamabi_host` | **`#![forbid(unsafe_code)]`** (strictly headless) | `AbiModPlugin`, `ModLifecycleFlush`, `ModAppExt`, `ModWorldExt`, `ModCommandsExt`, `ModBudgetPolicy`, `ModDagErrorKind` | [`README`](crates/bamabi_host/README.md) · [`ARCHITECTURE`](crates/bamabi_host/ARCHITECTURE.md) · [`TESTS`](crates/bamabi_host/TESTS.md) · [`GEMINI`](crates/bamabi_host/GEMINI.md) |
| `bamabi_watcher` | **`#![forbid(unsafe_code)]`** (`notify` isolated here) | `ModHotReloadPlugin`, `ModHotReloadWatcher`, `ModHotReloadHistory` | [`README`](crates/bamabi_watcher/README.md) · [`ARCHITECTURE`](crates/bamabi_watcher/ARCHITECTURE.md) · [`TESTS`](crates/bamabi_watcher/TESTS.md) · [`GEMINI`](crates/bamabi_watcher/GEMINI.md) |

---

## 2. Non-Negotiable Engineering Invariants

1. **Safety Boundary:** `crates/bamabi_host/src/lib.rs` and `crates/bamabi_watcher/src/lib.rs` MUST keep `#![forbid(unsafe_code)]`. Every `unsafe` block in `bamabi_host_sys` MUST carry `// SAFETY:`; every `unsafe fn` MUST document `# Safety`.
2. **Zero `#[allow]` & Zero Non-Test Panics in `src/`:** Never add `#[allow(...)]` in `crates/*/src/`. Never call `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, or `todo!()` outside `#[cfg(test)]`, `tests/`, or `benches/`.
3. **Strict Headless Isolation in `bamabi_host`:** `bamabi_host` MUST gain zero `bevy_render`, `bevy_reflect`, `bevy_scene`, `bevy_asset`, `notify`, `bevy_winit`, `bevy_window`, `wgpu`, or `glutin` dependencies (direct or transitive).
4. **Rustdoc & Spelling Cleanliness:** All crates enable `#![warn(missing_docs)]`; `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features` and `typos` MUST pass cleanly.

---

## 3. Critical Runtime Contracts

- **`ModLifecycleFlush` & Leased-Schedule Rule (`ScheduleLeased`):**
  - `ensure_mod_lifecycle_flush_schedule(app)` inserts `ModLifecycleFlush` after `Last` in `MainScheduleOrder`.
  - `ModCommandsExt` operations defer into `PendingModLifecycleOps` unless `leased ⊆ {ModLifecycleFlush, Main}` and `(ModLifecycleFlush ∈ leased || Main ∉ leased)`.
  - Direct `ModWorldExt` loads/unloads/reloads targeting a leased schedule return `ModLoadError::ScheduleLeased { id, schedule }`; `unload_all_mods()` warns and skips leased mods so `ModHost` may remain non-empty.
- **`bamabi_watcher` Lifecycle:**
  - Polls in `ModLifecycleFlush` (never `PreUpdate`); `with_sync_on_start(true)` runs `sync_dir` synchronously in `Plugin::build` so `Startup` systems schedule cleanly.
  - Transient `ScheduleLeased` errors defer into `pending_retries` without recording `Failed`; `TryRecvError::Disconnected` logs once (`tracing::error!`) and emits one `ModHotReloadAction::Failed` outcome.
- **Trust Parity & `keygen` Gate:**
  - `bamabi_host_sys::loader::signing` exposes full non-`keygen` `bamabi_trust` parity (`add_krl_signer_openssh`, `add_krl_signer_file`, `with_krl_signer_principal`, `verify_bampack_bytes`, `verify_bampack_file`, 16 `Deref` getters, and `bampack`/`embedded_sig`/`ssh_key` re-exports).
  - `signature-verification` alone continues to omit `bamabi_trust/keygen` (`ssh-key/getrandom`), while the opt-in `keygen` feature (`bamabi_host_sys/keygen`, `bamabi_host/keygen` — implying `signature-verification`) exposes `keygen`, `GeneratedKeypair`, `IssuedCertificate`, `generate_ed25519_keypair`, `issue_ed25519_certificate_openssh`, and `write_openssh_keypair`.
- **`ModDagErrorKind` & Accessors:**
  - `ModDagErrorKind::as_str()` and all 13 `ModDagError` accessors (`kind`, `path`, `related_paths`, `mod_name`, `dependency`, `required_version`, `found_version`, `cycle_members`, `load_error`, `mod_id`, `is_cycle`, `is_io`, `is_recoverable`) provide allocation-light structured DAG diagnostics.

---

## 4. Key Verification Commands

See [`TESTS.md`](TESTS.md) for the complete test, Loom, Miri, benchmark, and coverage reference.

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo test --workspace --all-features
cargo bench -p bamabi_host --bench chunk_throughput -- --test
cargo bench -p bamabi_host --bench v03_subsystem_bench -- --test
cargo test -p bamabi_host_sys --test loom_drop_trampoline --release
cargo llvm-cov --no-report --all-features -p bamabi_host_sys -p bamabi_host -p bamabi_watcher
cargo llvm-cov report --ignore-filename-regex '(tests|examples|xtask|benches)/|\.cargo/' --fail-under-lines 85
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p bamabi_host_sys --lib
typos
```

---

## 5. Federated Hygiene

Keep tracked `Cargo.toml` and `Cargo.lock` pointed at GitHub `git` URLs (`branch = "main"`). When testing local sibling edits across `bamabi-wire` / `bamabi-tools`, toggle overrides via `../bamabi-dev-overlay/overlay.py` and always run `overlay.py disable` before committing.
