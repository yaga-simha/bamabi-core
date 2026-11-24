#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Safe (`#![forbid(unsafe_code)]`) debounced filesystem hot-reload watcher for `bamabi`
//! (`ABI 0.4.0`).
//!
//! Monitors a mods directory recursively for native shared libraries (`.so`, `.dylib`, `.dll`)
//! and declarative `mod.toml` manifests via `notify-debouncer-full`, validates SemVer DAG
//! constraints, and drives cascading topological reloads and unloads in Bevy
//! [`ModLifecycleFlush`](bamabi_host::ModLifecycleFlush).

pub mod watcher;

pub use watcher::{
    DEFAULT_MAX_HOT_RELOAD_HISTORY, ModHotReloadAction, ModHotReloadHistory, ModHotReloadOutcome,
    ModHotReloadPlugin, ModHotReloadWatcher,
};

/// Convenient prelude re-exporting [`ModHotReloadPlugin`], [`ModHotReloadWatcher`], and outcome types.
pub mod prelude {
    pub use crate::watcher::{
        DEFAULT_MAX_HOT_RELOAD_HISTORY, ModHotReloadAction, ModHotReloadHistory,
        ModHotReloadOutcome, ModHotReloadPlugin, ModHotReloadWatcher,
    };
}
