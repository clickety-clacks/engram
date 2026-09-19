use std::path::Path;

use engram::proof::t1772::{
    CANDIDATE_BASE, CANONICAL_BYTES_CONTRACT, DISPATCH_ROWS, INPUT_ROOT, MANIFEST_SHA256,
    PROOF_ROOT, canonical_json_lf, safe_relative_path, sha256_bytes,
};
use serde_json::json;

#[test]
fn canonical_json_is_sorted_compact_utf8_with_one_lf() {
    let value = json!({
        "z": {"two": 2, "one": 1},
        "a": [true, null, "é"]
    });
    let bytes = canonical_json_lf(&value).expect("canonical bytes");
    assert_eq!(
        bytes,
        b"{\"a\":[true,null,\"\xc3\xa9\"],\"z\":{\"one\":1,\"two\":2}}\n"
    );
    assert_eq!(
        sha256_bytes(&bytes),
        "af9995d157d9f079dfda9b54f52eb02b84b4009dd1b7ce1e46ca57bf01581ab2"
    );
    assert!(CANONICAL_BYTES_CONTRACT.contains("exactly one LF"));
}

#[test]
fn manifest_paths_cannot_escape_the_input_root() {
    assert_eq!(
        safe_relative_path("p0-tape-ids.txt").expect("simple relative path"),
        Path::new("p0-tape-ids.txt")
    );
    for invalid in ["/absolute", "../escape", "a/../escape", "./same"] {
        assert!(safe_relative_path(invalid).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn reviewed_r29_bindings_are_compile_time_constants() {
    assert_eq!(CANDIDATE_BASE, "9f8afc65d0b365444446a473c04389adf80bd4b3");
    assert_eq!(
        INPUT_ROOT,
        "/Users/mike/shared-workspace/engram/proofs/t1772/inputs"
    );
    assert_eq!(
        PROOF_ROOT,
        "/Users/mike/shared-workspace/engram/proofs/t1772/final-staging-asg-b260c05f-r27"
    );
    assert_eq!(
        MANIFEST_SHA256,
        "0d279da30da8b118ae1b4433c86c78736ddda8d2ae6f2d8c00ecfcb1b47fd89a"
    );
    assert_eq!(DISPATCH_ROWS, 14_369);
}

#[cfg(target_os = "macos")]
#[test]
fn darwin_sigcont_is_19() {
    assert_eq!(engram::proof::t1772::SIGCONT_NUMBER, 19);
}
