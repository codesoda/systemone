use super::*;

const PINNED_BASE_REVISION: &str = "78cea040597df251eedefa9d7ee2a756af39fe64";

/// Write the four classification files and a manifest whose digests
/// match them, with the given identity fields spliced in.
fn write_bundle(dir: &Path, hf_model: &str, hf_revision: &str, status: &str, release_ready: bool) {
    let mut files = serde_json::Map::new();
    for name in VERIFIED_FILES {
        let path = dir.join(name);
        fs::write(&path, format!("contents of {name}")).unwrap();
        let (bytes, sha256) = sha256_file(&path).unwrap();
        files.insert(
            name.to_owned(),
            serde_json::json!({ "bytes": bytes, "sha256": sha256 }),
        );
    }
    let manifest = serde_json::json!({
        "manifest_version": 1,
        "architecture": "boundary",
        "architecture_version": 1,
        "status": status,
        "release_ready": release_ready,
        "hf_model": hf_model,
        "hf_revision": hf_revision,
        "gliner2_commit": "test",
        "opset": 17,
        "precision": "fp32",
        "ort_crate_version": "test",
        "native_onnx_runtime": "test",
        "validation_onnxruntime": "test",
        "dependencies": {},
        "source_file_sha256": {},
        "files": files,
        "graphs": {},
        "validation": { "parity": "test" },
    });
    fs::write(
        dir.join(MANIFEST),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn pinned_validated_bundle_verifies() {
    let dir = tempfile::tempdir().unwrap();
    write_bundle(
        dir.path(),
        "fastino/gliner2.5-base-v1",
        PINNED_BASE_REVISION,
        "validated",
        true,
    );
    let identity = verify_against_manifest(dir.path(), "gliner2.5-base-v1").unwrap();
    assert_eq!(identity.hf_model, "fastino/gliner2.5-base-v1");
    assert_eq!(identity.hf_revision, PINNED_BASE_REVISION);
}

#[test]
fn short_revision_is_a_clean_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    write_bundle(
        dir.path(),
        "fastino/gliner2.5-base-v1",
        "main",
        "validated",
        true,
    );
    let error = verify_against_manifest(dir.path(), "gliner2.5-base-v1").unwrap_err();
    assert!(matches!(error, HostError::Unavailable(_)), "{error}");
    assert!(error.to_string().contains("revision"), "{error}");
}

#[test]
fn unvalidated_or_unready_bundles_are_rejected() {
    for (status, release_ready) in [("exported-unvalidated", true), ("validated", false)] {
        let dir = tempfile::tempdir().unwrap();
        write_bundle(
            dir.path(),
            "fastino/gliner2.5-base-v1",
            PINNED_BASE_REVISION,
            status,
            release_ready,
        );
        let error = verify_against_manifest(dir.path(), "gliner2.5-base-v1").unwrap_err();
        assert!(
            error.to_string().contains("not a validated release bundle"),
            "{error}"
        );
    }
}

#[test]
fn unpinned_model_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    write_bundle(
        dir.path(),
        "someone/gliner2.5-base-v1",
        PINNED_BASE_REVISION,
        "validated",
        true,
    );
    let error = verify_against_manifest(dir.path(), "gliner2.5-base-v1").unwrap_err();
    assert!(error.to_string().contains("unpinned model"), "{error}");
}

#[test]
fn tampered_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    write_bundle(
        dir.path(),
        "fastino/gliner2.5-base-v1",
        PINNED_BASE_REVISION,
        "validated",
        true,
    );
    fs::write(dir.path().join("encoder.onnx"), "tampered").unwrap();
    let error = verify_against_manifest(dir.path(), "gliner2.5-base-v1").unwrap_err();
    assert!(error.to_string().contains("does not match"), "{error}");
}

#[test]
fn empty_and_oversized_manifests_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    write_bundle(
        dir.path(),
        "fastino/gliner2.5-base-v1",
        PINNED_BASE_REVISION,
        "validated",
        true,
    );
    fs::write(dir.path().join(MANIFEST), "").unwrap();
    let error = verify_against_manifest(dir.path(), "gliner2.5-base-v1").unwrap_err();
    assert!(error.to_string().contains("is empty"), "{error}");
    let oversized = vec![b' '; (MAX_MANIFEST_BYTES + 1) as usize];
    fs::write(dir.path().join(MANIFEST), oversized).unwrap();
    let error = verify_against_manifest(dir.path(), "gliner2.5-base-v1").unwrap_err();
    assert!(error.to_string().contains("manifest limit"), "{error}");
}
