use super::*;
use serde_json::{Value, json};

const FIXTURE: &str = r#"{
    "schema_version": 1,
    "source_sha": "0123456789abcdef0123456789abcdef01234567",
    "target": "aarch64-apple-darwin",
    "rust": {
        "toolchain": "1.95.0",
        "compiler_commit": "abcdef0123456789abcdef0123456789abcdef01"
    },
    "sdk": { "version": "26.2", "build": "25C58" },
    "artifacts": [
        {
            "name": "handkited", "size": 1,
            "sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "code_identifier": "com.handkite.handkited"
        },
        {
            "name": "handkitectl", "size": 1024,
            "sha256": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "code_identifier": "com.handkite.handkitectl"
        },
        {
            "name": "handkite-mcp", "size": 1073741824,
            "sha256": "9876543210abcdef9876543210abcdef9876543210abcdef9876543210abcdef",
            "code_identifier": "com.handkite.handkite-mcp"
        }
    ]
}"#;

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).unwrap()
}

fn parse(value: &Value) -> Result<ReleaseManifest, &'static str> {
    ReleaseManifest::parse(&serde_json::to_vec(value).unwrap())
}

#[test]
fn accepts_inventory_and_boundary_sizes_in_any_order() {
    let manifest = ReleaseManifest::parse(FIXTURE.as_bytes()).unwrap();
    assert_eq!(
        manifest.source_sha(),
        "0123456789abcdef0123456789abcdef01234567"
    );
    assert_eq!(manifest.rust().toolchain, "1.95.0");
    assert_eq!(manifest.sdk().version, "26.2");
    assert_eq!(manifest.sdk().build, "25C58");
    let artifacts = manifest.artifacts();
    assert_eq!(artifacts[0].name.filename(), "handkited");
    assert_eq!(artifacts[1].name.filename(), "handkitectl");
    assert_eq!(artifacts[2].name.filename(), "handkite-mcp");
    assert_eq!(artifacts[0].size, 1);
    assert_eq!(artifacts[2].size, MAX_ARTIFACT_BYTES);
    let mut value = fixture();
    value["artifacts"].as_array_mut().unwrap().reverse();
    value["sdk"]["version"] = json!("26.2.1");
    assert!(parse(&value).is_ok());
}

#[test]
fn rejects_unknown_duplicate_and_missing_fields_at_every_level() {
    for pointer in ["", "/rust", "/sdk", "/artifacts/0"] {
        let object = fixture()
            .pointer(pointer)
            .unwrap()
            .as_object()
            .unwrap()
            .clone();
        for key in object.keys() {
            let mut value = fixture();
            value
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(parse(&value).is_err(), "missing {pointer}/{key}");

            let needle = format!("\"{key}\":");
            let duplicate =
                FIXTURE.replacen(&needle, &format!("{needle}{},{needle}", object[key]), 1);
            assert!(
                ReleaseManifest::parse(duplicate.as_bytes()).is_err(),
                "duplicate {key}"
            );
        }
        let mut value = fixture();
        value
            .pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("signer".into(), json!("untrusted"));
        assert!(parse(&value).is_err(), "unknown field at {pointer}");
    }
    // Escapes must not disguise duplicate keys.
    let duplicate = FIXTURE.replacen(
        "\"schema_version\": 1",
        r#""schema_version": 1, "schema_\u0076ersion": 1"#,
        1,
    );
    assert!(ReleaseManifest::parse(duplicate.as_bytes()).is_err());
}

#[test]
fn rejects_invalid_build_identity_and_schema() {
    for (pointer, invalid) in [
        ("/schema_version", json!(0)),
        ("/schema_version", json!(2)),
        ("/schema_version", json!(1.0)),
        ("/target", json!("x86_64-apple-darwin")),
        ("/source_sha", json!("0123456")),
        ("/source_sha", json!("A".repeat(40))),
        ("/source_sha", json!("g".repeat(40))),
        ("/source_sha", json!("a".repeat(41))),
        ("/rust/compiler_commit", json!("a".repeat(39))),
        ("/rust/toolchain", json!("stable")),
        ("/rust/toolchain", json!("1.95.0-nightly")),
        ("/rust/toolchain", json!("1.95.0-aarch64-apple-darwin")),
        ("/rust/toolchain", json!("1.095.0")),
        ("/rust/toolchain", json!("1.95")),
        ("/sdk/version", json!("26")),
        ("/sdk/version", json!("26.2.0.1")),
        ("/sdk/version", json!("26..2")),
        ("/sdk/version", json!("26.1000")),
        ("/sdk/build", json!("")),
        ("/sdk/build", json!("x".repeat(33))),
        ("/sdk/build", json!("25C58\n")),
    ] {
        let mut value = fixture();
        *value.pointer_mut(pointer).unwrap() = invalid;
        assert!(
            parse(&value).is_err(),
            "{pointer}: {}",
            value.pointer(pointer).unwrap()
        );
    }
}

#[test]
fn rejects_missing_duplicate_extra_or_unexpected_artifacts() {
    for count in [0, 1, 2, 4] {
        let mut value = fixture();
        let artifacts = value["artifacts"].as_array_mut().unwrap();
        artifacts.resize(count, artifacts[0].clone());
        assert!(parse(&value).is_err(), "artifact count {count}");
    }
    let mut value = fixture();
    value["artifacts"][1] = value["artifacts"][0].clone();
    assert!(parse(&value).is_err());
    value = fixture();
    value["artifacts"][0]["name"] = json!("handkite-doctor");
    assert!(parse(&value).is_err());
    value["artifacts"][0]["name"] = json!({"handkited": null});
    assert!(parse(&value).is_err());
}

#[test]
fn rejects_sequence_representations_of_objects() {
    for (pointer, keys) in [
        (
            "",
            vec![
                "schema_version",
                "source_sha",
                "target",
                "rust",
                "sdk",
                "artifacts",
            ],
        ),
        ("/rust", vec!["toolchain", "compiler_commit"]),
        ("/sdk", vec!["version", "build"]),
        (
            "/artifacts/0",
            vec!["name", "size", "sha256", "code_identifier"],
        ),
    ] {
        let mut value = fixture();
        let object = value.pointer_mut(pointer).unwrap();
        *object = Value::Array(keys.iter().map(|key| object[key].clone()).collect());
        assert!(parse(&value).is_err(), "array at {pointer}");
    }
}

#[test]
fn rejects_invalid_artifact_sizes_hashes_and_identifiers() {
    for (key, invalid) in [
        ("size", json!(0)),
        ("size", json!(-1)),
        ("size", json!(1.5)),
        ("size", json!("1")),
        ("size", json!(MAX_ARTIFACT_BYTES + 1)),
        ("size", json!(u64::MAX)),
        ("sha256", json!("a".repeat(63))),
        ("sha256", json!("a".repeat(65))),
        ("sha256", json!("A".repeat(64))),
        ("sha256", json!("g".repeat(64))),
        ("code_identifier", json!("com.handkite.handkitectl")),
        ("code_identifier", json!("com.handkite.other")),
        ("code_identifier", json!("com.handkite.handkited\n")),
    ] {
        let mut value = fixture();
        value["artifacts"][0][key] = invalid;
        assert!(parse(&value).is_err(), "invalid {key}");
    }
}

#[test]
fn rejects_paths_traversal_and_non_ascii_in_all_identity_fields() {
    for pointer in [
        "/source_sha",
        "/target",
        "/rust/toolchain",
        "/rust/compiler_commit",
        "/sdk/version",
        "/sdk/build",
        "/artifacts/0/name",
        "/artifacts/0/sha256",
        "/artifacts/0/code_identifier",
    ] {
        for invalid in [
            "../handkited",
            "..",
            "/handkited",
            "a/b",
            "a\\b",
            "\u{e9}",
            "",
            "a\0b",
        ] {
            let mut value = fixture();
            *value.pointer_mut(pointer).unwrap() = json!(invalid);
            assert!(parse(&value).is_err(), "{pointer}: {invalid:?}");
        }
    }
}

#[test]
fn bounds_json_and_rejects_trailing_data_and_malformed_input() {
    let mut bytes = FIXTURE.as_bytes().to_vec();
    bytes.resize(MAX_MANIFEST_BYTES, b' ');
    assert!(ReleaseManifest::parse(&bytes).is_ok());
    bytes.push(b' ');
    assert_eq!(
        ReleaseManifest::parse(&bytes).unwrap_err(),
        "release manifest exceeds 16 KiB"
    );
    for bytes in [
        b"".as_slice(),
        b"null",
        b"[]",
        b"{",
        b"\xff",
        format!("{FIXTURE}{{}}").as_bytes(),
    ] {
        assert!(ReleaseManifest::parse(bytes).is_err());
    }
}
