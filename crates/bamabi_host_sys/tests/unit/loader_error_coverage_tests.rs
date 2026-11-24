use super::*;
use crate::registry::ComponentCollisionError;
use bamabi_sys::pack_abi_version;
use std::error::Error as StdError;
use std::path::PathBuf;

#[test]
fn abi_version_compatibility_matches_semver_rules() {
    let host = pack_abi_version(1, 2, 3);
    assert!(is_abi_version_compatible(host, host));
    assert!(is_abi_version_compatible(host, pack_abi_version(1, 1, 9)));
    assert!(!is_abi_version_compatible(host, pack_abi_version(1, 3, 0)));
    assert!(!is_abi_version_compatible(host, pack_abi_version(2, 0, 0)));
    assert!(!is_abi_version_compatible(host, pack_abi_version(0, 2, 0)));
}

#[test]
fn mod_load_error_display_and_source_cover_pure_variants() {
    let shadow = ModLoadError::ShadowCopy {
        path: PathBuf::from("foo.dylib"),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
    };
    assert!(shadow.to_string().contains("failed to shadow-copy mod"));
    assert!(shadow.source().is_some());

    let invalid_manifest = ModLoadError::InvalidManifest {
        path: PathBuf::from("foo.dylib"),
        status: FfiResult::INVALID_VERSION,
        detail: "bad size".to_string(),
    };
    assert!(invalid_manifest.to_string().contains("invalid BamManifest"));
    assert!(invalid_manifest.source().is_none());

    let incompatible = ModLoadError::IncompatibleAbiVersion {
        path: PathBuf::from("foo.dylib"),
        host_version: 0x0100_0000,
        guest_version: 0x0200_0000,
    };
    assert!(
        incompatible
            .to_string()
            .contains("incompatible ABI version")
    );
    assert!(incompatible.source().is_none());

    let init_empty = ModLoadError::InitFailed {
        path: PathBuf::from("foo.dylib"),
        status: FfiResult::INTERNAL_ERROR,
        diagnostic: String::new(),
    };
    assert!(!init_empty.to_string().ends_with(": "));
    let init_diag = ModLoadError::InitFailed {
        path: PathBuf::from("foo.dylib"),
        status: FfiResult::INTERNAL_ERROR,
        diagnostic: "bad config".to_string(),
    };
    assert!(init_diag.to_string().contains(": bad config"));
    assert!(init_diag.source().is_none());

    let panic_empty = ModLoadError::InitPanicked {
        path: PathBuf::from("foo.dylib"),
        diagnostic: String::new(),
    };
    assert!(
        panic_empty
            .to_string()
            .contains("panicked during initialization")
    );
    let panic_diag = ModLoadError::InitPanicked {
        path: PathBuf::from("foo.dylib"),
        diagnostic: "boom".to_string(),
    };
    assert!(panic_diag.to_string().contains(": boom"));

    let missing_comps = ModLoadError::MissingComponents {
        path: PathBuf::from("foo.dylib"),
        system_name: "sys".to_string(),
        missing: vec!["A".to_string(), "B".to_string()],
    };
    assert!(missing_comps.to_string().contains("A, B"));

    let unreg_sched = ModLoadError::UnregisteredSchedule {
        path: PathBuf::from("foo.dylib"),
        system_name: "sys".to_string(),
        schedule: ScheduleId::custom(42),
    };
    assert!(
        unreg_sched
            .to_string()
            .contains("unregistered custom schedule")
    );

    let unresolved = ModLoadError::UnresolvedOrderingTarget {
        path: PathBuf::from("foo.dylib"),
        system_name: "sys".to_string(),
        target: "other::sys".to_string(),
    };
    assert!(unresolved.to_string().contains("other::sys"));

    let invalid_sched = ModLoadError::InvalidScheduleRegistration {
        schedule: ScheduleId::UPDATE,
    };
    assert!(invalid_sched.to_string().contains("CUSTOM_BASE"));

    let collision: ModLoadError = ComponentCollisionError::MetadataMismatch {
        name: "demo::Comp".to_string(),
        reason: "size mismatch".to_string(),
    }
    .into();
    assert!(collision.to_string().contains("size mismatch"));
    assert!(collision.source().is_none());

    let uuid_coll = ComponentCollisionError::UuidCollision {
        existing_name: "a::X".to_string(),
        new_name: "b::Y".to_string(),
        uuid: bamabi_sys::ComponentUuid::from_canonical_name("a::X"),
    };
    assert!(uuid_coll.to_string().contains("b::Y"));

    let not_found = ModLoadError::ModNotFound { id: ModId(99) };
    assert!(not_found.to_string().contains("is not loaded"));

    let leased = ModLoadError::ScheduleLeased {
        id: ModId(1),
        schedule: "Update".to_string(),
    };
    assert!(leased.to_string().contains("currently running"));

    let save_failed = ModLoadError::StateSaveFailed {
        id: ModId(1),
        mod_name: "demo".to_string(),
        status: FfiResult::INTERNAL_ERROR,
    };
    assert!(save_failed.to_string().contains("save_state failed"));

    let restore_failed = ModLoadError::StateRestoreFailed {
        path: PathBuf::from("foo.dylib"),
        mod_name: "demo".to_string(),
        status: FfiResult::INTERNAL_ERROR,
    };
    assert!(restore_failed.to_string().contains("restore_state failed"));

    let migration_failed = ModLoadError::SchemaMigrationFailed {
        component_name: "demo::Comp".to_string(),
        status: FfiResult::INVALID_ARGUMENT,
        detail: "incompatible layout".to_string(),
    };
    assert!(
        migration_failed
            .to_string()
            .contains("schema migration failed")
    );
    assert!(migration_failed.source().is_none());
}

#[cfg(all(not(miri), feature = "dynamic-loader"))]
#[test]
fn mod_load_error_dlopen_variants_format_and_expose_source() {
    // SAFETY: Loading a nonexistent library path deterministically returns `Err(libloading::Error)`.
    let lib_err_a = unsafe { libloading::Library::new("/nonexistent/bamabi_test_missing.so") }
        .expect_err("nonexistent shared library path should fail dlopen");
    let dlopen = ModLoadError::DlOpen {
        path: PathBuf::from("/nonexistent/bamabi_test_missing.so"),
        source: lib_err_a,
    };
    assert!(dlopen.to_string().contains("failed to load mod"));
    assert!(dlopen.source().is_some());

    // SAFETY: Same deterministic missing-library error reused for missing-symbol variants.
    let lib_err_b = unsafe { libloading::Library::new("/nonexistent/bamabi_test_missing_2.so") }
        .expect_err("nonexistent shared library path should fail dlopen");
    let missing_manifest = ModLoadError::MissingManifest {
        path: PathBuf::from("foo.dylib"),
        source: lib_err_b,
    };
    assert!(missing_manifest.to_string().contains("bamabi_manifest"));
    assert!(missing_manifest.source().is_some());

    // SAFETY: Same deterministic missing-library error reused for missing entry point variant.
    let lib_err_c = unsafe { libloading::Library::new("/nonexistent/bamabi_test_missing_3.so") }
        .expect_err("nonexistent shared library path should fail dlopen");
    let missing_entry = ModLoadError::MissingEntryPoint {
        path: PathBuf::from("foo.dylib"),
        symbol: "bamabi_init",
        source: lib_err_c,
    };
    assert!(missing_entry.to_string().contains("bamabi_init"));
    assert!(missing_entry.source().is_some());
}
