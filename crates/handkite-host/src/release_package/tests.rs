use super::*;
use crate::release_manifest::MAX_ARTIFACT_BYTES;
use serde_json::{Value, json};

pub(super) const CERTIFICATE: &str = "0123456789abcdef0123456789abcdef01234567";

pub(super) fn request_json() -> Value {
    json!({
        "source_sha": "a".repeat(40),
        "target": TARGET,
        "rust": { "toolchain": "1.95.0", "compiler_commit": "b".repeat(40) },
        "sdk": { "version": "26.0", "build": "25A1" },
        "candidates": (["handkited", "handkitectl", "handkite-mcp"].map(|name| json!({
            "name": name, "size": 64, "sha256": "c".repeat(64)
        })))
    })
}

fn parse(value: Value) -> Result<PackageRequest, &'static str> {
    PackageRequest::parse(&serde_json::to_vec(&value).unwrap(), CERTIFICATE)
}

#[test]
fn request_is_unsigned_inventory_with_explicit_build_and_signer_inputs() {
    let request = parse(request_json()).unwrap();
    assert_eq!(request.certificate_sha1(), CERTIFICATE);
    assert_eq!(request.candidates().source_sha(), "a".repeat(40));
    assert_eq!(request.candidates().artifacts().len(), 3);
}

#[test]
fn rejects_malformed_certificate_and_oversized_input() {
    let bytes = serde_json::to_vec(&request_json()).unwrap();
    for pin in [
        "".into(),
        "-".into(),
        "Default Identity".into(),
        CERTIFICATE.to_uppercase(),
        "g".repeat(40),
        format!("{CERTIFICATE}\n"),
    ] {
        assert!(PackageRequest::parse(&bytes, &pin).is_err());
    }
    assert!(PackageRequest::parse(&vec![b' '; MAX_REQUEST_BYTES + 1], CERTIFICATE).is_err());
}

#[test]
fn rejects_duplicate_unknown_missing_and_array_shaped_fields() {
    let value = request_json();
    let text = value.to_string();
    assert!(
        PackageRequest::parse(
            format!("{{\"target\":\"{TARGET}\",{}", &text[1..]).as_bytes(),
            CERTIFICATE
        )
        .is_err()
    );
    for location in ["", "/rust", "/sdk", "/candidates/0"] {
        let mut changed = value.clone();
        let target = changed.pointer_mut(location).unwrap();
        target
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), json!(true));
        assert!(parse(changed).is_err());
        let mut changed = value.clone();
        *changed.pointer_mut(location).unwrap() = json!([]);
        assert!(parse(changed).is_err());
    }
    for field in ["source_sha", "target", "rust", "sdk", "candidates"] {
        let mut changed = value.clone();
        changed.as_object_mut().unwrap().remove(field);
        assert!(parse(changed).is_err());
    }
}

#[test]
fn rejects_bad_build_identity_candidate_names_hashes_and_sizes() {
    for (pointer, replacement) in [
        ("/source_sha", json!("a")),
        ("/target", json!("x86_64-apple-darwin")),
        ("/rust/toolchain", json!("stable")),
        ("/sdk/build", json!("bad build")),
        ("/candidates/0/name", json!("../handkited")),
        ("/candidates/0/name", json!("handkitectl")),
        ("/candidates/0/sha256", json!("C".repeat(64))),
        ("/candidates/0/size", json!(0)),
        ("/candidates/0/size", json!(MAX_ARTIFACT_BYTES + 1)),
    ] {
        let mut changed = request_json();
        *changed.pointer_mut(pointer).unwrap() = replacement;
        assert!(parse(changed).is_err());
    }
}
