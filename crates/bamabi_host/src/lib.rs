#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Safe headless Bevy host crate for `bamabi` (`ABI 0.4.0`).
//!
//! `bamabi_host` provides core Bevy ECS (`v0.20+`) integration (`ModWorldExt`, `ModAppExt`,
//! `ModCommandsExt`, `AbiModPlugin`) for loading, executing, quarantining, and hot-unloading
//! native C-ABI mods compiled in Rust, C99, C++, or Zig — with zero rendering, reflection, scene,
//! asset, or filesystem-watcher dependencies.
//!
//! All `unsafe` FFI internals are isolated inside `bamabi_host_sys`; this crate enforces
//! `#![forbid(unsafe_code)]`. Optional Bevy subsystem bridges live in companion
//! `#![forbid(unsafe_code)]` crates (`bamabi_bridges`, `bamabi_render`,
//! `bamabi_reflect`, and `bamabi_watcher`), re-exported together by the top-level
//! `bamabi` facade crate.
//!
//! # Optional Features
//!
//! - `strict-ffi-checks`: Extra FFI pointer, alignment, and handle-generation validation.
//! - `diagnostics`: Publishes per-mod and aggregate `bevy_diagnostic` counters.
//! - `signature-verification`: Pure-Rust OpenSSH `SSHSIG` + Ed25519 CA certificate verification
//!   of shared libraries prior to `dlopen` / `LoadLibraryW` (`ModTrustPolicy`, `VerifiedModSignature`).
//! - `keygen`: Implies `signature-verification` and enables pure-Rust Ed25519 OpenSSH keypair
//!   generation and certificate issuance (`generate_ed25519_keypair`,
//!   `issue_ed25519_certificate_openssh`, `write_openssh_keypair`, `GeneratedKeypair`,
//!   `IssuedCertificate`, `keygen`).
//!
//! # Quickstart
//!
//! ```no_run
//! use bamabi_host::prelude::*;
//! use bevy_app::{App, Update};
//! use bevy_ecs::component::Component;
//! use bevy_ecs::schedule::SystemSet;
//!
//! #[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
//! struct PhysicsSet;
//!
//! #[repr(C)]
//! #[derive(Component, Clone, Copy, Default, ModComponent)]
//! #[mod_component(name = "game::Position")]
//! struct Position {
//!     x: f32,
//!     y: f32,
//! }
//!
//! let mut app = App::new();
//! app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
//! app.register_mod_component::<Position>(ComponentOptions::new().with_raw_insert().with_default())
//!     .unwrap();
//! app.register_mod_visible_set("physics", PhysicsSet);
//! app.load_mod_with("mods/libmovement.dylib", LoadOptions::default()).unwrap();
//! ```

pub mod app_ext;
pub mod budgets;
pub mod commands_ext;
#[cfg(feature = "diagnostics")]
pub mod diagnostics;
pub mod events;
pub mod harness;
pub mod mod_dag;
pub mod plugin;
pub mod schedule_sets;
pub mod telemetry;
pub mod world_ext;

pub use bamabi_macros::{AbiPod, ModComponent};
pub use bamabi_sys;

#[cfg(feature = "dynamic-loader")]
pub use app_ext::ModLoaderAppExt;
pub use app_ext::{ModAppExt, ModRegistryAppExt};
pub use bamabi_host_sys::PersistentMessageCursors;
pub use bamabi_host_sys::loader::{
    ExportedFieldRecord, ReloadColumnSnapshot, SnapshotComponentColumn, SnapshotEntityFields,
};
#[cfg(feature = "signature-verification")]
pub use bamabi_host_sys::{
    Algorithm, BAM_SIG_MACHO_SECTION_NAME, BAM_SIG_SECTION_NAME, BAMPACK_MAGIC, BAMPACK_VERSION,
    BamPackBuilder, BamPackEntry, BamPackExtractError, BamPackExtractLimits, Certificate,
    CertificateValidityPolicy, DEFAULT_BAMPACK_NAMESPACE, DEFAULT_KRL_SIGNATURE_NAMESPACE,
    DEFAULT_KRL_SIGNER_PRINCIPAL, DEFAULT_MAX_BAMPACK_UNCOMPRESSED_BYTES,
    DEFAULT_MAX_MOD_BINARY_BYTES, DEFAULT_SIGNATURE_NAMESPACE, EmbeddedSignatureSection,
    Fingerprint, HashAlg, KrlBuilder, LineEnding, MAX_BAMPACK_ENTRIES, MAX_BAMPACK_PATH_DEPTH,
    MAX_BAMPACK_PATH_LEN, MAX_KRL_BYTES, MAX_SIGNATURE_SIDECAR_BYTES, ModSigningError,
    ModTrustConfigError, ModTrustPolicy, PrivateKey, PublicKey, SignatureEnforcement,
    SignatureVerificationError, VerifiedBamPack, VerifiedModSignature, bampack,
    default_signature_path_for, embedded_sig, extract_embedded_bam_sig,
    parse_bampack_entries_unverified, preverify_mod_on_disk, sign_mod_bytes_with_certificate,
    sign_mod_bytes_with_key, sign_mod_bytes_with_openssh_certificate, ssh_key,
    validate_bampack_entry_path, zeroed_binary_for_bam_sig,
};
pub use bamabi_host_sys::{
    BamFieldBytePatch, BamFieldDescriptor, BamFieldPodKind, BamResolveSymbolFn, BamSymbolTable,
    BinaryFormat, CascadeHierarchyCache, CommandArenaConfig, ComponentCollisionError,
    ComponentOptions, ConfiguredScheduleSet, HostStringBridge, HostWireBridge,
    InMemoryModEntrypoints, InMemoryObjectLoader, LibraryResidencyInfo, LoadOptions, LoadedMod,
    ModBacking, ModErrorPolicy, ModErrorPolicyConfig, ModHost, ModId, ModLoadError,
    ModNamedObjects, ModQuarantine, ModRegistry, ModRelationGraph, ModScheduleSetRegistry,
    ModScheduleSetRewirePass, ModScoped, PreparedModImage, RegisteredComponent, RegistrySnapshot,
    RelationEdgeView, RelationFlushError, RelationFlushPolicy, ScheduleSetConfig, ScheduleSetMode,
    TargetEntity, WitnessFieldDescriptor, WorldDropGuard, apply_entity_component_fields,
    apply_mod_schedule_sets_to_schedule, apply_resource_fields, check_relation_acyclic,
    ensure_component_in_target_world, ensure_relation_graph_in_world,
    export_entity_component_fields, export_resource_fields, extract_component_between_worlds,
    extract_resource_between_worlds, hash_world_pod_state, insert_entity_component_from_fields,
    insert_entity_pod_with_field_patches, insert_or_apply_entity_component_fields,
    inspect_component_bam_fields, inspect_component_bam_fields_in_registry,
    inspect_component_witness_fields, inspect_component_witness_fields_in_registry,
    inspect_library_residency, inspect_library_residency_file, is_abi_version_compatible,
    prevalidate_mod_schedule_set_rewire_in_schedule, prevalidate_mod_schedule_set_rewire_in_world,
    read_entity_bam_field_bytes, read_entity_pod_bytes, read_resource_bam_field_bytes,
    read_table_column_bytes, register_bridged_component, register_bridged_entity_event,
    register_bridged_entity_event_with, register_bridged_resource,
    register_bridged_string_component, rewire_mod_schedule_sets_in_schedule,
    rewire_mod_schedule_sets_in_world, spawn_entity_pod_bundle, write_entity_bam_field_bytes,
    write_entity_pod_bytes, write_resource_bam_field_bytes,
};
#[cfg(feature = "keygen")]
pub use bamabi_host_sys::{
    GeneratedKeypair, IssuedCertificate, generate_ed25519_keypair,
    issue_ed25519_certificate_openssh, keygen, write_openssh_keypair,
};
pub use budgets::{
    DEFAULT_BUDGET_WINDOW_SIZE, DEFAULT_MAX_CONSECUTIVE_BREACHES, ModBudgetBreach,
    ModBudgetBreachKind, ModBudgetExt, ModBudgetLimits, ModBudgetPolicy, ModBudgetSnapshot,
    ModBudgetTelemetry, flush_mod_budgets, sync_mod_budget_policy,
};
pub use commands_ext::ModCommandsExt;
pub use events::{
    LoadedModInfo, ModLoadFailed, ModLoaded, ModSystemError, ModSystemFaulted, ModUnloaded,
    SystemInitError,
};
pub use harness::HeadlessModTestHarness;
pub use mod_dag::{
    DagResolution, DirectoryLoadReport, ManifestError, ModDagError, ModDagErrorKind, ModDagNode,
    ModDependencyGraph, ModTomlManifest, discover_mod_manifests, host_abi_semver,
    resolve_dependency_dag,
};
pub use plugin::{
    AbiModPlugin, ConfiguredAbiModPlugin, ModLifecycleFlush, ensure_mod_lifecycle_flush_schedule,
};
pub use schedule_sets::{
    ModNamedSystemSet, ModScheduleMap, ModSystemSet, ModVisibleSets, StartupCompleted,
    configure_guest_system_in_world, configure_weak_set_chain,
};
pub use telemetry::{
    DEFAULT_TELEMETRY_RING_CAPACITY, ModTelemetryRing, ModTelemetrySinkPlugin,
    NativeSystemTelemetryTracker, TelemetryDigestSinkFn, record_native_system_run,
    with_native_system_telemetry, wrap_native_system,
};
pub use world_ext::{
    ModUnloadHookFn, ModUnloadHooks, ModWorldExt, register_mod_unload_hook_in_world,
};

/// Convenient prelude module for Bevy games hosting native C-ABI mods (`ABI 0.4.0`).
pub mod prelude {
    pub use crate::app_ext::ModAppExt;
    pub use crate::budgets::{
        DEFAULT_BUDGET_WINDOW_SIZE, DEFAULT_MAX_CONSECUTIVE_BREACHES, ModBudgetBreach,
        ModBudgetBreachKind, ModBudgetExt, ModBudgetLimits, ModBudgetPolicy, ModBudgetSnapshot,
        ModBudgetTelemetry,
    };
    pub use crate::commands_ext::ModCommandsExt;
    #[cfg(feature = "signature-verification")]
    pub use crate::events::VerifiedModSignature;
    pub use crate::events::{
        LoadedModInfo, ModLoadFailed, ModLoaded, ModSystemError, ModSystemFaulted, ModUnloaded,
        SystemInitError,
    };
    pub use crate::harness::HeadlessModTestHarness;
    pub use crate::mod_dag::{
        DirectoryLoadReport, ModDagError, ModDagErrorKind, ModDependencyGraph, ModTomlManifest,
    };
    pub use crate::plugin::{
        AbiModPlugin, ConfiguredAbiModPlugin, ModLifecycleFlush,
        ensure_mod_lifecycle_flush_schedule,
    };
    pub use crate::schedule_sets::{
        ModNamedSystemSet, ModScheduleMap, ModSystemSet, ModVisibleSets, StartupCompleted,
        configure_guest_system_in_world, configure_weak_set_chain,
    };
    pub use crate::telemetry::{ModTelemetryRing, ModTelemetrySinkPlugin};
    pub use crate::world_ext::{ModUnloadHooks, ModWorldExt};
    pub use crate::{
        AbiPod, BamSymbolTable, CascadeHierarchyCache, CommandArenaConfig, ComponentCollisionError,
        ComponentOptions, ConfiguredScheduleSet, InMemoryModEntrypoints, InMemoryObjectLoader,
        LoadOptions, LoadedMod, ModBacking, ModComponent, ModErrorPolicy, ModHost, ModId,
        ModLoadError, ModNamedObjects, ModQuarantine, ModRegistry, ModRelationGraph,
        ModScheduleSetRegistry, ModScheduleSetRewirePass, ModScoped, PersistentMessageCursors,
        ScheduleSetConfig, ScheduleSetMode, TargetEntity, hash_world_pod_state,
        insert_entity_pod_with_field_patches, inspect_component_bam_fields,
        read_entity_bam_field_bytes, read_resource_bam_field_bytes, read_table_column_bytes,
        rewire_mod_schedule_sets_in_world, spawn_entity_pod_bundle, write_entity_bam_field_bytes,
        write_resource_bam_field_bytes,
    };
    #[cfg(feature = "signature-verification")]
    pub use crate::{
        CertificateValidityPolicy, DEFAULT_KRL_SIGNATURE_NAMESPACE, DEFAULT_MAX_MOD_BINARY_BYTES,
        DEFAULT_SIGNATURE_NAMESPACE, KrlBuilder, MAX_KRL_BYTES, MAX_SIGNATURE_SIDECAR_BYTES,
        ModSigningError, ModTrustConfigError, ModTrustPolicy, SignatureEnforcement,
        SignatureVerificationError, default_signature_path_for, preverify_mod_on_disk,
        sign_mod_bytes_with_certificate, sign_mod_bytes_with_key,
        sign_mod_bytes_with_openssh_certificate,
    };
    #[cfg(feature = "keygen")]
    pub use crate::{
        GeneratedKeypair, IssuedCertificate, generate_ed25519_keypair,
        issue_ed25519_certificate_openssh, write_openssh_keypair,
    };
    pub use bamabi_sys::{
        ABI_VERSION, AccessMode, BAM_COMPONENT_FLAG_SUMMARY_TICK, BAM_LOG_DEBUG, BAM_LOG_ERROR,
        BAM_LOG_INFO, BAM_LOG_TRACE, BAM_LOG_WARN, BAM_OBSERVER_FLAG_PROPAGATE,
        BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL, BAM_TRIGGER_FLAG_PROPAGATE, BamChangeTicks,
        BamColumnInfo, BamComponent, BamEventKind, BamFieldDesc, BamFieldKind, BamLayoutInfo,
        BamObserverDesc, BamObserverFn, BamObserverMutFn, BamQueryDesc, BamRelationDesc,
        BamRequiredComponent, BamRowMigrateFn, BamRunIfFn, BamScheduleSetDesc, BamSymbol,
        BamSystem, BamSystemDesc, BamSystemSet, BamTermDesc, BamTriggerDesc, ComponentDesc,
        ComponentUuid, FfiAssetHandle, FfiAssetLoadState, FfiBool, FfiChildOf, FfiEntity,
        FfiOption, FfiResult, FfiSlice, RawInsertable, ScheduleId, StrView, VTableHeader,
    };
}
