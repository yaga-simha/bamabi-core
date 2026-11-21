use super::*;
use std::collections::BTreeSet;
use std::error::Error as _;

fn sample_toml_error() -> toml::de::Error {
    toml::from_str::<toml::Value>("invalid = [").expect_err("invalid TOML should fail")
}

fn sample_semver_version_error() -> semver::Error {
    semver::Version::parse("not-a-semver").expect_err("invalid SemVer version should fail")
}

fn sample_semver_req_error() -> semver::Error {
    semver::VersionReq::parse("===broken").expect_err("invalid SemVer req should fail")
}

#[test]
fn mod_dag_error_kind_as_str_is_unique_and_snake_case() {
    let kinds = [
        (ModDagErrorKind::Io, "io"),
        (ModDagErrorKind::TomlParse, "toml_parse"),
        (ModDagErrorKind::MissingModName, "missing_mod_name"),
        (ModDagErrorKind::InvalidVersion, "invalid_version"),
        (ModDagErrorKind::InvalidAbiVersion, "invalid_abi_version"),
        (
            ModDagErrorKind::IncompatibleAbiVersion,
            "incompatible_abi_version",
        ),
        (
            ModDagErrorKind::InvalidDependencyReq,
            "invalid_dependency_req",
        ),
        (ModDagErrorKind::DuplicateModName, "duplicate_mod_name"),
        (ModDagErrorKind::MissingLibrary, "missing_library"),
        (ModDagErrorKind::MissingDependency, "missing_dependency"),
        (
            ModDagErrorKind::IncompatibleDependencyVersion,
            "incompatible_dependency_version",
        ),
        (ModDagErrorKind::CycleQuarantined, "cycle_quarantined"),
        (ModDagErrorKind::LoadFailed, "load_failed"),
        (ModDagErrorKind::ModNotFound, "mod_not_found"),
    ];

    let mut seen_kinds = BTreeSet::new();
    let mut seen_names = BTreeSet::new();
    for (kind, expected_name) in kinds {
        let s = kind.as_str();
        assert_eq!(s, expected_name);
        assert!(!s.is_empty());
        assert!(
            s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "expected snake_case for {kind:?}, got {s:?}"
        );
        assert!(seen_names.insert(s), "duplicate kind string {s:?}");
        assert!(seen_kinds.insert(format!("{kind:?}")));
    }
    assert_eq!(seen_names.len(), 14);
}

#[test]
fn all_mod_dag_error_variants_expose_expected_structured_accessors() {
    let req_abi = semver::VersionReq::parse("^0.2.0").expect("valid req");
    let host_abi = semver::Version::new(0, 3, 0);
    let req_dep = semver::VersionReq::parse("^2.0").expect("valid req");
    let found_dep = semver::Version::new(1, 5, 0);

    // 1. Io
    let err = ModDagError::Io {
        path: PathBuf::from("mods/missing"),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, "missing dir"),
    };
    assert_eq!(err.kind(), ModDagErrorKind::Io);
    assert_eq!(err.kind().as_str(), "io");
    assert_eq!(err.path(), Some(Path::new("mods/missing")));
    assert_eq!(err.related_paths(), vec![Path::new("mods/missing")]);
    assert_eq!(err.mod_name(), None);
    assert_eq!(err.dependency(), None);
    assert_eq!(err.required_version(), None);
    assert_eq!(err.found_version(), None);
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(err.is_io());
    assert!(!err.is_recoverable());
    assert!(err.source().is_some());

    // 2. TomlParse
    let err = ModDagError::TomlParse {
        path: PathBuf::from("mods/bad/mod.toml"),
        source: Box::new(sample_toml_error()),
    };
    assert_eq!(err.kind(), ModDagErrorKind::TomlParse);
    assert_eq!(err.kind().as_str(), "toml_parse");
    assert_eq!(err.path(), Some(Path::new("mods/bad/mod.toml")));
    assert_eq!(err.related_paths(), vec![Path::new("mods/bad/mod.toml")]);
    assert_eq!(err.mod_name(), None);
    assert_eq!(err.dependency(), None);
    assert_eq!(err.required_version(), None);
    assert_eq!(err.found_version(), None);
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_some());

    // 3. MissingModName
    let err = ModDagError::MissingModName {
        path: PathBuf::from("mods/unnamed/mod.toml"),
    };
    assert_eq!(err.kind(), ModDagErrorKind::MissingModName);
    assert_eq!(err.kind().as_str(), "missing_mod_name");
    assert_eq!(err.path(), Some(Path::new("mods/unnamed/mod.toml")));
    assert_eq!(
        err.related_paths(),
        vec![Path::new("mods/unnamed/mod.toml")]
    );
    assert_eq!(err.mod_name(), None);
    assert_eq!(err.dependency(), None);
    assert_eq!(err.required_version(), None);
    assert_eq!(err.found_version(), None);
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_none());

    // 4. InvalidVersion
    let err = ModDagError::InvalidVersion {
        path: PathBuf::from("mods/bad_ver/mod.toml"),
        raw: "not-a-semver".to_owned(),
        source: sample_semver_version_error(),
    };
    assert_eq!(err.kind(), ModDagErrorKind::InvalidVersion);
    assert_eq!(err.kind().as_str(), "invalid_version");
    assert_eq!(err.path(), Some(Path::new("mods/bad_ver/mod.toml")));
    assert_eq!(
        err.related_paths(),
        vec![Path::new("mods/bad_ver/mod.toml")]
    );
    assert_eq!(err.mod_name(), None);
    assert_eq!(err.dependency(), None);
    assert_eq!(err.required_version(), None);
    assert_eq!(err.found_version(), None);
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_some());

    // 5. InvalidAbiVersion
    let err = ModDagError::InvalidAbiVersion {
        path: PathBuf::from("mods/bad_abi/mod.toml"),
        raw: "===broken".to_owned(),
        source: sample_semver_req_error(),
    };
    assert_eq!(err.kind(), ModDagErrorKind::InvalidAbiVersion);
    assert_eq!(err.kind().as_str(), "invalid_abi_version");
    assert_eq!(err.path(), Some(Path::new("mods/bad_abi/mod.toml")));
    assert_eq!(
        err.related_paths(),
        vec![Path::new("mods/bad_abi/mod.toml")]
    );
    assert_eq!(err.mod_name(), None);
    assert_eq!(err.dependency(), None);
    assert_eq!(err.required_version(), None);
    assert_eq!(err.found_version(), None);
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_some());

    // 6. IncompatibleAbiVersion
    let err = ModDagError::IncompatibleAbiVersion {
        path: PathBuf::from("mods/old_abi/mod.toml"),
        mod_name: "old_abi".to_owned(),
        required: req_abi.clone(),
        host_abi: host_abi.clone(),
    };
    assert_eq!(err.kind(), ModDagErrorKind::IncompatibleAbiVersion);
    assert_eq!(err.kind().as_str(), "incompatible_abi_version");
    assert_eq!(err.path(), Some(Path::new("mods/old_abi/mod.toml")));
    assert_eq!(
        err.related_paths(),
        vec![Path::new("mods/old_abi/mod.toml")]
    );
    assert_eq!(err.mod_name(), Some("old_abi"));
    assert_eq!(err.dependency(), None);
    assert_eq!(err.required_version(), Some(&req_abi));
    assert_eq!(err.found_version(), Some(&host_abi));
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_none());

    // 7. InvalidDependencyReq
    let err = ModDagError::InvalidDependencyReq {
        path: PathBuf::from("mods/bad_dep/mod.toml"),
        mod_name: "bad_dep".to_owned(),
        dependency: "core_lib".to_owned(),
        raw: "===broken".to_owned(),
        source: sample_semver_req_error(),
    };
    assert_eq!(err.kind(), ModDagErrorKind::InvalidDependencyReq);
    assert_eq!(err.kind().as_str(), "invalid_dependency_req");
    assert_eq!(err.path(), Some(Path::new("mods/bad_dep/mod.toml")));
    assert_eq!(
        err.related_paths(),
        vec![Path::new("mods/bad_dep/mod.toml")]
    );
    assert_eq!(err.mod_name(), Some("bad_dep"));
    assert_eq!(err.dependency(), Some("core_lib"));
    assert_eq!(err.required_version(), None);
    assert_eq!(err.found_version(), None);
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_some());

    // 8. DuplicateModName
    let err = ModDagError::DuplicateModName {
        mod_name: "dup_mod".to_owned(),
        first_path: PathBuf::from("mods/01/mod.toml"),
        second_path: PathBuf::from("mods/02/mod.toml"),
    };
    assert_eq!(err.kind(), ModDagErrorKind::DuplicateModName);
    assert_eq!(err.kind().as_str(), "duplicate_mod_name");
    assert_eq!(err.path(), Some(Path::new("mods/02/mod.toml")));
    assert_eq!(
        err.related_paths(),
        vec![Path::new("mods/01/mod.toml"), Path::new("mods/02/mod.toml")]
    );
    assert_eq!(err.mod_name(), Some("dup_mod"));
    assert_eq!(err.dependency(), None);
    assert_eq!(err.required_version(), None);
    assert_eq!(err.found_version(), None);
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_none());

    // 9. MissingLibrary
    let err = ModDagError::MissingLibrary {
        mod_name: "missing_lib".to_owned(),
        path: PathBuf::from("mods/missing_lib/libmissing_lib.dylib"),
    };
    assert_eq!(err.kind(), ModDagErrorKind::MissingLibrary);
    assert_eq!(err.kind().as_str(), "missing_library");
    assert_eq!(
        err.path(),
        Some(Path::new("mods/missing_lib/libmissing_lib.dylib"))
    );
    assert_eq!(
        err.related_paths(),
        vec![Path::new("mods/missing_lib/libmissing_lib.dylib")]
    );
    assert_eq!(err.mod_name(), Some("missing_lib"));
    assert_eq!(err.dependency(), None);
    assert_eq!(err.required_version(), None);
    assert_eq!(err.found_version(), None);
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_none());

    // 10. MissingDependency
    let err = ModDagError::MissingDependency {
        mod_name: "app".to_owned(),
        dependency: "math".to_owned(),
        required: req_dep.clone(),
    };
    assert_eq!(err.kind(), ModDagErrorKind::MissingDependency);
    assert_eq!(err.kind().as_str(), "missing_dependency");
    assert_eq!(err.path(), None);
    assert!(err.related_paths().is_empty());
    assert_eq!(err.mod_name(), Some("app"));
    assert_eq!(err.dependency(), Some("math"));
    assert_eq!(err.required_version(), Some(&req_dep));
    assert_eq!(err.found_version(), None);
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_none());

    // 11. IncompatibleDependencyVersion
    let err = ModDagError::IncompatibleDependencyVersion {
        mod_name: "app".to_owned(),
        dependency: "math".to_owned(),
        required: req_dep.clone(),
        found: found_dep.clone(),
    };
    assert_eq!(err.kind(), ModDagErrorKind::IncompatibleDependencyVersion);
    assert_eq!(err.kind().as_str(), "incompatible_dependency_version");
    assert_eq!(err.path(), None);
    assert!(err.related_paths().is_empty());
    assert_eq!(err.mod_name(), Some("app"));
    assert_eq!(err.dependency(), Some("math"));
    assert_eq!(err.required_version(), Some(&req_dep));
    assert_eq!(err.found_version(), Some(&found_dep));
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_none());

    // 12. CycleQuarantined
    let err = ModDagError::CycleQuarantined {
        cycle_members: vec!["cyc_a".to_owned(), "cyc_b".to_owned()],
    };
    assert_eq!(err.kind(), ModDagErrorKind::CycleQuarantined);
    assert_eq!(err.kind().as_str(), "cycle_quarantined");
    assert_eq!(err.path(), None);
    assert!(err.related_paths().is_empty());
    assert_eq!(err.mod_name(), None);
    assert_eq!(err.dependency(), None);
    assert_eq!(err.required_version(), None);
    assert_eq!(err.found_version(), None);
    assert_eq!(
        err.cycle_members(),
        &["cyc_a".to_owned(), "cyc_b".to_owned()]
    );
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), None);
    assert!(err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_none());

    // 13a. LoadFailed (non-I/O ModLoadError)
    let err = ModDagError::LoadFailed {
        mod_name: "broken".to_owned(),
        path: PathBuf::from("mods/broken/libbroken.dylib"),
        source: Box::new(ModLoadError::InitFailed {
            path: PathBuf::from("mods/broken/libbroken.dylib"),
            status: bamabi_sys::FfiResult::INTERNAL_ERROR,
            diagnostic: "init failed".to_owned(),
        }),
    };
    assert_eq!(err.kind(), ModDagErrorKind::LoadFailed);
    assert_eq!(err.kind().as_str(), "load_failed");
    assert_eq!(err.path(), Some(Path::new("mods/broken/libbroken.dylib")));
    assert_eq!(
        err.related_paths(),
        vec![Path::new("mods/broken/libbroken.dylib")]
    );
    assert_eq!(err.mod_name(), Some("broken"));
    assert_eq!(err.dependency(), None);
    assert_eq!(err.required_version(), None);
    assert_eq!(err.found_version(), None);
    assert!(err.cycle_members().is_empty());
    assert!(matches!(
        err.load_error(),
        Some(ModLoadError::InitFailed { diagnostic, .. }) if diagnostic == "init failed"
    ));
    assert_eq!(err.mod_id(), None);
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(err.is_recoverable());
    assert!(err.source().is_some());

    // 13b. LoadFailed wrapping ShadowCopy (underlying std::io::Error)
    let shadow_err = ModDagError::LoadFailed {
        mod_name: "shadow_broken".to_owned(),
        path: PathBuf::from("mods/shadow_broken/libshadow.dylib"),
        source: Box::new(ModLoadError::ShadowCopy {
            path: PathBuf::from("mods/shadow_broken/libshadow.dylib"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        }),
    };
    assert_eq!(shadow_err.kind(), ModDagErrorKind::LoadFailed);
    assert_eq!(
        shadow_err.path(),
        Some(Path::new("mods/shadow_broken/libshadow.dylib"))
    );
    assert!(shadow_err.is_io());
    assert!(shadow_err.is_recoverable());

    // 14. ModNotFound
    let err = ModDagError::ModNotFound { id: ModId(42) };
    assert_eq!(err.kind(), ModDagErrorKind::ModNotFound);
    assert_eq!(err.kind().as_str(), "mod_not_found");
    assert_eq!(err.path(), None);
    assert!(err.related_paths().is_empty());
    assert_eq!(err.mod_name(), None);
    assert_eq!(err.dependency(), None);
    assert_eq!(err.required_version(), None);
    assert_eq!(err.found_version(), None);
    assert!(err.cycle_members().is_empty());
    assert!(err.load_error().is_none());
    assert_eq!(err.mod_id(), Some(ModId(42)));
    assert!(!err.is_cycle());
    assert!(!err.is_io());
    assert!(!err.is_recoverable());
    assert!(err.source().is_none());
}

#[cfg(feature = "signature-verification")]
#[test]
fn load_failed_signature_verification_is_io_matches_underlying_cause() {
    let io_sig_err = ModDagError::LoadFailed {
        mod_name: "signed_mod".to_owned(),
        path: PathBuf::from("mods/signed/libsigned.dylib"),
        source: Box::new(ModLoadError::SignatureVerification {
            path: PathBuf::from("mods/signed/libsigned.dylib"),
            signature_path: PathBuf::from("mods/signed/libsigned.dylib.sig"),
            source: bamabi_host_sys::SignatureVerificationError::MissingSignatureFile {
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "no sig"),
            },
        }),
    };
    assert!(io_sig_err.is_io());
    assert!(io_sig_err.is_recoverable());

    let crypto_sig_err = ModDagError::LoadFailed {
        mod_name: "signed_mod".to_owned(),
        path: PathBuf::from("mods/signed/libsigned.dylib"),
        source: Box::new(ModLoadError::SignatureVerification {
            path: PathBuf::from("mods/signed/libsigned.dylib"),
            signature_path: PathBuf::from("mods/signed/libsigned.dylib.sig"),
            source: bamabi_host_sys::SignatureVerificationError::InvalidSignature,
        }),
    };
    assert!(!crypto_sig_err.is_io());
    assert!(crypto_sig_err.is_recoverable());
}

#[test]
fn display_strings_remain_stable_and_from_manifest_no_manifests_maps_to_io() {
    let dup = ModDagError::DuplicateModName {
        mod_name: "foo".to_owned(),
        first_path: PathBuf::from("mods/a/mod.toml"),
        second_path: PathBuf::from("mods/b/mod.toml"),
    };
    assert_eq!(
        dup.to_string(),
        "duplicate mod 'foo' declared in 'mods/a/mod.toml' and 'mods/b/mod.toml'"
    );

    let incompat = ModDagError::IncompatibleDependencyVersion {
        mod_name: "app".to_owned(),
        dependency: "math".to_owned(),
        required: semver::VersionReq::parse("^2.0").expect("valid req"),
        found: semver::Version::new(1, 5, 0),
    };
    assert_eq!(
        incompat.to_string(),
        "mod 'app' requires 'math' ^2.0, found 1.5.0"
    );

    let cycle = ModDagError::CycleQuarantined {
        cycle_members: vec!["cyc_a".to_owned(), "cyc_b".to_owned()],
    };
    assert_eq!(
        cycle.to_string(),
        "quarantined cyclic dependency subgraph: cyc_a, cyc_b"
    );

    let not_found = ModDagError::ModNotFound { id: ModId(42) };
    assert_eq!(not_found.to_string(), "mod ModId(42) is not loaded");

    let converted: ModDagError = ManifestError::NoManifestsFound {
        dir: PathBuf::from("empty_mods"),
    }
    .into();
    assert_eq!(converted.kind(), ModDagErrorKind::Io);
    assert!(converted.is_io());
    assert!(!converted.is_recoverable());
    assert_eq!(converted.path(), Some(Path::new("empty_mods")));
    assert_eq!(
        converted.to_string(),
        "I/O error reading 'empty_mods': no `mod.toml` manifests found in 'empty_mods'"
    );
}
