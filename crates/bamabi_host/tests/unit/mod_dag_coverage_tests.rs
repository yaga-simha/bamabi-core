use super::*;
use std::error::Error as StdError;

fn toml_err() -> toml::de::Error {
    toml::from_str::<toml::Value>("k = [").expect_err("invalid toml")
}

fn semver_ver_err() -> semver::Error {
    semver::Version::parse("invalid").expect_err("invalid semver")
}

fn semver_req_err() -> semver::Error {
    semver::VersionReq::parse("====").expect_err("invalid req")
}

#[test]
fn mod_dag_error_display_and_from_manifest_error_cover_all_variants() {
    let req = semver::VersionReq::parse("^1.0").expect("valid req");
    let found = semver::Version::new(2, 0, 0);

    let converted: Vec<ModDagError> = vec![
        ManifestError::Io {
            path: PathBuf::from("a/mod.toml"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        }
        .into(),
        ManifestError::TomlParse {
            path: PathBuf::from("b/mod.toml"),
            source: Box::new(toml_err()),
        }
        .into(),
        ManifestError::MissingModName {
            path: PathBuf::from("c/mod.toml"),
        }
        .into(),
        ManifestError::InvalidVersion {
            path: PathBuf::from("d/mod.toml"),
            raw: "bad".into(),
            source: semver_ver_err(),
        }
        .into(),
        ManifestError::InvalidAbiVersion {
            path: PathBuf::from("e/mod.toml"),
            raw: "bad".into(),
            source: semver_req_err(),
        }
        .into(),
        ManifestError::IncompatibleAbiVersion {
            path: PathBuf::from("f/mod.toml"),
            mod_name: "old_mod".into(),
            required: req.clone(),
            host_abi: found.clone(),
        }
        .into(),
        ManifestError::InvalidDependencyReq {
            path: PathBuf::from("g/mod.toml"),
            mod_name: "g".into(),
            dependency: "dep".into(),
            raw: "bad".into(),
            source: semver_req_err(),
        }
        .into(),
        ManifestError::DuplicateModName {
            mod_name: "dup".into(),
            first_path: PathBuf::from("1/mod.toml"),
            second_path: PathBuf::from("2/mod.toml"),
        }
        .into(),
        ManifestError::MissingLibrary {
            mod_name: "nolib".into(),
            path: PathBuf::from("nolib.so"),
        }
        .into(),
        ManifestError::MissingDependency {
            mod_name: "need".into(),
            dependency: "core".into(),
            required: req.clone(),
        }
        .into(),
        ManifestError::IncompatibleDependencyVersion {
            mod_name: "need".into(),
            dependency: "core".into(),
            required: req,
            found,
        }
        .into(),
        ManifestError::CycleQuarantined {
            cycle_members: vec!["x".into(), "y".into()],
        }
        .into(),
        ManifestError::NoManifestsFound {
            dir: PathBuf::from("empty"),
        }
        .into(),
    ];

    for err in &converted {
        let msg = err.to_string();
        assert!(!msg.is_empty());
        let _ = err.source();
    }

    let load_failed = ModDagError::LoadFailed {
        mod_name: "broken".into(),
        path: PathBuf::from("broken.so"),
        source: Box::new(ModLoadError::ModNotFound { id: ModId(1) }),
    };
    assert!(load_failed.to_string().contains("failed to load mod"));

    let not_found = ModDagError::ModNotFound { id: ModId(7) };
    assert!(not_found.to_string().contains("is not loaded"));
}

#[test]
fn mod_dependency_graph_new_rename_and_register_dependency() {
    let mut graph = ModDependencyGraph::new();
    let opts = LoadOptions::default();

    graph.register_loaded_mod(
        ModId(1),
        "alpha".into(),
        PathBuf::from("alpha.so"),
        opts.clone(),
        vec!["base".into()],
    );
    // Re-register same `ModId` under a different name to exercise stale name cleanup
    graph.register_loaded_mod(
        ModId(1),
        "alpha_v2".into(),
        PathBuf::from("alpha_v2.so"),
        opts,
        vec!["base".into()],
    );
    graph.register_dependency(ModId(1), "extra".into());
    // Duplicate dependency is not appended twice
    graph.register_dependency(ModId(1), "extra".into());
    // Unknown dependent is a no-op
    graph.register_dependency(ModId(999), "ignored".into());

    let node = graph.get_node(ModId(1)).expect("node present");
    assert_eq!(node.name, "alpha_v2");
    assert_eq!(
        node.dependencies,
        vec!["base".to_string(), "extra".to_string()]
    );

    graph.unregister_mod(ModId(1));
    assert!(graph.get_node(ModId(1)).is_none());
    graph.clear();
}
