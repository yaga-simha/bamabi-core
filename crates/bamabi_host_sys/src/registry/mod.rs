//! Component, resource, asset, VTable, and `"bam.v0.*"` symbol registration catalog (`ModRegistry`).

pub mod bridge;
pub mod catalog;
pub mod collision;

pub(crate) use bridge::MigratedComponentSpec;
pub use bridge::{
    ExtractRelationshipTargetsFn, HostStringBridge, HostWireBridge, register_bridged_component,
    register_bridged_entity_event, register_bridged_entity_event_with, register_bridged_resource,
    register_bridged_string_component,
};
pub use catalog::{
    ComponentOptions, CreateMsgCursorFn, EntityEventTriggerFn, ExtractEventTargetFn,
    InsertDefaultFn, InsertRawFn, ModRegistry, ModRelationGraph, MsgReadBatchFn, MsgSendFn,
    MsgWriteBatchFn, RegisteredComponent, RegistrySnapshot, RelationEdgeView, RelationFlushError,
    RelationFlushPolicy, TargetEntity, TriggerEventFn, check_relation_acyclic,
    ensure_relation_graph_in_world,
};
pub(crate) use catalog::{
    DynamicComponentReg, NewComponentBase, RelationRegSpec, SymbolRecord,
    component_flags_to_storage_flags, extract_owned_dynamic_fields,
};
pub use collision::ComponentCollisionError;
