//! ECS lifecycle components and buffered `Message`s for native mods.
//!
//! Lifecycle and fault notifications (`ModLoaded`, `ModUnloaded`, `ModSystemFaulted`)
//! emit once via Bevy [`Message`] queues (`MessageReader<M>`) without
//! duplicate `Observer`/`Event` double-writes.

#[cfg(feature = "signature-verification")]
pub use bamabi_host_sys::VerifiedModSignature;
pub use bamabi_host_sys::{
    LoadedModInfo, ModId, ModLoaded, ModSystemError, ModSystemFaulted, ModUnloaded, SystemInitError,
};
use bevy_ecs::message::Message;
use std::path::PathBuf;

/// Buffered Bevy [`Message`] emitted whenever loading or hot-reloading a native mod fails.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ModLoadFailed {
    /// Source filesystem path or virtual mod label that failed to load.
    pub path: PathBuf,
    /// Existing [`ModId`] being reloaded when failure occurred during hot-reload (`None` on initial load).
    pub reload_target: Option<ModId>,
    /// Human-readable diagnostic error message.
    pub error: String,
}
