//! Round-trip checks against the shared protocol vectors in `docs/protocol-vectors.json`.
//!
//! The shared vectors - not this crate's types - are the source of truth for the wire
//! (see docs/protocol.md). Every `vectors[]` frame must round-trip through `Frame` with
//! the omit-None contract; every `must_reject[]` frame must fail to deserialize.
//! serum runs the same assertions over its copy of the vectors; see docs/protocol.md.

use serde_json::Value;
use serum_proxy::envelope::Frame;

fn vectors_file() -> Value {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/protocol-vectors.json");
    let raw = std::fs::read_to_string(path).expect("protocol-vectors.json is readable");
    serde_json::from_str(&raw).expect("protocol-vectors.json is valid JSON")
}

#[test]
fn vectors_file_targets_protocol_version_1() {
    assert_eq!(vectors_file()["protocol_version"], 1);
}

#[test]
fn vectors_round_trip() {
    let c = vectors_file();
    let vectors = c["vectors"].as_array().expect("vectors is an array");
    assert!(!vectors.is_empty(), "vectors file has at least one vector");

    for v in vectors {
        let desc = v["description"].as_str().unwrap_or("<no description>");
        let original = &v["frame"];

        let frame: Frame = serde_json::from_value(original.clone())
            .unwrap_or_else(|e| panic!("vector failed to deserialize [{desc}]: {e}"));
        let reserialized = serde_json::to_value(&frame)
            .unwrap_or_else(|e| panic!("vector failed to reserialize [{desc}]: {e}"));

        assert_eq!(
            &reserialized, original,
            "vector did not round-trip to an equal value [{desc}]"
        );
    }
}

#[test]
fn must_reject_frames_fail_to_parse() {
    let c = vectors_file();
    let rejects = c["must_reject"]
        .as_array()
        .expect("must_reject is an array");
    assert!(
        !rejects.is_empty(),
        "vectors file has at least one rejection vector"
    );

    for v in rejects {
        let desc = v["description"].as_str().unwrap_or("<no description>");
        let frame = v["frame"].clone();
        let parsed: Result<Frame, _> = serde_json::from_value(frame);
        assert!(
            parsed.is_err(),
            "must_reject vector unexpectedly parsed [{desc}]"
        );
    }
}
