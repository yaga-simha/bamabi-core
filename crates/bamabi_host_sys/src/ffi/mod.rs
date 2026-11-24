//! `extern "C"` host FFI callbacks, header-validated contexts, and `HOST_API` table.

pub use commands::{
    CommandArenaConfig, host_cmd_despawn_batch, host_cmd_insert_batch, host_cmd_remove_batch,
    host_cmd_spawn_batch_ex,
};
pub use iter::{
    host_iter_column_rows_mutable_untracked, host_iter_mark_changed_ranges, host_msg_read_slice,
    host_par_for_each_chunk_ex,
};
pub use register::{host_register_message, host_register_resource};
pub use relations::{
    host_sys_relation_edge_data, host_sys_relation_sources, host_sys_relation_targets,
};
pub use v03_ext::{host_resolve_named_object, host_resolve_symbol_str};

pub(crate) mod assets_scene;
pub(crate) mod commands;
pub(crate) mod commands_ext;
pub(crate) mod hierarchy_ext;
pub(crate) mod host_api;
pub(crate) mod iter;
pub(crate) mod iter_ext;
pub(crate) mod observers_ext;
pub(crate) mod proc_address;
pub(crate) mod register;
pub(crate) mod relations;
pub(crate) mod v03_ext;
pub(crate) mod validation;
