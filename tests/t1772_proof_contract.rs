use std::path::Path;

use engram::proof::t1772::{
    CANDIDATE_BASE, CANONICAL_BYTES_CONTRACT, DISPATCH_ROWS, INPUT_ROOT, MANIFEST_SHA256,
    PROOF_ROOT, canonical_json_lf, safe_relative_path, sha256_bytes,
};
use serde_json::json;

#[test]
fn separate_probes_report_real_sort_and_posting_counters_with_identical_binding() {
    use engram::proof::statement_probe::{COUNTER_SCOPE, run};
    use engram::proof::t1772::sha256_file;
    let dir = tempfile::tempdir().unwrap();
    for variant in ["baseline", "candidate"] {
        let db = dir.path().join(format!("{variant}.sqlite"));
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute_batch("CREATE TABLE edges(edge_id INTEGER PRIMARY KEY, from_anchor TEXT, to_anchor TEXT, confidence REAL, location_delta TEXT, cardinality TEXT, agent_link INTEGER, note TEXT);").unwrap();
        if variant == "baseline" {
            conn.execute_batch("PRAGMA user_version=3;
                CREATE TABLE evidence(anchor TEXT,tape_id TEXT,event_offset INTEGER,kind TEXT,file_path TEXT,timestamp TEXT);
                CREATE INDEX evidence_anchor ON evidence(anchor);
                INSERT INTO evidence VALUES('winnow:a','t',2,'read','/x','b'),('winnow:a','t',1,'read','/x','a');").unwrap();
        } else {
            conn.execute_batch("PRAGMA user_version=4;
                CREATE TABLE evidence_windows(evidence_id INTEGER PRIMARY KEY,anchor TEXT,tape_id TEXT,event_offset INTEGER,kind TEXT,file_path TEXT,timestamp TEXT);
                CREATE TABLE evidence_features(feature_hash TEXT,evidence_id INTEGER,PRIMARY KEY(feature_hash,evidence_id));
                INSERT INTO evidence_windows VALUES(1,'winnow:a,b','t',1,'read','/x','a'),(2,'winnow:a,c','t',2,'read','/x','b');
                INSERT INTO evidence_features VALUES('winnow:a',1),('winnow:a',2);").unwrap();
        }
        drop(conn);
        let before = sha256_file(&db).unwrap();
        let binding = json!({"query_id":"fixture", "variant":variant,"cache_class":"hot",
            "phase":"measured","iteration":0,"order_in_pair":0,"database_sha256":before,
            "derived_anchors":["winnow:a"],"flags":{"depth":10,"max_edges":500,"max_fanout":50,
                "min_confidence":0.5,"forensics":false,"include_deleted":false,"pretty":false}});
        let output = dir.path().join(format!("{variant}.json"));
        let report = run(&db, &binding, &output).unwrap();
        assert_eq!(report["binding"], binding);
        assert_eq!(report["counter_scope"], COUNTER_SCOPE);
        assert_eq!(report["direct_rows_visited"], 2);
        assert_eq!(report["direct_touch_autoindex"], 0);
        assert_eq!(
            report["direct_touch_sort"],
            if variant == "baseline" { 1 } else { 0 }
        );
        assert_eq!(
            report["posting_rows_visited"],
            if variant == "baseline" { 0 } else { 4 }
        );
        for row in report["statements"].as_array().unwrap() {
            assert!(row["sort"].is_u64() && row["autoindex"].is_u64());
            assert!(row["rows_visited"].is_u64() && row["vm_steps"].is_u64());
        }
        assert_eq!(sha256_file(&db).unwrap(), before);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(output).unwrap()).unwrap(),
            report
        );
    }
}

#[test]
fn performance_order_alternates_by_query_and_iteration() {
    use engram::proof::performance::alternating_order;
    let mut launches = [0, 0];
    for query in 0..12 {
        for _mode in 0..2 {
            for iteration in 0..33 {
                let order = alternating_order(query, iteration);
                assert_ne!(order, alternating_order(query, iteration + 1));
                assert_ne!(order, alternating_order(query + 1, iteration));
                for variant in order {
                    launches[variant] += 1;
                }
            }
        }
    }
    assert_eq!(launches, [792, 792]);
}

#[test]
fn thirty_sample_tail_percentiles_use_nearest_rank() {
    use engram::proof::measurement::percentiles;
    let samples = (1..=30).rev().collect::<Vec<u64>>();
    let result = percentiles(&samples).unwrap();
    assert_eq!(result["p50"], 15);
    assert_eq!(result["p95"], 29);
    assert_eq!(result["p99"], 30);
    assert!(percentiles(&[]).is_err());
}

#[test]
fn darwin_rss_requires_one_unambiguous_observation() {
    use engram::proof::performance::parse_darwin_rss;
    assert_eq!(
        parse_darwin_rss("noise\n  4096  maximum resident set size\n").unwrap(),
        4096
    );
    assert!(parse_darwin_rss("no observation").is_err());
    assert!(
        parse_darwin_rss("4 maximum resident set size\n8 maximum resident set size\n").is_err()
    );
}

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
