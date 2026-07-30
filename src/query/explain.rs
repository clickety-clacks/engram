use std::collections::{HashSet, VecDeque};

use crate::index::lineage::EvidenceFragmentRef;
use crate::index::{EdgeRow, SqliteIndex};

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

pub fn retrieve_direct(
    index: &SqliteIndex,
    anchors: &[String],
) -> rusqlite::Result<Vec<EvidenceFragmentRef>> {
    index.evidence_for_anchors(anchors)
}

pub fn retrieve_lineage(
    index: &SqliteIndex,
    anchors: &[String],
    traversal: ExplainTraversal,
    include_forensics: bool,
) -> rusqlite::Result<Vec<EdgeRow>> {
    let mut queue: VecDeque<(String, usize)> =
        anchors.iter().cloned().map(|anchor| (anchor, 0)).collect();
    let mut visited = HashSet::new();
    let mut seen_edges = HashSet::new();
    let mut out = Vec::new();

    while let Some((anchor, depth)) = queue.pop_front() {
        if !visited.insert(anchor.clone()) {
            continue;
        }
        if depth >= traversal.max_depth {
            continue;
        }
        if out.len() >= traversal.max_edges {
            break;
        }
        let mut edges =
            index.inbound_edges(&anchor, traversal.min_confidence, include_forensics)?;
        edges.extend(index.outbound_edges(&anchor, traversal.min_confidence, include_forensics)?);
        let mut candidate_edges = HashSet::new();
        edges.retain(|edge| {
            let key = edge_key(edge);
            !seen_edges.contains(&key) && candidate_edges.insert(key)
        });
        edges.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        for edge in edges.into_iter().take(traversal.max_fanout) {
            if out.len() >= traversal.max_edges {
                break;
            }
            seen_edges.insert(edge_key(&edge));
            let next = if edge.from_anchor == anchor {
                &edge.to_anchor
            } else {
                &edge.from_anchor
            };
            if !visited.contains(next) {
                queue.push_back((next.clone(), depth + 1));
            }
            out.push(edge);
        }
    }

    Ok(out)
}

pub(crate) fn edge_key(
    edge: &EdgeRow,
) -> (
    String,
    String,
    u32,
    crate::index::lineage::LocationDelta,
    crate::index::lineage::Cardinality,
    bool,
    Option<String>,
) {
    (
        edge.from_anchor.clone(),
        edge.to_anchor.clone(),
        edge.confidence.to_bits(),
        edge.location_delta,
        edge.cardinality,
        edge.agent_link,
        edge.note.clone(),
    )
}

pub fn explain_by_anchor(
    index: &SqliteIndex,
    anchors: &[String],
    traversal: ExplainTraversal,
    include_forensics: bool,
) -> rusqlite::Result<ExplainResult> {
    let direct = retrieve_direct(index, anchors)?;
    let mut traversal_anchors = Vec::new();
    let mut seen = HashSet::new();
    for anchor in anchors {
        for matched in index.matching_window_anchors(anchor)? {
            if seen.insert(matched.clone()) {
                traversal_anchors.push(matched);
            }
        }
    }
    let lineage = retrieve_lineage(index, &traversal_anchors, traversal, include_forensics)?;
    let mut seen = HashSet::new();
    let mut touched_anchors = traversal_anchors.clone();
    for anchor in &traversal_anchors {
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

pub fn explain_across_indexes_by_anchor(
    indexes: &[SqliteIndex],
    anchors: &[String],
    traversal: ExplainTraversal,
    include_forensics: bool,
) -> rusqlite::Result<ExplainResult> {
    let mut direct = Vec::new();
    let mut seen_direct = HashSet::new();
    let mut roots = Vec::new();
    let mut seen_roots = HashSet::new();

    for index in indexes {
        for fragment in retrieve_direct(index, anchors)? {
            let key = (
                fragment.tape_id.clone(),
                fragment.event_offset,
                fragment.kind as u8,
                fragment.file_path.clone(),
                fragment.timestamp.clone(),
            );
            if seen_direct.insert(key) {
                direct.push(fragment);
            }
        }
        for anchor in anchors {
            for matched in index.matching_window_anchors(anchor)? {
                if seen_roots.insert(matched.clone()) {
                    roots.push(matched);
                }
            }
        }
    }

    let mut queue: VecDeque<(String, usize)> =
        roots.iter().cloned().map(|anchor| (anchor, 0)).collect();
    let mut visited = HashSet::new();
    let mut seen_edges = HashSet::new();
    let mut lineage = Vec::new();
    while let Some((anchor, depth)) = queue.pop_front() {
        if !visited.insert(anchor.clone()) || depth >= traversal.max_depth {
            continue;
        }
        if lineage.len() >= traversal.max_edges {
            break;
        }
        let mut candidates = Vec::new();
        for index in indexes {
            candidates.extend(index.inbound_edges(
                &anchor,
                traversal.min_confidence,
                include_forensics,
            )?);
            candidates.extend(index.outbound_edges(
                &anchor,
                traversal.min_confidence,
                include_forensics,
            )?);
        }
        let mut candidate_edges = HashSet::new();
        candidates.retain(|edge| {
            let key = edge_key(edge);
            !seen_edges.contains(&key) && candidate_edges.insert(key)
        });
        candidates.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        for edge in candidates.into_iter().take(traversal.max_fanout) {
            if lineage.len() >= traversal.max_edges {
                break;
            }
            seen_edges.insert(edge_key(&edge));
            let next = if edge.from_anchor == anchor {
                &edge.to_anchor
            } else {
                &edge.from_anchor
            };
            if !visited.contains(next) {
                queue.push_back((next.clone(), depth + 1));
            }
            lineage.push(edge);
        }
    }

    let mut touched_anchors = roots;
    let mut seen_anchors = touched_anchors.iter().cloned().collect::<HashSet<_>>();
    for edge in &lineage {
        if seen_anchors.insert(edge.from_anchor.clone()) {
            touched_anchors.push(edge.from_anchor.clone());
        }
        if seen_anchors.insert(edge.to_anchor.clone()) {
            touched_anchors.push(edge.to_anchor.clone());
        }
    }
    direct.sort_by(|a, b| {
        a.timestamp
            .cmp(&b.timestamp)
            .then_with(|| a.tape_id.cmp(&b.tape_id))
            .then_with(|| a.event_offset.cmp(&b.event_offset))
    });
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
    use crate::index::{EdgeSource, EdgeSourceKind};
    use crate::tape::event::{CodeEditEvent, FileRange, TapeEvent, TapeEventAt, TapeEventData};

    fn code(prefix: &str) -> String {
        (1..=24)
            .map(|line| format!("fn {prefix}_{line}() {{ value_{line}(); }}\n"))
            .collect()
    }

    fn edit(offset: u64, before: &str, after: &str) -> TapeEventAt {
        TapeEventAt {
            offset,
            event: TapeEvent {
                timestamp: format!("2026-02-22T00:00:{offset:02}Z"),
                data: TapeEventData::CodeEdit(CodeEditEvent {
                    file: "src/lib.rs".to_string(),
                    before_range: Some(FileRange { start: 1, end: 24 }),
                    after_range: Some(FileRange { start: 1, end: 24 }),
                    before_text: Some(before.to_string()),
                    after_text: Some(after.to_string()),
                    before_hash: None,
                    after_hash: None,
                    before_anchor_hashes: Vec::new(),
                    after_anchor_hashes: Vec::new(),
                    similarity: None,
                }),
            },
        }
    }

    #[test]
    fn explain_collects_direct_and_lineage_edges() {
        let index = SqliteIndex::open_in_memory().expect("sqlite");
        let before = code("before");
        let after = code("after");
        let events = vec![edit(0, &before, &after)];

        index
            .ingest_tape_events("tape", &events, LINK_THRESHOLD_DEFAULT)
            .expect("ingest");

        let after_window = crate::anchor::fingerprint_windows(&after).remove(0);
        let before_anchor = crate::anchor::fingerprint_windows(&before).remove(0).anchor;
        let result = explain_by_anchor(
            &index,
            &[after_window.features[0].clone()],
            ExplainTraversal::default(),
            true,
        )
        .expect("explain");

        assert_eq!(result.direct.len(), 1);
        assert_eq!(result.direct[0].tape_id, "tape");

        assert_eq!(result.lineage.len(), 1);
        assert!(result.touched_anchors.contains(&before_anchor));
    }

    #[test]
    fn lineage_traversal_respects_depth_limit() {
        let index = SqliteIndex::open_in_memory().expect("sqlite");
        let a = code("a");
        let b = code("b");
        let c = code("c");
        let events = vec![edit(0, &a, &b), edit(1, &b, &c)];
        index
            .ingest_tape_events("tape", &events, LINK_THRESHOLD_DEFAULT)
            .expect("ingest");
        let c_anchor = crate::anchor::fingerprint_windows(&c).remove(0).anchor;

        let one_hop = explain_by_anchor(
            &index,
            std::slice::from_ref(&c_anchor),
            ExplainTraversal {
                min_confidence: 0.0,
                max_fanout: 50,
                max_edges: 500,
                max_depth: 1,
            },
            true,
        )
        .expect("explain one hop");
        assert_eq!(one_hop.lineage.len(), 1);

        let two_hops = explain_by_anchor(
            &index,
            std::slice::from_ref(&c_anchor),
            ExplainTraversal {
                min_confidence: 0.0,
                max_fanout: 50,
                max_edges: 500,
                max_depth: 2,
            },
            true,
        )
        .expect("explain two hops");
        assert_eq!(two_hops.lineage.len(), 2);

        let one_hop_lineage_only = retrieve_lineage(
            &index,
            &[c_anchor],
            ExplainTraversal {
                max_depth: 1,
                ..ExplainTraversal::default()
            },
            true,
        )
        .expect("retrieve lineage");
        assert_eq!(one_hop_lineage_only.len(), 1);
    }

    #[test]
    fn lineage_traversal_honors_max_depth_with_explicit_edges() {
        let index = SqliteIndex::open_in_memory().expect("sqlite");
        index
            .insert_edge(
                &EdgeSource {
                    source_kind: EdgeSourceKind::SpanLink,
                    tape_id: "tape".to_string(),
                    event_offset: 1,
                    pair_ordinal: 0,
                    from_window_ordinal: -1,
                    to_window_ordinal: -1,
                },
                &SpanEdge {
                    from_anchor: "span:a:1-1".to_string(),
                    to_anchor: "span:b:1-1".to_string(),
                    confidence: 0.90,
                    location_delta: LocationDelta::Moved,
                    cardinality: Cardinality::OneToOne,
                    agent_link: false,
                    note: None,
                },
            )
            .expect("insert edge a->b");
        index
            .insert_edge(
                &EdgeSource {
                    source_kind: EdgeSourceKind::SpanLink,
                    tape_id: "tape".to_string(),
                    event_offset: 2,
                    pair_ordinal: 0,
                    from_window_ordinal: -1,
                    to_window_ordinal: -1,
                },
                &SpanEdge {
                    from_anchor: "span:b:1-1".to_string(),
                    to_anchor: "span:c:1-1".to_string(),
                    confidence: 0.90,
                    location_delta: LocationDelta::Moved,
                    cardinality: Cardinality::OneToOne,
                    agent_link: false,
                    note: None,
                },
            )
            .expect("insert edge b->c");

        let lineage = retrieve_lineage(
            &index,
            &["span:c:1-1".to_string()],
            ExplainTraversal {
                max_depth: 1,
                ..ExplainTraversal::default()
            },
            false,
        )
        .expect("retrieve lineage");

        assert_eq!(lineage.len(), 1);
        assert_eq!(lineage[0].from_anchor, "span:b:1-1");
        assert_eq!(lineage[0].to_anchor, "span:c:1-1");
    }

    #[test]
    fn lineage_fanout_sorts_mixed_directions_after_dropping_seen_edges() {
        let index = SqliteIndex::open_in_memory().expect("sqlite");
        for (event_offset, (from_anchor, to_anchor, confidence)) in [
            ("span:start:1-1", "span:pivot:1-1", 0.99),
            ("span:other:1-1", "span:pivot:1-1", 0.80),
            ("span:pivot:1-1", "span:best:1-1", 0.95),
        ]
        .into_iter()
        .enumerate()
        {
            index
                .insert_edge(
                    &EdgeSource {
                        source_kind: EdgeSourceKind::SpanLink,
                        tape_id: "tape".to_string(),
                        event_offset: event_offset as u64,
                        pair_ordinal: 0,
                        from_window_ordinal: -1,
                        to_window_ordinal: -1,
                    },
                    &SpanEdge {
                        from_anchor: from_anchor.to_string(),
                        to_anchor: to_anchor.to_string(),
                        confidence,
                        location_delta: LocationDelta::Moved,
                        cardinality: Cardinality::OneToOne,
                        agent_link: false,
                        note: None,
                    },
                )
                .expect("insert edge");
        }

        let lineage = retrieve_lineage(
            &index,
            &["span:start:1-1".to_string()],
            ExplainTraversal {
                max_fanout: 2,
                max_depth: 2,
                ..ExplainTraversal::default()
            },
            false,
        )
        .expect("retrieve lineage");

        assert_eq!(lineage.len(), 3);
        assert_eq!(
            lineage
                .iter()
                .map(|edge| (
                    edge.from_anchor.as_str(),
                    edge.to_anchor.as_str(),
                    edge.confidence
                ))
                .collect::<Vec<_>>(),
            vec![
                ("span:start:1-1", "span:pivot:1-1", 0.99),
                ("span:pivot:1-1", "span:best:1-1", 0.95),
                ("span:other:1-1", "span:pivot:1-1", 0.80),
            ]
        );
    }
}
