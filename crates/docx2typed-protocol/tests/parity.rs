//! Canonical-JSON parity tests: the expected digests were computed with the
//! Python Reference (`scripts/protocol.py` `semantic_sha256`) so any drift
//! in key ordering, separators, or escaping breaks the test.

use serde_json::json;

#[test]
fn semantic_sha256_matches_python_canonical_json() {
    let cases = [
        (
            json!({"operation": "build", "args": {"output": null, "workdir": "C:\\x"}}),
            "211be05b3f57bdcb686ee9f30cb887863dafd542eac4e46d250fdb78b7a7d659",
        ),
        (
            json!({"b": [1, 2, {"c": "d"}], "a": "文本\u{e9}"}),
            "5f8586a191199bd5266750ea3e72cd2c8a82ada6e6c0f3b6ae60e8a033865f5f",
        ),
        (
            json!({"z": null, "a": true, "n": 0}),
            "b8763b8e3df496246a56ad46c84ccbd3baf9aba12b4492118033883573a32173",
        ),
    ];
    for (value, expected) in cases {
        assert_eq!(docx2typed_protocol::semantic_sha256(&value), expected);
    }
}

#[test]
fn semantic_sha256_is_key_sorted_and_compact() {
    let value = json!({"b": 1, "a": 2});
    assert_eq!(
        docx2typed_protocol::canonical_json(&value),
        r#"{"a":2,"b":1}"#
    );
}

#[test]
fn descriptor_contracts_match_frozen_ranges() {
    let descriptor = docx2typed_protocol::engine_descriptor("test-commit");
    assert_eq!(descriptor.schema, "docx2typed-engine-descriptor-1");
    assert_eq!(descriptor.name, "docx2typed-rust");
    assert_eq!(descriptor.version, "0.2.0");
    assert_eq!(descriptor.build_commit, "test-commit");
    let contracts = descriptor.contracts.as_object().expect("contracts object");
    for name in ["cli", "mcp", "result", "evidence", "workdir"] {
        let range = &contracts[name];
        assert_eq!(range["major"], 1);
        assert_eq!(range["min_minor"], 0);
        assert_eq!(range["max_minor"], 0);
    }
    assert_eq!(
        descriptor.schema_bundle["sha256"].as_str().unwrap(),
        "8d1e8dbf2778e31cc6aa838e1ccae642c0481ff03719a996f439cf939e378d84"
    );
    assert_eq!(
        descriptor.capability_manifest["sha256"].as_str().unwrap(),
        "d911b616e2238ccc2dcd8b4dd7e170f3feff01ac2fb412ce3752bc7ec77baf37"
    );
}

#[test]
fn negotiation_accepts_frozen_client_and_rejects_wrong_major() {
    let frozen: serde_json::Value = serde_json::json!({
        "cli": {"major": 1, "min_minor": 0, "max_minor": 0},
        "mcp": {"major": 1, "min_minor": 0, "max_minor": 0},
        "result": {"major": 1, "min_minor": 0, "max_minor": 0},
        "evidence": {"major": 1, "min_minor": 0, "max_minor": 0},
        "workdir": {"major": 1, "min_minor": 0, "max_minor": 0},
    });
    assert!(docx2typed_protocol::negotiate(Some(&frozen), None, None).is_ok());

    let wrong_major = serde_json::json!({"cli": {"major": 2, "min_minor": 0, "max_minor": 0}});
    let error = docx2typed_protocol::negotiate(Some(&wrong_major), None, None);
    assert!(matches!(
        error,
        Err(docx2typed_protocol::NegotiationError::ContractIncompatible { .. })
    ));

    let missing_feature =
        docx2typed_protocol::negotiate(None, Some(&["hybrid-fidelity".to_string()]), None);
    assert!(matches!(
        missing_feature,
        Err(docx2typed_protocol::NegotiationError::RequiredFeatureUnsupported {
            missing_features
        }) if missing_features == vec!["locked-structure", "typed-mode"]
    ));
}

#[test]
fn diagnostic_shape_matches_frozen_registry() {
    let diagnostic = docx2typed_protocol::Diagnostic::new(
        "input-not-found",
        "source file not found: x".to_string(),
    );
    let value = serde_json::to_value(&diagnostic).expect("serializes");
    assert_eq!(value["schema"], "docx2typed-diagnostic-1");
    assert_eq!(value["severity"], "error");
    assert_eq!(value["category"], "input");
    assert_eq!(value["retriable"], false);
}
