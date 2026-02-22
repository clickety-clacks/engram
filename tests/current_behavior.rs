use engram::index::lineage::{
    Cardinality, LINK_THRESHOLD_DEFAULT, LocationDelta, SpanEdge, StoredEdgeClass, Tombstone,
    should_link_identical_reinsertion, should_link_successor,
};
use engram::query::explain::{ExplainTraversal, PrettyConfidenceTier, pretty_tier};
use engram::tape::event::FileRange;

fn edge(confidence: f32, agent_link: bool) -> SpanEdge {
    SpanEdge {
        from_anchor: "from".to_string(),
        to_anchor: "to".to_string(),
        confidence,
        location_delta: LocationDelta::Same,
        cardinality: Cardinality::OneToOne,
        agent_link,
        note: None,
    }
}

#[test]
fn linkage_and_query_defaults_work_together_at_boundaries() {
    let link_edge = edge(0.30, false);
    assert_eq!(
        link_edge.stored_class(LINK_THRESHOLD_DEFAULT),
        StoredEdgeClass::Lineage
    );
    assert!(!link_edge.included_in_default_traversal(0.50));
    assert_eq!(pretty_tier(0.30, false, false), PrettyConfidenceTier::Hidden);

    let default_traversal = ExplainTraversal::default();
    assert_eq!(default_traversal.min_confidence, 0.50);
    assert_eq!(default_traversal.max_fanout, 50);
    assert_eq!(default_traversal.max_edges, 500);
    assert_eq!(default_traversal.max_depth, 10);
}

#[test]
fn agent_link_and_location_only_behavior_are_distinct() {
    let low_conf_linked = edge(0.01, true);
    assert_eq!(
        low_conf_linked.stored_class(LINK_THRESHOLD_DEFAULT),
        StoredEdgeClass::Lineage
    );
    assert!(low_conf_linked.included_in_default_traversal(0.50));
    assert_eq!(
        pretty_tier(0.01, true, true),
        PrettyConfidenceTier::ForensicsOnly
    );
}

#[test]
fn reinsertion_threshold_and_tombstone_data_shape() {
    assert!(!should_link_identical_reinsertion(0.89));
    assert!(should_link_identical_reinsertion(0.90));
    assert!(!should_link_successor(0.29, false, LINK_THRESHOLD_DEFAULT));
    assert!(should_link_successor(0.30, false, LINK_THRESHOLD_DEFAULT));

    let deleted = Tombstone {
        anchor_hashes: vec!["a1".to_string()],
        tape_id: "tape".to_string(),
        event_offset: 9,
        file_path: "src/main.rs".to_string(),
        range_at_deletion: FileRange { start: 1, end: 1 },
        timestamp: "2026-02-22T00:00:00Z".to_string(),
    };
    assert_eq!(deleted.anchor_hashes, vec!["a1".to_string()]);
    assert_eq!(deleted.file_path, "src/main.rs");
}
