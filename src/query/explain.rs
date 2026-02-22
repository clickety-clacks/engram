use std::collections::{HashSet, VecDeque};

use crate::index::{EdgeRow, SqliteIndex};
use crate::index::lineage::EvidenceFragmentRef;

pub const MIN_CONFIDENCE_DEFAULT: f32 = 0.50;
pub const MAX_FANOUT_DEFAULT: usize = 50;
pub const MAX_EDGES_DEFAULT: usize = 500;
pub const MAX_DEPTH_DEFAULT: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExplainTraversal {
    pub min_confidence: f32,
    pub max_fanout: usize,
    pub max_edges: usize,
    pub max_depth: usize,
}

impl Default for ExplainTraversal {
    fn default() -> Self {
        Self {
            min_confidence: MIN_CONFIDENCE_DEFAULT,
            max_fanout: MAX_FANOUT_DEFAULT,
            max_edges: MAX_EDGES_DEFAULT,
            max_depth: MAX_DEPTH_DEFAULT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrettyConfidenceTier {
    Edit,
    Move,
    Related,
    Hidden,
    ForensicsOnly,
}

pub fn pretty_tier(confidence: f32, moved: bool, location_only: bool) -> PrettyConfidenceTier {
    if location_only {
        return PrettyConfidenceTier::ForensicsOnly;
    }
    if confidence >= 0.90 && !moved {
        PrettyConfidenceTier::Edit
    } else if confidence >= 0.85 && moved {
        PrettyConfidenceTier::Move
    } else if confidence >= MIN_CONFIDENCE_DEFAULT {
        PrettyConfidenceTier::Related
    } else {
        PrettyConfidenceTier::Hidden
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExplainResult {
    pub direct: Vec<EvidenceFragmentRef>,
    pub lineage: Vec<EdgeRow>,
    pub touched_anchors: Vec<String>,
}

pub fn retrieve_direct(index: &SqliteIndex, anchors: &[String]) -> rusqlite::Result<Vec<EvidenceFragmentRef>> {
    let mut all = Vec::new();
    for anchor in anchors {
        all.extend(index.evidence_for_anchor(anchor)?);
    }
    Ok(all)
}

pub fn retrieve_lineage(
    index: &SqliteIndex,
    anchors: &[String],
    traversal: ExplainTraversal,
    include_forensics: bool,
) -> rusqlite::Result<Vec<EdgeRow>> {
    let mut queue: VecDeque<(String, usize)> = anchors.iter().cloned().map(|a| (a, 0)).collect();
    let mut visited = HashSet::new();
    let mut out = Vec::new();

    while let Some((anchor, depth)) = queue.pop_front() {
        if !visited.insert(anchor.clone()) {
            continue;
        }
        if out.len() >= traversal.max_edges {
            break;
        }
        if depth >= traversal.max_depth {
            continue;
        }
        let edges = index.inbound_edges(&anchor, traversal.min_confidence, include_forensics)?;
        for edge in edges.into_iter().take(traversal.max_fanout) {
            if out.len() >= traversal.max_edges {
                break;
            }
            if !visited.contains(&edge.from_anchor) {
                queue.push_back((edge.from_anchor.clone(), depth + 1));
            }
            out.push(edge);
        }
    }

    Ok(out)
}

pub fn explain_by_anchor(
    index: &SqliteIndex,
    anchors: &[String],
    traversal: ExplainTraversal,
    include_forensics: bool,
) -> rusqlite::Result<ExplainResult> {
    let direct = retrieve_direct(index, anchors)?;
    let lineage = retrieve_lineage(index, anchors, traversal, include_forensics)?;
    let mut seen = HashSet::new();
    let mut touched_anchors = anchors.to_vec();
    for anchor in anchors {
        seen.insert(anchor.clone());
    }
    for edge in &lineage {
        if seen.insert(edge.from_anchor.clone()) {
            touched_anchors.push(edge.from_anchor.clone());
        }
        if seen.insert(edge.to_anchor.clone()) {
            touched_anchors.push(edge.to_anchor.clone());
        }
    }
    Ok(ExplainResult {
        direct,
        lineage,
        touched_anchors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::lineage::LINK_THRESHOLD_DEFAULT;
    use crate::index::lineage::{Cardinality, LocationDelta, SpanEdge};
    use crate::tape::event::{CodeEditEvent, FileRange, TapeEvent, TapeEventAt, TapeEventData};

    #[test]
    fn explain_collects_direct_and_backward_lineage_edges() {
        let index = SqliteIndex::open_in_memory().expect("sqlite");
        let events = vec![TapeEventAt {
            offset: 0,
            event: TapeEvent {
                timestamp: "2026-02-22T00:00:00Z".to_string(),
                data: TapeEventData::CodeEdit(CodeEditEvent {
                    file: "src/lib.rs".to_string(),
                    before_range: Some(FileRange { start: 1, end: 1 }),
                    after_range: Some(FileRange { start: 1, end: 1 }),
                    before_hash: Some("a".to_string()),
                    after_hash: Some("b".to_string()),
                    similarity: Some(0.80),
                }),
            },
        }];

        index
            .ingest_tape_events("tape", &events, LINK_THRESHOLD_DEFAULT)
            .expect("ingest");

        let result = explain_by_anchor(
            &index,
            &["b".to_string()],
            ExplainTraversal::default(),
            true,
        )
        .expect("explain");

        assert_eq!(result.direct.len(), 1);
        assert_eq!(result.direct[0].tape_id, "tape");

        let lineage = explain_by_anchor(
            &index,
            &["b".to_string()],
            ExplainTraversal::default(),
            true,
        )
        .expect("explain from successor");
        assert_eq!(lineage.lineage.len(), 1);
        assert_eq!(lineage.lineage[0].from_anchor, "a");
        assert_eq!(lineage.lineage[0].to_anchor, "b");
        assert!(lineage.touched_anchors.contains(&"a".to_string()));
    }

    #[test]
    fn lineage_traversal_honors_max_depth() {
        let index = SqliteIndex::open_in_memory().expect("sqlite");
        index
            .insert_edge(
                &SpanEdge {
                    from_anchor: "a".to_string(),
                    to_anchor: "b".to_string(),
                    confidence: 0.90,
                    location_delta: LocationDelta::Moved,
                    cardinality: Cardinality::OneToOne,
                    agent_link: false,
                    note: None,
                },
                LINK_THRESHOLD_DEFAULT,
            )
            .expect("insert edge a->b");
        index
            .insert_edge(
                &SpanEdge {
                    from_anchor: "b".to_string(),
                    to_anchor: "c".to_string(),
                    confidence: 0.90,
                    location_delta: LocationDelta::Moved,
                    cardinality: Cardinality::OneToOne,
                    agent_link: false,
                    note: None,
                },
                LINK_THRESHOLD_DEFAULT,
            )
            .expect("insert edge b->c");

        let lineage = retrieve_lineage(
            &index,
            &["c".to_string()],
            ExplainTraversal {
                max_depth: 1,
                ..ExplainTraversal::default()
            },
            false,
        )
        .expect("retrieve lineage");

        assert_eq!(lineage.len(), 1);
        assert_eq!(lineage[0].from_anchor, "b");
        assert_eq!(lineage[0].to_anchor, "c");
    }
}
