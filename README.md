# `bamabi-core`

Layer 2 (**slim headless Bevy `0.20.1` host runtime** + hot-reload watcher) for the federated **`bamabi`** (`ABI 0.3.0`, `v0.3.0`) native modding framework.

Refactored from the original [`bevy_mod_abi`](https://github.com/yaga-simha/bevy_mod_abi) monorepo into a strictly acyclic 5-repository DAG plus [`bamabi-dev-overlay`](https://github.com/yaga-simha/bamabi-dev-overlay), `bamabi-core` depends on Layer 0 ([`bamabi-wire`](https://github.com/yaga-simha/bamabi-wire)) and Layer 1 ([`bamabi-tools`](https://github.com/yaga-simha/bamabi-tools)) via GitHub `git` URLs (`branch = "main"`), providing the complete headless ECS modding host (`bevy_ecs` + `bevy_app` only — **zero** `bevy_render`, `bevy_reflect`, `bevy_scene`, or `bevy_asset` dependencies in `bamabi_host`) for dedicated servers, headless simulations, and higher-level game hosts.

## Workspace Crates

| Crate | Safety Policy | Bevy Deps | Responsibilities | Docs |
|---|---|---|---|---|
| [`bamabi_host_sys`](crates/bamabi_host_sys/README.md) | Host `unsafe` (sealed `pub(crate)` `ffi/`) | `bevy_ecs`, `bevy_app`, `bevy_tasks`, opt. `bevy_reflect` | 68/68 live `HOST_API` slots, `ModRegistry` (CoW `Arc<RegistrySnapshot>`), `GuestSystem`, `HostIter`, `CascadeHierarchyCache`, `CommandArena`, `ModRelationGraph`, `ModScheduleSetRegistry`, `DROP_PAGES`, `ModHost`, `LoadedMod::registered_schedule_labels`, checked ELF/Mach-O/PE parsers, zero-copy reload, `migrate_component_column`, optional `signature-verification` (`bamabi_trust` non-`keygen` parity) | [`ARCHITECTURE`](crates/bamabi_host_sys/ARCHITECTURE.md) · [`TESTS`](crates/bamabi_host_sys/TESTS.md) |
| [`bamabi_host`](crates/bamabi_host/README.md) | `#![forbid(unsafe_code)]` | `bevy_ecs`, `bevy_app`, opt. `bevy_diagnostic` (**headless**) | `AbiModPlugin`, `ModLifecycleFlush` (`ensure_mod_lifecycle_flush_schedule`), `ModBudgetPolicy` / `ModBudgetTelemetry`, `ModAppExt`, `ModWorldExt` (`ScheduleLeased` guards, skip-leased `unload_all_mods`), `ModCommandsExt` (safe-point deferral), `ModVisibleSets`, `ModUnloadHooks`, `ModQuarantine`, `mod.toml` SemVer DAG loading (`ModDagErrorKind` + 13 accessors) | [`ARCHITECTURE`](crates/bamabi_host/ARCHITECTURE.md) · [`TESTS`](crates/bamabi_host/TESTS.md) |
| [`bamabi_watcher`](crates/bamabi_watcher/README.md) | `#![forbid(unsafe_code)]` | `bevy_ecs`, `bevy_app` + `notify-debouncer-full` | Debounced recursive filesystem hot-reload watcher (`ModHotReloadPlugin`, `ModHotReloadWatcher`, `ModHotReloadHistory`) polling in `ModLifecycleFlush`, synchronous `with_sync_on_start` initial load, transient `ScheduleLeased` retries, disconnect handling, and cascading SemVer DAG reloads | [`ARCHITECTURE`](crates/bamabi_watcher/ARCHITECTURE.md) · [`TESTS`](crates/bamabi_watcher/TESTS.md) |

## Repository Documentation

- **[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)** — Full federated architecture guide, safety invariants, execution lifecycle, `ModLifecycleFlush` / `ScheduleLeased` contract, `DROP_PAGES` reclamation state machine, trust model, and per-repo verification workflow.
- **[`TESTS.md`](TESTS.md)** — Complete verification pyramid, test inventory per crate, `chunk_throughput` & `v03_subsystem_bench` benchmarks, Loom/Miri/proptest checks, and `cargo llvm-cov` (`>= 85%`) coverage status.
- **[`GEMINI.md`](GEMINI.md)** — Workspace contributor and AI-agent engineering guide.
- **[`CHANGELOG.md`](CHANGELOG.md)** — Release and `[Unreleased]` change history.

## Standalone Usage

`bamabi-core` builds and runs completely standalone with zero sibling checkouts required.

Most Bevy game applications depend on Layer 3 ([`bamabi`](https://github.com/yaga-simha/bamabi)), which re-exports `bamabi_host` and `bamabi_watcher` alongside companion bridges, rendering, and reflection crates. Headless servers or simulations that only need the slim ECS runtime can depend directly on `bamabi-core`:

```toml
[dependencies]
bamabi_host = { git = "https://github.com/yaga-simha/bamabi-core.git", branch = "main" }
bamabi_watcher = { git = "https://github.com/yaga-simha/bamabi-core.git", branch = "main" }

# Optional Ed25519 load-time signature verification & diagnostics:
# bamabi_host = { git = "https://github.com/yaga-simha/bamabi-core.git", branch = "main", features = ["diagnostics", "signature-verification"] }
```

> **Note (`keygen`):** `bamabi_trust::keygen` items remain upstream-gated because `bamabi_trust/keygen` (`ssh-key/getrandom`) is not enabled by `signature-verification` in [`crates/bamabi_host_sys/Cargo.toml`](crates/bamabi_host_sys/Cargo.toml). Use `cargo-bam keygen` (`bamabi-tools`) or depend directly on `bamabi_trust` with `features = ["keygen"]` for key generation.

## Local & CI Verification

All checks run standalone inside `bamabi-core` with zero sibling repositories required (see [`TESTS.md`](TESTS.md) for detailed test/bench catalogs):

```bash
# Formatting, #![forbid(unsafe_code)], clippy (default / sig-verif / all-features), and rustdoc
cargo fmt --all -- --check
grep -q '#!\[forbid(unsafe_code)\]' crates/bamabi_host/src/lib.rs
grep -q '#!\[forbid(unsafe_code)\]' crates/bamabi_watcher/src/lib.rs
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p bamabi_host_sys -p bamabi_host --features signature-verification --all-targets -- -D warnings
cargo clippy -p bamabi_host_sys --features bridge-reflect --all-targets -- -D warnings
cargo clippy --workspace --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features

# Verify strict headless isolation for bamabi_host (must print nothing)
cargo tree -p bamabi_host --edges normal --all-features | grep -E 'bevy_render|bevy_reflect|bevy_scene|bevy_asset|\bnotify\b|bevy_winit|bevy_window|wgpu|glutin'
cargo tree -p bamabi_host --all-features | grep -E 'bevy_render|bevy_reflect|bevy_scene|bevy_asset|\bnotify\b|bevy_winit|bevy_window|wgpu|glutin'

# Unit, integration, signature-verification, doctest, and excluded asymmetric fixture checks
cargo test --workspace
cargo test -p bamabi_host_sys -p bamabi_host --features signature-verification
cargo test --workspace --all-features
cargo test --doc --all-features
cargo check --manifest-path tests/asymmetric_rust_mod/Cargo.toml
cargo check --manifest-path tests/fixtures/panic_abort_mod/Cargo.toml

# C99 asymmetric mod + layout verification (auto-stages target/include/bamabi.h if needed)
make -C tests/asymmetric_c_mod

# Concurrency (3 Loom models), 10k soak, benchmark smoke gates, coverage (>= 85%), and Miri
cargo test -p bamabi_host_sys --test loom_drop_trampoline --release
BAM_SOAK_CYCLES=10000 cargo test -p bamabi_host --test soak_10k_reloads --release
cargo bench -p bamabi_host --bench chunk_throughput -- --test
cargo bench -p bamabi_host --bench v03_subsystem_bench -- --test
cargo llvm-cov --no-report --all-features -p bamabi_host_sys -p bamabi_host -p bamabi_watcher
cargo llvm-cov report --ignore-filename-regex '(crates/bamabi_watcher/|(tests|examples|xtask|benches)/|\.cargo/)' --fail-under-lines 85
MIRIFLAGS="-Zmiri-tree-borrows" cargo +nightly miri test -p bamabi_host_sys --lib

# Spellcheck and dependency/license/source audit
typos
cargo deny --all-features check advisories licenses bans sources
```

> **Environment Overrides (`BAM_FIXTURE_DIR`, `BAM_WIRE_DIR`, `BAM_INCLUDE_DIR`):** By default in standalone checkouts, tests render `bamabi.h` via `bamabi_sdk_gen`, use the vendored `tests/asymmetric_c_mod/include/generated/testbed_components.h`, and build temporary Rust guest crates against `https://github.com/yaga-simha/bamabi-wire.git`. Set `BAM_FIXTURE_DIR` to point to prebuilt guest shared libraries (used by `asymmetry-rust`, `sanitizers`, and nightly TSan/randomize-layout jobs), or `BAM_WIRE_DIR` / `BAM_INCLUDE_DIR` to test against a local Layer 0 checkout.

## Federated `bamabi` Repositories

| Layer | Repository | Role |
|---|---|---|
| **0** | [`bamabi-wire`](https://github.com/yaga-simha/bamabi-wire) | Zero-Bevy ABI wire contract (`spec.md`), derives, `include/bamabi.h`, and Rust guest SDK |
| **1** | [`bamabi-tools`](https://github.com/yaga-simha/bamabi-tools) | Zero-Bevy Ed25519 trust (`bamabi_trust`), manifest DAG (`bamabi_manifest`), and `cargo-bam` CLI |
| **2** | **[`bamabi-core`](https://github.com/yaga-simha/bamabi-core)** *(this repo)* | Headless `bevy_ecs` + `bevy_app` host runtime, hot-reload watcher, and `docs/ARCHITECTURE.md` |
| **3** | [`bamabi`](https://github.com/yaga-simha/bamabi) | Bevy `0.20.1` companion bridges/render/reflect crates and sole canonical `bamabi` host facade |
| **4** | [`bamabi-showcase`](https://github.com/yaga-simha/bamabi-showcase) | Top-level integration repo: runnable examples (`01`–`10`), benchmarks (`docs/BENCHMARKS.md`), fuzzing, mdBook guide (`docs/book/`), and `xtask` |
| **Overlay** | [`bamabi-dev-overlay`](https://github.com/yaga-simha/bamabi-dev-overlay) | Multi-repo `abi-lock.toml` / standalone-git consistency verifier (`sync_check.py`) and local sibling `[patch]` overlay (`overlay.py`) |

## Multi-Repo Local Development (`bamabi-dev-overlay`)

When iterating on `bamabi-core` alongside sibling checkouts (`bamabi-wire`, `bamabi-tools`, `bamabi`, `bamabi-showcase`) under a common parent directory, use [`bamabi-dev-overlay`](https://github.com/yaga-simha/bamabi-dev-overlay) instead of editing tracked `Cargo.toml` dependencies:

```bash
# Verify ABI lockfiles, versions, licenses, and strict GitHub dependency hygiene across all repos
python3 ../bamabi-dev-overlay/sync_check.py --strict-git

# Temporarily patch upstream github.com/yaga-simha/*.git deps to local sibling paths
python3 ../bamabi-dev-overlay/overlay.py enable
python3 ../bamabi-dev-overlay/overlay.py status --strict

# Disable the local overlay before committing or testing standalone builds (Cargo.lock hygiene)
python3 ../bamabi-dev-overlay/overlay.py disable
```

> **Note (`Cargo.lock` hygiene):** Never commit `Cargo.lock`, `tests/asymmetric_rust_mod/Cargo.lock`, or `tests/fixtures/panic_abort_mod/Cargo.lock` changes generated while `[patch]` is enabled; tracked lockfiles must retain `git+https://github.com/yaga-simha/bamabi-{wire,tools}.git?branch=main#...` sources for standalone CI.

## License

Dual-licensed under `MIT OR Apache-2.0` ([`LICENSE-MIT`](LICENSE-MIT) and [`LICENSE-APACHE`](LICENSE-APACHE)).
