pub const LINK_THRESHOLD_DEFAULT: f32 = 0.30;
pub const IDENTICAL_REINSERTION_THRESHOLD: f32 = 0.90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceKind {
    Edit,
    Read,
    Tool,
    Message,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocationDelta {
    Same,
    Adjacent,
    Moved,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cardinality {
    OneToOne,
    OneToMany,
    ManyToOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredEdgeClass {
    Lineage,
    LocationOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceFragmentRef {
    pub tape_id: String,
    pub event_offset: u64,
    pub kind: EvidenceKind,
    pub file_path: String,
    pub timestamp: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpanEdge {
    pub from_anchor: String,
    pub to_anchor: String,
    pub confidence: f32,
    pub location_delta: LocationDelta,
    pub cardinality: Cardinality,
    pub agent_link: bool,
    pub note: Option<String>,
}

impl SpanEdge {
    pub fn stored_class(&self, link_threshold: f32) -> StoredEdgeClass {
        if self.agent_link || self.confidence >= link_threshold {
            StoredEdgeClass::Lineage
        } else {
            StoredEdgeClass::LocationOnly
        }
    }

    pub fn included_in_default_traversal(&self, min_confidence: f32) -> bool {
        self.agent_link || self.confidence >= min_confidence
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tombstone {
    pub anchor_hashes: Vec<String>,
    pub tape_id: String,
    pub event_offset: u64,
    pub file_path: String,
    pub range_at_deletion: FileRange,
    pub timestamp: String,
}

pub fn should_link_successor(similarity: f32, agent_link: bool, link_threshold: f32) -> bool {
    agent_link || similarity >= link_threshold
}

pub fn should_link_identical_reinsertion(similarity: f32) -> bool {
    similarity >= IDENTICAL_REINSERTION_THRESHOLD
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_edge(confidence: f32, agent_link: bool) -> SpanEdge {
        SpanEdge {
            from_anchor: "a".to_string(),
            to_anchor: "b".to_string(),
            confidence,
            location_delta: LocationDelta::Moved,
            cardinality: Cardinality::OneToOne,
            agent_link,
            note: None,
        }
    }

    #[test]
    fn below_link_threshold_is_location_only_without_agent_link() {
        let edge = sample_edge(0.29, false);
        assert_eq!(
            edge.stored_class(LINK_THRESHOLD_DEFAULT),
            StoredEdgeClass::LocationOnly
        );
    }

    #[test]
    fn at_link_threshold_is_lineage() {
        let edge = sample_edge(0.30, false);
        assert_eq!(
            edge.stored_class(LINK_THRESHOLD_DEFAULT),
            StoredEdgeClass::Lineage
        );
    }

    #[test]
    fn agent_link_overrides_low_confidence_for_storage_and_traversal() {
        let mut edge = sample_edge(0.01, true);
        edge.location_delta = LocationDelta::Absent;
        edge.cardinality = Cardinality::OneToMany;
        edge.note = Some("explicit successor".to_string());
        assert_eq!(
            edge.stored_class(LINK_THRESHOLD_DEFAULT),
            StoredEdgeClass::Lineage
        );
        assert!(edge.included_in_default_traversal(0.50));
    }

    #[test]
    fn identical_reinsertion_threshold_is_inclusive() {
        assert!(should_link_identical_reinsertion(0.90));
        assert!(!should_link_identical_reinsertion(0.89));
    }

    #[test]
    fn should_link_successor_is_inclusive_at_threshold() {
        assert!(should_link_successor(0.30, false, LINK_THRESHOLD_DEFAULT));
        assert!(!should_link_successor(0.29, false, LINK_THRESHOLD_DEFAULT));
    }

    #[test]
    fn traversal_threshold_is_inclusive_without_agent_link() {
        let at_threshold = sample_edge(0.50, false);
        let below_threshold = sample_edge(0.49, false);
        assert!(at_threshold.included_in_default_traversal(0.50));
        assert!(!below_threshold.included_in_default_traversal(0.50));
    }

    #[test]
    fn tombstone_captures_required_deletion_facts() {
        let tombstone = Tombstone {
            anchor_hashes: vec!["h1".to_string(), "h2".to_string()],
            tape_id: "tape-123".to_string(),
            event_offset: 42,
            file_path: "src/lib.rs".to_string(),
            range_at_deletion: FileRange { start: 10, end: 20 },
            timestamp: "2026-02-22T00:00:00Z".to_string(),
        };

        assert_eq!(tombstone.anchor_hashes.len(), 2);
        assert_eq!(tombstone.tape_id, "tape-123");
        assert_eq!(tombstone.event_offset, 42);
        assert_eq!(tombstone.file_path, "src/lib.rs");
        assert_eq!(tombstone.range_at_deletion.start, 10);
        assert_eq!(tombstone.range_at_deletion.end, 20);
        assert_eq!(tombstone.timestamp, "2026-02-22T00:00:00Z");
    }
}
