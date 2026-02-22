pub mod lineage;

use rusqlite::{Connection, OptionalExtension, params};

use crate::index::lineage::{
    Cardinality, EvidenceFragmentRef, EvidenceKind, FileRange, LocationDelta, SpanEdge,
    LINK_THRESHOLD_DEFAULT, StoredEdgeClass, Tombstone,
};
use crate::tape::event::{TapeEventAt, TapeEventData};

#[derive(Debug, Clone, PartialEq)]
pub struct EdgeRow {
    pub from_anchor: String,
    pub to_anchor: String,
    pub confidence: f32,
    pub location_delta: LocationDelta,
    pub cardinality: Cardinality,
    pub agent_link: bool,
    pub note: Option<String>,
    pub stored_class: StoredEdgeClass,
}

pub struct SqliteIndex {
    conn: Connection,
}

impl SqliteIndex {
    pub fn open(path: &str) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        let index = Self { conn };
        index.init_schema()?;
        Ok(index)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let index = Self { conn };
        index.init_schema()?;
        Ok(index)
    }

    fn init_schema(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS evidence (
                anchor TEXT NOT NULL,
                tape_id TEXT NOT NULL,
                event_offset INTEGER NOT NULL,
                kind TEXT NOT NULL,
                file_path TEXT NOT NULL,
                timestamp TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_evidence_anchor ON evidence(anchor);

            CREATE TABLE IF NOT EXISTS edges (
                from_anchor TEXT NOT NULL,
                to_anchor TEXT NOT NULL,
                confidence REAL NOT NULL,
                location_delta TEXT NOT NULL,
                cardinality TEXT NOT NULL,
                agent_link INTEGER NOT NULL,
                note TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_edges_from_anchor ON edges(from_anchor);

            CREATE TABLE IF NOT EXISTS tombstones (
                anchor TEXT NOT NULL,
                tape_id TEXT NOT NULL,
                event_offset INTEGER NOT NULL,
                file_path TEXT NOT NULL,
                range_start INTEGER NOT NULL,
                range_end INTEGER NOT NULL,
                timestamp TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_tombstones_anchor ON tombstones(anchor);
            ",
        )?;
        Ok(())
    }

    pub fn insert_evidence(
        &self,
        anchor: &str,
        fragment: &EvidenceFragmentRef,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO evidence (anchor, tape_id, event_offset, kind, file_path, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                anchor,
                fragment.tape_id,
                fragment.event_offset,
                encode_evidence_kind(fragment.kind),
                fragment.file_path,
                fragment.timestamp
            ],
        )?;
        Ok(())
    }

    pub fn insert_edge(&self, edge: &SpanEdge, _link_threshold: f32) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO edges (
                from_anchor, to_anchor, confidence, location_delta, cardinality,
                agent_link, note
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                edge.from_anchor,
                edge.to_anchor,
                edge.confidence,
                encode_location_delta(edge.location_delta),
                encode_cardinality(edge.cardinality),
                if edge.agent_link { 1_i64 } else { 0_i64 },
                edge.note
            ],
        )?;
        Ok(())
    }

    pub fn insert_tombstone(&self, tombstone: &Tombstone) -> rusqlite::Result<()> {
        for anchor in &tombstone.anchor_hashes {
            self.conn.execute(
                "INSERT INTO tombstones (
                    anchor, tape_id, event_offset, file_path, range_start, range_end, timestamp
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    anchor,
                    tombstone.tape_id,
                    tombstone.event_offset,
                    tombstone.file_path,
                    tombstone.range_at_deletion.start,
                    tombstone.range_at_deletion.end,
                    tombstone.timestamp
                ],
            )?;
        }
        Ok(())
    }

    pub fn evidence_for_anchor(&self, anchor: &str) -> rusqlite::Result<Vec<EvidenceFragmentRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT tape_id, event_offset, kind, file_path, timestamp
             FROM evidence
             WHERE anchor = ?1
             ORDER BY event_offset ASC",
        )?;

        let mut rows = stmt.query(params![anchor])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(EvidenceFragmentRef {
                tape_id: row.get(0)?,
                event_offset: row.get(1)?,
                kind: decode_evidence_kind(&row.get::<_, String>(2)?),
                file_path: row.get(3)?,
                timestamp: row.get(4)?,
            });
        }
        Ok(out)
    }

    pub fn outbound_edges(
        &self,
        from_anchor: &str,
        min_confidence: f32,
        include_forensics: bool,
    ) -> rusqlite::Result<Vec<EdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_anchor, to_anchor, confidence, location_delta, cardinality,
                    agent_link, note
             FROM edges
             WHERE from_anchor = ?1
             ORDER BY confidence DESC",
        )?;

        let mut rows = stmt.query(params![from_anchor])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let confidence: f32 = row.get(2)?;
            let agent_link = row.get::<_, i64>(5)? != 0;
            let stored_class = if !agent_link && confidence < LINK_THRESHOLD_DEFAULT {
                StoredEdgeClass::LocationOnly
            } else {
                StoredEdgeClass::Lineage
            };
            if !include_forensics && stored_class == StoredEdgeClass::LocationOnly && !agent_link {
                continue;
            }
            if !agent_link && confidence < min_confidence && !include_forensics {
                continue;
            }
            out.push(EdgeRow {
                from_anchor: row.get(0)?,
                to_anchor: row.get(1)?,
                confidence,
                location_delta: decode_location_delta(&row.get::<_, String>(3)?),
                cardinality: decode_cardinality(&row.get::<_, String>(4)?),
                agent_link,
                note: row.get(6)?,
                stored_class,
            });
        }
        Ok(out)
    }

    pub fn tombstones_for_anchor(&self, anchor: &str) -> rusqlite::Result<Vec<Tombstone>> {
        let mut stmt = self.conn.prepare(
            "SELECT tape_id, event_offset, file_path, range_start, range_end, timestamp
             FROM tombstones
             WHERE anchor = ?1
             ORDER BY event_offset ASC",
        )?;

        let mut rows = stmt.query(params![anchor])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(Tombstone {
                anchor_hashes: vec![anchor.to_string()],
                tape_id: row.get(0)?,
                event_offset: row.get(1)?,
                file_path: row.get(2)?,
                range_at_deletion: FileRange {
                    start: row.get(3)?,
                    end: row.get(4)?,
                },
                timestamp: row.get(5)?,
            });
        }

        Ok(out)
    }

    pub fn ingest_tape_events(
        &self,
        tape_id: &str,
        events: &[TapeEventAt],
        link_threshold: f32,
    ) -> rusqlite::Result<()> {
        for item in events {
            match &item.event.data {
                TapeEventData::CodeRead(read) => {
                    let fragment = EvidenceFragmentRef {
                        tape_id: tape_id.to_string(),
                        event_offset: item.offset,
                        kind: EvidenceKind::Read,
                        file_path: read.file.clone(),
                        timestamp: item.event.timestamp.clone(),
                    };
                    for anchor in &read.anchor_hashes {
                        self.insert_evidence(anchor, &fragment)?;
                    }
                }
                TapeEventData::CodeEdit(edit) => {
                    if let Some(after_hash) = &edit.after_hash {
                        let fragment = EvidenceFragmentRef {
                            tape_id: tape_id.to_string(),
                            event_offset: item.offset,
                            kind: EvidenceKind::Edit,
                            file_path: edit.file.clone(),
                            timestamp: item.event.timestamp.clone(),
                        };
                        self.insert_evidence(after_hash, &fragment)?;
                    }

                    if let (Some(before_hash), Some(after_hash)) =
                        (edit.before_hash.as_ref(), edit.after_hash.as_ref())
                    {
                        let confidence = if before_hash == after_hash { 1.0 } else { 0.30 };
                        self.insert_edge(
                            &SpanEdge {
                                from_anchor: before_hash.clone(),
                                to_anchor: after_hash.clone(),
                                confidence,
                                location_delta: LocationDelta::Same,
                                cardinality: Cardinality::OneToOne,
                                agent_link: false,
                                note: None,
                            },
                            link_threshold,
                        )?;
                    }

                    if edit.after_hash.is_none() {
                        if let Some(before_hash) = &edit.before_hash {
                            let range = edit
                                .before_range
                                .or(edit.after_range)
                                .map(|r| FileRange {
                                    start: r.start,
                                    end: r.end,
                                })
                                .unwrap_or(FileRange { start: 0, end: 0 });
                            self.insert_tombstone(&Tombstone {
                                anchor_hashes: vec![before_hash.clone()],
                                tape_id: tape_id.to_string(),
                                event_offset: item.offset,
                                file_path: edit.file.clone(),
                                range_at_deletion: range,
                                timestamp: item.event.timestamp.clone(),
                            })?;
                        }
                    }
                }
                TapeEventData::SpanLink(link) => {
                    let from_anchor = encode_span_link_anchor(&link.from_file, link.from_range);
                    let to_anchor = encode_span_link_anchor(&link.to_file, link.to_range);
                    self.insert_edge(
                        &SpanEdge {
                            from_anchor,
                            to_anchor,
                            confidence: 0.0,
                            location_delta: LocationDelta::Moved,
                            cardinality: Cardinality::OneToOne,
                            agent_link: true,
                            note: link.note.clone(),
                        },
                        link_threshold,
                    )?;
                }
                TapeEventData::Meta(_) | TapeEventData::Other { .. } => {}
            }
        }

        Ok(())
    }

    pub fn maybe_meta_label(&self) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT NULL", [], |row| row.get(0))
            .optional()
    }
}

fn encode_span_link_anchor(file: &str, range: crate::tape::event::FileRange) -> String {
    format!("span:{file}:{}-{}", range.start, range.end)
}

fn encode_evidence_kind(kind: EvidenceKind) -> &'static str {
    match kind {
        EvidenceKind::Edit => "edit",
        EvidenceKind::Read => "read",
        EvidenceKind::Tool => "tool",
        EvidenceKind::Message => "message",
    }
}

fn decode_evidence_kind(raw: &str) -> EvidenceKind {
    match raw {
        "edit" => EvidenceKind::Edit,
        "read" => EvidenceKind::Read,
        "tool" => EvidenceKind::Tool,
        "message" => EvidenceKind::Message,
        _ => EvidenceKind::Read,
    }
}

fn encode_location_delta(delta: LocationDelta) -> &'static str {
    match delta {
        LocationDelta::Same => "same",
        LocationDelta::Adjacent => "adjacent",
        LocationDelta::Moved => "moved",
        LocationDelta::Absent => "absent",
    }
}

fn decode_location_delta(raw: &str) -> LocationDelta {
    match raw {
        "same" => LocationDelta::Same,
        "adjacent" => LocationDelta::Adjacent,
        "moved" => LocationDelta::Moved,
        "absent" => LocationDelta::Absent,
        _ => LocationDelta::Absent,
    }
}

fn encode_cardinality(cardinality: Cardinality) -> &'static str {
    match cardinality {
        Cardinality::OneToOne => "1:1",
        Cardinality::OneToMany => "1:N",
        Cardinality::ManyToOne => "N:1",
    }
}

fn decode_cardinality(raw: &str) -> Cardinality {
    match raw {
        "1:1" => Cardinality::OneToOne,
        "1:N" => Cardinality::OneToMany,
        "N:1" => Cardinality::ManyToOne,
        _ => Cardinality::OneToOne,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::lineage::LINK_THRESHOLD_DEFAULT;
    use crate::tape::event::{CodeEditEvent, CodeReadEvent, FileRange, TapeEvent, TapeEventData};

    fn read_event(anchor: &str, file: &str, offset: u64) -> TapeEventAt {
        TapeEventAt {
            offset,
            event: TapeEvent {
                timestamp: "2026-02-22T00:00:00Z".to_string(),
                data: TapeEventData::CodeRead(CodeReadEvent {
                    file: file.to_string(),
                    range: FileRange { start: 1, end: 1 },
                    anchor_hashes: vec![anchor.to_string()],
                }),
            },
        }
    }

    fn edit_event(
        before_hash: Option<&str>,
        after_hash: Option<&str>,
        file: &str,
        offset: u64,
    ) -> TapeEventAt {
        TapeEventAt {
            offset,
            event: TapeEvent {
                timestamp: "2026-02-22T00:00:01Z".to_string(),
                data: TapeEventData::CodeEdit(CodeEditEvent {
                    file: file.to_string(),
                    before_range: Some(FileRange { start: 10, end: 12 }),
                    after_range: Some(FileRange { start: 10, end: 13 }),
                    before_hash: before_hash.map(ToOwned::to_owned),
                    after_hash: after_hash.map(ToOwned::to_owned),
                }),
            },
        }
    }

    #[test]
    fn ingests_reads_edits_edges_and_tombstones() {
        let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
        let events = vec![
            read_event("read-anchor", "src/lib.rs", 0),
            edit_event(Some("before"), Some("after"), "src/lib.rs", 1),
            edit_event(Some("deleted"), None, "src/lib.rs", 2),
        ];

        index
            .ingest_tape_events("tape-1", &events, LINK_THRESHOLD_DEFAULT)
            .expect("ingest succeeds");

        let read_refs = index
            .evidence_for_anchor("read-anchor")
            .expect("read evidence query");
        assert_eq!(read_refs.len(), 1);
        assert_eq!(read_refs[0].kind, EvidenceKind::Read);

        let edit_refs = index.evidence_for_anchor("after").expect("edit evidence query");
        assert_eq!(edit_refs.len(), 1);
        assert_eq!(edit_refs[0].kind, EvidenceKind::Edit);

        let edges = index
            .outbound_edges("before", 0.50, false)
            .expect("edge query");
        assert_eq!(edges.len(), 0);

        let edges_forensics = index
            .outbound_edges("before", 0.50, true)
            .expect("edge query with forensics");
        assert_eq!(edges_forensics.len(), 1);

        let tombstones = index
            .tombstones_for_anchor("deleted")
            .expect("tombstone query");
        assert_eq!(tombstones.len(), 1);
        assert_eq!(tombstones[0].file_path, "src/lib.rs");
    }

    #[test]
    fn span_link_is_agent_edge_and_survives_min_confidence() {
        let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
        let events = vec![TapeEventAt {
            offset: 5,
            event: TapeEvent {
                timestamp: "2026-02-22T00:00:03Z".to_string(),
                data: TapeEventData::SpanLink(crate::tape::event::SpanLinkEvent {
                    from_file: "src/a.rs".to_string(),
                    from_range: FileRange { start: 1, end: 2 },
                    to_file: "src/b.rs".to_string(),
                    to_range: FileRange { start: 10, end: 20 },
                    note: Some("extract".to_string()),
                }),
            },
        }];

        index
            .ingest_tape_events("tape-2", &events, LINK_THRESHOLD_DEFAULT)
            .expect("ingest succeeds");

        let from = "span:src/a.rs:1-2";
        let edges = index
            .outbound_edges(from, 0.99, false)
            .expect("edge query for span link");
        assert_eq!(edges.len(), 1);
        assert!(edges[0].agent_link);
        assert_eq!(edges[0].note.as_deref(), Some("extract"));
    }

    #[test]
    fn location_only_edges_remain_hidden_without_forensics_even_when_min_confidence_is_lowered() {
        let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
        index
            .insert_edge(
                &SpanEdge {
                    from_anchor: "before".to_string(),
                    to_anchor: "after".to_string(),
                    confidence: 0.29,
                    location_delta: LocationDelta::Moved,
                    cardinality: Cardinality::OneToOne,
                    agent_link: false,
                    note: None,
                },
                LINK_THRESHOLD_DEFAULT,
            )
            .expect("insert edge");

        let normal = index
            .outbound_edges("before", 0.20, false)
            .expect("edge query without forensics");
        assert_eq!(normal.len(), 0);

        let forensics = index
            .outbound_edges("before", 0.20, true)
            .expect("edge query with forensics");
        assert_eq!(forensics.len(), 1);
        assert_eq!(forensics[0].stored_class, StoredEdgeClass::LocationOnly);
    }

    #[test]
    fn below_threshold_links_are_stored_as_location_only_instead_of_dropped() {
        let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
        index
            .insert_edge(
                &SpanEdge {
                    from_anchor: "before".to_string(),
                    to_anchor: "after".to_string(),
                    confidence: 0.29,
                    location_delta: LocationDelta::Moved,
                    cardinality: Cardinality::OneToOne,
                    agent_link: false,
                    note: None,
                },
                LINK_THRESHOLD_DEFAULT,
            )
            .expect("insert edge");

        let forensics = index
            .outbound_edges("before", 0.50, true)
            .expect("edge query with forensics");
        assert_eq!(forensics.len(), 1);
        assert_eq!(forensics[0].to_anchor, "after");
        assert_eq!(forensics[0].stored_class, StoredEdgeClass::LocationOnly);
    }
}
