# `bamabi_watcher`

Safe (`#![forbid(unsafe_code)]`) debounced filesystem hot-reload watcher for `bamabi` (`ABI 0.3.0`, `v0.3.0`, Layer 2 in [`bamabi-core`](../../README.md)).

## Overview

- **`ModHotReloadPlugin` / `ModHotReloadWatcher`:** Recursively monitors a mods directory for native shared libraries (`.so`, `.dylib`, `.dll`) and `mod.toml` manifests via `notify-debouncer-full` (pairing seamlessly with `cargo bam watch --out-dir`), validates SemVer dependency DAG constraints, and executes cascading reverse/forward topological hot-reloads and unloads in Bevy `PreUpdate`.
- **`ModHotReloadHistory` / `ModHotReloadOutcome` / `ModHotReloadAction`:** Records chronological load, reload, unload, and validation failure outcomes as a Bevy resource.

## Standalone Usage

```toml
[dependencies]
bamabi_watcher = { git = "https://github.com/yaga-simha/bamabi-core.git", branch = "main" }
```

## Verification

```bash
cargo test -p bamabi_watcher --all-features
cargo clippy -p bamabi_watcher --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p bamabi_watcher --no-deps --all-features
```

## License

Dual-licensed under `MIT OR Apache-2.0` ([`LICENSE-MIT`](../../LICENSE-MIT) and [`LICENSE-APACHE`](../../LICENSE-APACHE)).
