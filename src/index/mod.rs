pub mod lineage;

use std::collections::HashSet;
use std::ops::Deref;

use rusqlite::{Connection, params};

use crate::anchor::{expand_winnow_anchor, fingerprint_anchor_hashes, fingerprint_token_hashes};
use crate::index::lineage::{
    Cardinality, EvidenceFragmentRef, EvidenceKind, LINK_THRESHOLD_DEFAULT, LocationDelta,
    SpanEdge, StoredEdgeClass, Tombstone,
};
use crate::tape::event::{FileRange, TapeEventAt, TapeEventData};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchDirection {
    Received,
    Sent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchLink {
    pub uuid: String,
    pub first_turn_index: i64,
    pub direction: DispatchDirection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchLinkRow {
    pub tape_id: String,
    pub uuid: String,
    pub first_turn_index: i64,
    pub direction: DispatchDirection,
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
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = FULL;
            ",
        )?;

        let version: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        match version {
            0 => {
                if self.table_exists("evidence")? {
                    self.migrate_legacy_schema_to_v1()?;
                } else {
                    self.create_schema_v1()?;
                    self.conn.execute_batch("PRAGMA user_version = 1;")?;
                }
                self.migrate_v1_to_v2()?;
                self.migrate_v2_to_v3()?;
            }
            1 => {
                self.create_schema_v1()?;
                self.migrate_v1_to_v2()?;
                self.migrate_v2_to_v3()?;
            }
            2 => {
                self.create_schema_v2()?;
                self.migrate_v2_to_v3()?;
            }
            3 => {
                self.create_schema_v3()?;
            }
            _ => return Err(rusqlite::Error::InvalidQuery),
        }
        Ok(())
    }

    fn table_exists(&self, name: &str) -> rusqlite::Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    fn create_schema_v1(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS evidence (
                anchor TEXT NOT NULL,
                tape_id TEXT NOT NULL,
                event_offset INTEGER NOT NULL,
                kind TEXT NOT NULL,
                file_path TEXT NOT NULL,
                timestamp TEXT NOT NULL,
                UNIQUE(anchor, tape_id, event_offset, kind)
            );

            CREATE INDEX IF NOT EXISTS idx_evidence_anchor ON evidence(anchor);

            CREATE TABLE IF NOT EXISTS edges (
                from_anchor TEXT NOT NULL,
                to_anchor TEXT NOT NULL,
                confidence REAL NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
                location_delta TEXT NOT NULL,
                cardinality TEXT NOT NULL,
                agent_link INTEGER NOT NULL CHECK (agent_link IN (0, 1)),
                note TEXT NOT NULL DEFAULT '',
                UNIQUE(from_anchor, to_anchor, confidence, location_delta, cardinality, agent_link, note)
            );

            CREATE INDEX IF NOT EXISTS idx_edges_from_anchor ON edges(from_anchor);
            CREATE INDEX IF NOT EXISTS idx_edges_to_anchor ON edges(to_anchor);

            CREATE TABLE IF NOT EXISTS tombstones (
                anchor TEXT NOT NULL,
                tape_id TEXT NOT NULL,
                event_offset INTEGER NOT NULL,
                file_path TEXT NOT NULL,
                range_start INTEGER NOT NULL,
                range_end INTEGER NOT NULL,
                timestamp TEXT NOT NULL,
                UNIQUE(anchor, tape_id, event_offset)
            );

            CREATE INDEX IF NOT EXISTS idx_tombstones_anchor ON tombstones(anchor);

            CREATE TABLE IF NOT EXISTS tapes (
                tape_id TEXT PRIMARY KEY
            );
            ",
        )?;
        Ok(())
    }

    fn create_schema_v2(&self) -> rusqlite::Result<()> {
        self.create_schema_v1()?;
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS dispatch_links (
                tape_id TEXT NOT NULL,
                uuid TEXT NOT NULL,
                first_turn_index INTEGER NOT NULL,
                direction TEXT NOT NULL CHECK(direction IN ('received', 'sent')),
                PRIMARY KEY (tape_id, uuid)
            );

            CREATE INDEX IF NOT EXISTS idx_dispatch_links_uuid ON dispatch_links(uuid);
            CREATE INDEX IF NOT EXISTS idx_dispatch_links_tape ON dispatch_links(tape_id);
            CREATE INDEX IF NOT EXISTS idx_dispatch_links_received
                ON dispatch_links(tape_id, direction, first_turn_index);
            ",
        )?;
        Ok(())
    }

    fn create_schema_v3(&self) -> rusqlite::Result<()> {
        self.create_schema_v2()?;
        self.ensure_query_feedback_schema()?;
        Ok(())
    }

    fn ensure_query_feedback_schema(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS query_results (
                result_id TEXT PRIMARY KEY,
                command TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_query_results_command
                ON query_results(command, created_at);

            CREATE TABLE IF NOT EXISTS result_feedback (
                result_id TEXT PRIMARY KEY,
                outcome TEXT NOT NULL,
                note TEXT,
                rated_at TEXT NOT NULL,
                FOREIGN KEY(result_id) REFERENCES query_results(result_id) ON DELETE CASCADE
            );
            ",
        )
    }

    fn migrate_v1_to_v2(&self) -> rusqlite::Result<()> {
        self.create_schema_v2()?;
        self.conn.execute_batch("PRAGMA user_version = 2;")?;
        Ok(())
    }

    fn migrate_v2_to_v3(&self) -> rusqlite::Result<()> {
        self.create_schema_v3()?;
        self.conn.execute_batch("PRAGMA user_version = 3;")?;
        Ok(())
    }

    fn migrate_legacy_schema_to_v1(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "
            BEGIN IMMEDIATE;
            ALTER TABLE evidence RENAME TO evidence_legacy;
            ALTER TABLE edges RENAME TO edges_legacy;
            ALTER TABLE tombstones RENAME TO tombstones_legacy;
            COMMIT;
            ",
        )?;

        self.create_schema_v1()?;
        self.conn.execute_batch(
            "
            INSERT OR IGNORE INTO evidence (anchor, tape_id, event_offset, kind, file_path, timestamp)
            SELECT anchor, tape_id, event_offset, kind, file_path, timestamp
            FROM evidence_legacy;

            INSERT OR IGNORE INTO edges (
                from_anchor, to_anchor, confidence, location_delta, cardinality,
                agent_link, note
            )
            SELECT
                from_anchor,
                to_anchor,
                confidence,
                location_delta,
                cardinality,
                agent_link,
                COALESCE(note, '')
            FROM edges_legacy;

            INSERT OR IGNORE INTO tombstones (
                anchor, tape_id, event_offset, file_path, range_start, range_end, timestamp
            )
            SELECT anchor, tape_id, event_offset, file_path, range_start, range_end, timestamp
            FROM tombstones_legacy;

            DROP TABLE evidence_legacy;
            DROP TABLE edges_legacy;
            DROP TABLE tombstones_legacy;
            PRAGMA user_version = 1;
            ",
        )?;
        Ok(())
    }

    pub fn insert_evidence(
        &self,
        anchor: &str,
        fragment: &EvidenceFragmentRef,
    ) -> rusqlite::Result<()> {
        Self::validate_anchor(anchor)?;
        Self::insert_evidence_on(&self.conn, anchor, fragment)
    }

    fn insert_evidence_on(
        conn: &Connection,
        anchor: &str,
        fragment: &EvidenceFragmentRef,
    ) -> rusqlite::Result<()> {
        Self::validate_anchor(anchor)?;
        conn.execute(
            "INSERT OR IGNORE INTO evidence (anchor, tape_id, event_offset, kind, file_path, timestamp)
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

    pub fn insert_edge(&self, edge: &SpanEdge, link_threshold: f32) -> rusqlite::Result<()> {
        Self::validate_anchor(&edge.from_anchor)?;
        Self::validate_anchor(&edge.to_anchor)?;
        Self::insert_edge_on(&self.conn, edge, link_threshold)
    }

    fn insert_edge_on(
        conn: &Connection,
        edge: &SpanEdge,
        _link_threshold: f32,
    ) -> rusqlite::Result<()> {
        Self::validate_anchor(&edge.from_anchor)?;
        Self::validate_anchor(&edge.to_anchor)?;
        Self::validate_confidence(edge.confidence)?;
        conn.execute(
            "INSERT OR IGNORE INTO edges (
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
                edge.note.as_deref().unwrap_or("")
            ],
        )?;
        Ok(())
    }

    pub fn insert_tombstone(&self, tombstone: &Tombstone) -> rusqlite::Result<()> {
        Self::insert_tombstone_on(&self.conn, tombstone)
    }

    fn insert_tombstone_on(conn: &Connection, tombstone: &Tombstone) -> rusqlite::Result<()> {
        for anchor in &tombstone.anchor_hashes {
            Self::validate_anchor(anchor)?;
            conn.execute(
                "INSERT OR IGNORE INTO tombstones (
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

    pub fn insert_dispatch_link(&self, tape_id: &str, link: &DispatchLink) -> rusqlite::Result<()> {
        Self::insert_dispatch_link_on(&self.conn, tape_id, link)
    }

    fn insert_dispatch_link_on(
        conn: &Connection,
        tape_id: &str,
        link: &DispatchLink,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT OR IGNORE INTO dispatch_links (tape_id, uuid, first_turn_index, direction)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                tape_id,
                link.uuid,
                link.first_turn_index,
                encode_dispatch_direction(link.direction)
            ],
        )?;
        Ok(())
    }

    pub fn record_query_result(
        &self,
        result_id: &str,
        command: &str,
        payload_json: &str,
        created_at: &str,
    ) -> rusqlite::Result<()> {
        self.ensure_query_feedback_schema()?;
        self.conn.execute(
            "INSERT OR REPLACE INTO query_results (result_id, command, payload_json, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![result_id, command, payload_json, created_at],
        )?;
        Ok(())
    }

    pub fn query_result_exists(&self, result_id: &str) -> rusqlite::Result<bool> {
        self.ensure_query_feedback_schema()?;
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM query_results WHERE result_id = ?1",
            params![result_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn upsert_result_feedback(
        &self,
        result_id: &str,
        outcome: &str,
        note: Option<&str>,
        rated_at: &str,
    ) -> rusqlite::Result<()> {
        self.ensure_query_feedback_schema()?;
        self.conn.execute(
            "INSERT INTO result_feedback (result_id, outcome, note, rated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(result_id) DO UPDATE SET
               outcome = excluded.outcome,
               note = excluded.note,
               rated_at = excluded.rated_at",
            params![result_id, outcome, note, rated_at],
        )?;
        Ok(())
    }

    pub fn evidence_for_anchor(&self, anchor: &str) -> rusqlite::Result<Vec<EvidenceFragmentRef>> {
        let mut stmt = self.conn.prepare(
            "SELECT tape_id, event_offset, kind, file_path, timestamp
             FROM evidence
             WHERE anchor = ?1
             ORDER BY timestamp ASC, tape_id ASC, event_offset ASC",
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

    pub fn window_anchor_stats_for_file(
        &self,
        file_path: &str,
    ) -> rusqlite::Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT anchor, COUNT(*) AS hits
             FROM evidence
             WHERE file_path = ?1
               AND instr(anchor, ',') > 0
             GROUP BY anchor
             ORDER BY hits DESC, anchor ASC",
        )?;

        let mut rows = stmt.query(params![file_path])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((row.get(0)?, row.get(1)?));
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
            let stored_class = derive_stored_class(agent_link, confidence);
            if !include_forensics && stored_class == StoredEdgeClass::LocationOnly && !agent_link {
                continue;
            }
            if !include_forensics && !agent_link && confidence < min_confidence {
                continue;
            }
            out.push(EdgeRow {
                from_anchor: row.get(0)?,
                to_anchor: row.get(1)?,
                confidence,
                location_delta: decode_location_delta(&row.get::<_, String>(3)?),
                cardinality: decode_cardinality(&row.get::<_, String>(4)?),
                agent_link,
                note: {
                    let note: String = row.get(6)?;
                    if note.is_empty() { None } else { Some(note) }
                },
                stored_class,
            });
        }
        Ok(out)
    }

    pub fn inbound_edges(
        &self,
        to_anchor: &str,
        min_confidence: f32,
        include_forensics: bool,
    ) -> rusqlite::Result<Vec<EdgeRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT from_anchor, to_anchor, confidence, location_delta, cardinality,
                    agent_link, note
             FROM edges
             WHERE to_anchor = ?1
             ORDER BY confidence DESC",
        )?;

        let mut rows = stmt.query(params![to_anchor])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let confidence: f32 = row.get(2)?;
            let agent_link = row.get::<_, i64>(5)? != 0;
            let stored_class = derive_stored_class(agent_link, confidence);
            if !include_forensics && stored_class == StoredEdgeClass::LocationOnly && !agent_link {
                continue;
            }
            if !include_forensics && !agent_link && confidence < min_confidence {
                continue;
            }
            out.push(EdgeRow {
                from_anchor: row.get(0)?,
                to_anchor: row.get(1)?,
                confidence,
                location_delta: decode_location_delta(&row.get::<_, String>(3)?),
                cardinality: decode_cardinality(&row.get::<_, String>(4)?),
                agent_link,
                note: {
                    let note: String = row.get(6)?;
                    if note.is_empty() { None } else { Some(note) }
                },
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

    pub fn referenced_tape_ids(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT tape_id FROM evidence
             UNION
             SELECT tape_id FROM tombstones",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(row.get(0)?);
        }
        Ok(out)
    }

    pub fn has_tape(&self, tape_id: &str) -> rusqlite::Result<bool> {
        let mut stmt = self
            .conn
            .prepare("SELECT 1 FROM tapes WHERE tape_id = ?1 LIMIT 1")?;
        let mut rows = stmt.query(params![tape_id])?;
        Ok(rows.next()?.is_some())
    }

    pub fn ingest_tape_events(
        &self,
        tape_id: &str,
        events: &[TapeEventAt],
        link_threshold: f32,
    ) -> rusqlite::Result<()> {
        self.ingest_tape_events_with_dispatch(tape_id, events, &[], link_threshold)
    }

    pub fn ingest_tape_events_with_dispatch(
        &self,
        tape_id: &str,
        events: &[TapeEventAt],
        dispatch_links: &[DispatchLink],
        link_threshold: f32,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
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
                    for anchor in read_evidence_anchors(read) {
                        Self::insert_evidence_on(tx.deref(), &anchor, &fragment)?;
                    }
                }
                TapeEventData::CodeEdit(edit) => {
                    // Individual tokens for evidence rows (one DB row per hash).
                    let before_tokens = edit_side_tokens(
                        edit.before_text.as_deref(),
                        edit.before_hash.as_deref(),
                        &edit.before_anchor_hashes,
                    );
                    let after_tokens = edit_side_tokens(
                        edit.after_text.as_deref(),
                        edit.after_hash.as_deref(),
                        &edit.after_anchor_hashes,
                    );
                    // Window-level anchors for edges (avoids N×M explosion).
                    let before_edge = edit_side_edge_anchors(
                        edit.before_text.as_deref(),
                        edit.before_hash.as_deref(),
                        &edit.before_anchor_hashes,
                    );
                    let after_edge = edit_side_edge_anchors(
                        edit.after_text.as_deref(),
                        edit.after_hash.as_deref(),
                        &edit.after_anchor_hashes,
                    );

                    if !before_tokens.is_empty() {
                        let fragment = EvidenceFragmentRef {
                            tape_id: tape_id.to_string(),
                            event_offset: item.offset,
                            kind: EvidenceKind::Edit,
                            file_path: edit.file.clone(),
                            timestamp: item.event.timestamp.clone(),
                        };
                        for anchor in &before_tokens {
                            Self::insert_evidence_on(tx.deref(), anchor, &fragment)?;
                        }
                    }

                    if !after_tokens.is_empty() {
                        let fragment = EvidenceFragmentRef {
                            tape_id: tape_id.to_string(),
                            event_offset: item.offset,
                            kind: EvidenceKind::Edit,
                            file_path: edit.file.clone(),
                            timestamp: item.event.timestamp.clone(),
                        };
                        for anchor in &after_tokens {
                            Self::insert_evidence_on(tx.deref(), anchor, &fragment)?;
                        }
                    }

                    if !before_edge.is_empty() && !after_edge.is_empty() {
                        let confidence = if before_edge == after_edge {
                            1.0
                        } else {
                            edit.similarity.unwrap_or(0.0)
                        };
                        Self::validate_confidence(confidence)?;
                        for before_anchor in &before_edge {
                            for after_anchor in &after_edge {
                                Self::insert_edge_on(
                                    tx.deref(),
                                    &SpanEdge {
                                        from_anchor: before_anchor.clone(),
                                        to_anchor: after_anchor.clone(),
                                        confidence,
                                        location_delta: LocationDelta::Same,
                                        cardinality: Cardinality::OneToOne,
                                        agent_link: false,
                                        note: None,
                                    },
                                    link_threshold,
                                )?;
                            }
                        }
                    }

                    if after_tokens.is_empty() && !before_tokens.is_empty() {
                        let range = edit
                            .before_range
                            .or(edit.after_range)
                            .map(|r| FileRange {
                                start: r.start,
                                end: r.end,
                            })
                            .unwrap_or(FileRange { start: 0, end: 0 });
                        Self::insert_tombstone_on(
                            tx.deref(),
                            &Tombstone {
                                anchor_hashes: before_tokens,
                                tape_id: tape_id.to_string(),
                                event_offset: item.offset,
                                file_path: edit.file.clone(),
                                range_at_deletion: range,
                                timestamp: item.event.timestamp.clone(),
                            },
                        )?;
                    }
                }
                TapeEventData::SpanLink(link) => {
                    let from_anchor = encode_span_link_anchor(&link.from_file, link.from_range);
                    let to_anchor = encode_span_link_anchor(&link.to_file, link.to_range);
                    Self::insert_edge_on(
                        tx.deref(),
                        &SpanEdge {
                            from_anchor,
                            to_anchor,
                            confidence: 1.0,
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

        for link in dispatch_links {
            Self::insert_dispatch_link_on(tx.deref(), tape_id, link)?;
        }

        tx.execute(
            "INSERT OR IGNORE INTO tapes (tape_id) VALUES (?1)",
            params![tape_id],
        )?;

        tx.commit()?;
        Ok(())
    }

    pub fn dispatch_links_for_tape(&self, tape_id: &str) -> rusqlite::Result<Vec<DispatchLink>> {
        let mut stmt = self.conn.prepare(
            "SELECT uuid, first_turn_index, direction
             FROM dispatch_links
             WHERE tape_id = ?1
             ORDER BY first_turn_index ASC, uuid ASC",
        )?;
        let mut rows = stmt.query(params![tape_id])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(DispatchLink {
                uuid: row.get(0)?,
                first_turn_index: row.get(1)?,
                direction: decode_dispatch_direction(&row.get::<_, String>(2)?),
            });
        }
        Ok(out)
    }

    pub fn dispatch_links_for_uuid(&self, uuid: &str) -> rusqlite::Result<Vec<DispatchLinkRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT tape_id, uuid, first_turn_index, direction
             FROM dispatch_links
             WHERE uuid = ?1
             ORDER BY first_turn_index ASC, tape_id ASC",
        )?;
        let mut rows = stmt.query(params![uuid])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push(DispatchLinkRow {
                tape_id: row.get(0)?,
                uuid: row.get(1)?,
                first_turn_index: row.get(2)?,
                direction: decode_dispatch_direction(&row.get::<_, String>(3)?),
            });
        }
        Ok(out)
    }

    pub fn latest_received_dispatch_before_turn(
        &self,
        tape_id: &str,
        turn_index: i64,
    ) -> rusqlite::Result<Option<DispatchLink>> {
        let mut stmt = self.conn.prepare(
            "SELECT uuid, first_turn_index, direction
             FROM dispatch_links
             WHERE tape_id = ?1
               AND direction = 'received'
               AND first_turn_index < ?2
             ORDER BY first_turn_index DESC, uuid ASC
             LIMIT 1",
        )?;
        let mut rows = stmt.query(params![tape_id, turn_index])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(DispatchLink {
                uuid: row.get(0)?,
                first_turn_index: row.get(1)?,
                direction: decode_dispatch_direction(&row.get::<_, String>(2)?),
            }));
        }
        Ok(None)
    }

    pub fn sent_dispatch_for_uuid(&self, uuid: &str) -> rusqlite::Result<Option<DispatchLinkRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT tape_id, uuid, first_turn_index, direction
             FROM dispatch_links
             WHERE uuid = ?1
               AND direction = 'sent'
             ORDER BY first_turn_index DESC, tape_id ASC
             LIMIT 1",
        )?;
        let mut rows = stmt.query(params![uuid])?;
        if let Some(row) = rows.next()? {
            return Ok(Some(DispatchLinkRow {
                tape_id: row.get(0)?,
                uuid: row.get(1)?,
                first_turn_index: row.get(2)?,
                direction: decode_dispatch_direction(&row.get::<_, String>(3)?),
            }));
        }
        Ok(None)
    }
}

fn encode_span_link_anchor(file: &str, range: crate::tape::event::FileRange) -> String {
    format!("span:{file}:{}-{}", range.start, range.end)
}

/// Anchors used to insert evidence rows for a code-read event.
/// Returns individual winnow hash tokens so each can be indexed by equality.
fn read_evidence_anchors(read: &crate::tape::event::CodeReadEvent) -> Vec<String> {
    if let Some(text) = read.text.as_deref() {
        return fingerprint_token_hashes(text);
    }
    expand_legacy_anchors(None, &read.anchor_hashes)
}

/// Anchors used to insert evidence rows for one side of a code-edit event.
/// Returns individual winnow hash tokens.
fn edit_side_tokens(text: Option<&str>, hash: Option<&str>, anchors: &[String]) -> Vec<String> {
    if let Some(text) = text {
        return fingerprint_token_hashes(text);
    }
    expand_legacy_anchors(hash, anchors)
}

/// Anchors used to insert edge rows for one side of a code-edit event.
///
/// * When `text` is available, returns window-level (comma-separated)
///   fingerprints — one per 24-line window — to prevent N×M edge explosion.
/// * For legacy tape events with pre-computed `anchor_hashes`, expands any
///   comma-separated winnow entries into individual tokens so that edge
///   targets remain queryable by exact equality.
fn edit_side_edge_anchors(
    text: Option<&str>,
    hash: Option<&str>,
    anchors: &[String],
) -> Vec<String> {
    if let Some(text) = text {
        return fingerprint_anchor_hashes(text);
    }
    expand_legacy_anchors(hash, anchors)
}

/// Expand legacy anchor_hashes into individual winnow tokens.
/// A comma-separated entry like "winnow:aaa,bbb" becomes ["winnow:aaa","winnow:bbb"].
fn expand_legacy_anchors(hash: Option<&str>, anchors: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for anchor in anchors {
        for token in expand_winnow_anchor(anchor) {
            if seen.insert(token.clone()) {
                out.push(token);
            }
        }
        // Non-winnow anchors (e.g. span: links) are kept as-is.
        if !anchor.starts_with("winnow:") && seen.insert(anchor.clone()) {
            out.push(anchor.clone());
        }
    }

    if out.is_empty()
        && let Some(anchor) = hash.filter(|h| h.starts_with("winnow:"))
    {
        for token in expand_winnow_anchor(anchor) {
            if seen.insert(token.clone()) {
                out.push(token);
            }
        }
    }

    out
}

impl SqliteIndex {
    fn validate_anchor(anchor: &str) -> rusqlite::Result<()> {
        if anchor.is_empty() {
            return Err(rusqlite::Error::InvalidParameterName(
                "anchor_hash must not be empty".to_string(),
            ));
        }
        Ok(())
    }

    fn validate_confidence(confidence: f32) -> rusqlite::Result<()> {
        if !(0.0..=1.0).contains(&confidence) {
            return Err(rusqlite::Error::InvalidParameterName(
                "confidence must be in [0.0, 1.0]".to_string(),
            ));
        }
        Ok(())
    }
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

fn encode_dispatch_direction(direction: DispatchDirection) -> &'static str {
    match direction {
        DispatchDirection::Received => "received",
        DispatchDirection::Sent => "sent",
    }
}

fn decode_dispatch_direction(raw: &str) -> DispatchDirection {
    match raw {
        "sent" => DispatchDirection::Sent,
        _ => DispatchDirection::Received,
    }
}

fn derive_stored_class(agent_link: bool, confidence: f32) -> StoredEdgeClass {
    if !agent_link && confidence < LINK_THRESHOLD_DEFAULT {
        StoredEdgeClass::LocationOnly
    } else {
        StoredEdgeClass::Lineage
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor::fingerprint_token_hashes;
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
                    text: None,
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
        edit_event_with_similarity(before_hash, after_hash, Some(0.80), file, offset)
    }

    fn edit_event_with_similarity(
        before_hash: Option<&str>,
        after_hash: Option<&str>,
        similarity: Option<f32>,
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
                    before_text: None,
                    after_text: None,
                    before_hash: before_hash.map(ToOwned::to_owned),
                    after_hash: after_hash.map(ToOwned::to_owned),
                    before_anchor_hashes: before_hash
                        .map(|anchor| vec![anchor.to_string()])
                        .unwrap_or_default(),
                    after_anchor_hashes: after_hash
                        .map(|anchor| vec![anchor.to_string()])
                        .unwrap_or_default(),
                    similarity,
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

        let edit_refs = index
            .evidence_for_anchor("after")
            .expect("edit evidence query");
        assert_eq!(edit_refs.len(), 1);
        assert_eq!(edit_refs[0].kind, EvidenceKind::Edit);
        let before_refs = index
            .evidence_for_anchor("before")
            .expect("before evidence query");
        assert_eq!(before_refs.len(), 1);
        assert_eq!(before_refs[0].kind, EvidenceKind::Edit);

        let edges = index
            .outbound_edges("before", 0.50, false)
            .expect("edge query");
        assert_eq!(edges.len(), 1);

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
    fn ingests_windowed_edit_text_as_direct_evidence() {
        let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
        let before_text = (1..=72)
            .map(|line| format!("fn before_{line}() {{ value_{line}(); }}\n"))
            .collect::<String>();
        let after_text = (1..=72)
            .map(|line| format!("fn after_{line}() {{ value_{line}(); }}\n"))
            .collect::<String>();
        // Individual tokens are stored in evidence; window-level anchors are
        // only used for edges.
        let before_tokens = fingerprint_token_hashes(&before_text);
        let after_tokens = fingerprint_token_hashes(&after_text);
        let events = vec![TapeEventAt {
            offset: 1,
            event: TapeEvent {
                timestamp: "2026-02-22T00:00:01Z".to_string(),
                data: TapeEventData::CodeEdit(CodeEditEvent {
                    file: "src/lib.rs".to_string(),
                    before_range: Some(FileRange { start: 10, end: 12 }),
                    after_range: Some(FileRange { start: 10, end: 13 }),
                    before_text: Some(before_text),
                    after_text: Some(after_text),
                    before_hash: None,
                    after_hash: None,
                    before_anchor_hashes: Vec::new(),
                    after_anchor_hashes: Vec::new(),
                    similarity: Some(0.80),
                }),
            },
        }];

        index
            .ingest_tape_events("tape-1", &events, LINK_THRESHOLD_DEFAULT)
            .expect("ingest succeeds");

        assert!(before_tokens.len() >= 3, "tokens={before_tokens:?}");
        assert!(after_tokens.len() >= 3, "tokens={after_tokens:?}");

        let before_refs = index
            .evidence_for_anchor(&before_tokens[0])
            .expect("before winnow token evidence");
        assert_eq!(before_refs.len(), 1);
        assert_eq!(before_refs[0].kind, EvidenceKind::Edit);

        let after_refs = index
            .evidence_for_anchor(&after_tokens[0])
            .expect("after winnow token evidence");
        assert_eq!(after_refs.len(), 1);
        assert_eq!(after_refs[0].kind, EvidenceKind::Edit);
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
    fn ingest_is_idempotent_for_same_tape_events() {
        let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
        let events = vec![
            read_event("read-anchor", "src/lib.rs", 0),
            edit_event(Some("before"), Some("after"), "src/lib.rs", 1),
        ];

        index
            .ingest_tape_events("tape-1", &events, LINK_THRESHOLD_DEFAULT)
            .expect("first ingest");
        index
            .ingest_tape_events("tape-1", &events, LINK_THRESHOLD_DEFAULT)
            .expect("second ingest");

        assert_eq!(
            index
                .evidence_for_anchor("read-anchor")
                .expect("read evidence")
                .len(),
            1
        );
        assert_eq!(
            index
                .evidence_for_anchor("after")
                .expect("edit evidence")
                .len(),
            1
        );
        assert_eq!(
            index
                .outbound_edges("before", 0.0, true)
                .expect("edges")
                .len(),
            1
        );
    }

    #[test]
    fn ingest_rolls_back_when_event_contains_invalid_anchor() {
        let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
        let events = vec![
            read_event("anchor-1", "src/lib.rs", 0),
            read_event("", "src/lib.rs", 1),
        ];

        let err = index.ingest_tape_events("tape-1", &events, LINK_THRESHOLD_DEFAULT);
        assert!(err.is_err());
        assert_eq!(
            index
                .evidence_for_anchor("anchor-1")
                .expect("query after rollback")
                .len(),
            0
        );
    }

    #[test]
    fn location_only_edges_are_hidden_without_forensics_even_with_low_min_confidence() {
        let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
        let events = vec![edit_event_with_similarity(
            Some("before"),
            Some("after"),
            Some(0.20),
            "src/lib.rs",
            1,
        )];

        index
            .ingest_tape_events("tape-1", &events, LINK_THRESHOLD_DEFAULT)
            .expect("ingest succeeds");

        let without_forensics = index
            .outbound_edges("before", 0.10, false)
            .expect("non-forensics query");
        assert_eq!(without_forensics.len(), 0);

        let with_forensics = index
            .outbound_edges("before", 0.10, true)
            .expect("forensics query");
        assert_eq!(with_forensics.len(), 1);
        assert_eq!(
            with_forensics[0].stored_class,
            StoredEdgeClass::LocationOnly
        );
    }

    #[test]
    fn invalid_similarity_rejects_ingest_and_rolls_back() {
        let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
        let events = vec![
            read_event("anchor-1", "src/lib.rs", 0),
            edit_event_with_similarity(Some("a"), Some("b"), Some(1.2), "src/lib.rs", 1),
        ];

        let err = index.ingest_tape_events("tape-1", &events, LINK_THRESHOLD_DEFAULT);
        assert!(err.is_err());
        assert_eq!(
            index
                .evidence_for_anchor("anchor-1")
                .expect("query after rollback")
                .len(),
            0
        );
    }

    #[test]
    fn ingest_persists_dispatch_links_and_queries_by_tape_and_uuid() {
        let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
        let events = vec![read_event("anchor-1", "src/lib.rs", 0)];
        let links = vec![
            DispatchLink {
                uuid: "11111111-1111-4111-8111-111111111111".to_string(),
                first_turn_index: 0,
                direction: DispatchDirection::Received,
            },
            DispatchLink {
                uuid: "22222222-2222-4222-8222-222222222222".to_string(),
                first_turn_index: 3,
                direction: DispatchDirection::Sent,
            },
        ];

        index
            .ingest_tape_events_with_dispatch(
                "tape-dispatch",
                &events,
                &links,
                LINK_THRESHOLD_DEFAULT,
            )
            .expect("ingest dispatch links");

        let by_tape = index
            .dispatch_links_for_tape("tape-dispatch")
            .expect("dispatch links by tape");
        assert_eq!(by_tape.len(), 2);
        assert_eq!(by_tape[0].direction, DispatchDirection::Received);
        assert_eq!(by_tape[1].direction, DispatchDirection::Sent);

        let by_uuid = index
            .dispatch_links_for_uuid("22222222-2222-4222-8222-222222222222")
            .expect("dispatch links by uuid");
        assert_eq!(by_uuid.len(), 1);
        assert_eq!(by_uuid[0].tape_id, "tape-dispatch");
        assert_eq!(by_uuid[0].direction, DispatchDirection::Sent);
    }

    #[test]
    fn latest_received_dispatch_before_turn_selects_most_recent_prior() {
        let index = SqliteIndex::open_in_memory().expect("in-memory sqlite");
        index
            .insert_dispatch_link(
                "tape-a",
                &DispatchLink {
                    uuid: "a".to_string(),
                    first_turn_index: 1,
                    direction: DispatchDirection::Received,
                },
            )
            .expect("insert");
        index
            .insert_dispatch_link(
                "tape-a",
                &DispatchLink {
                    uuid: "b".to_string(),
                    first_turn_index: 7,
                    direction: DispatchDirection::Received,
                },
            )
            .expect("insert");
        index
            .insert_dispatch_link(
                "tape-a",
                &DispatchLink {
                    uuid: "c".to_string(),
                    first_turn_index: 9,
                    direction: DispatchDirection::Sent,
                },
            )
            .expect("insert");

        let link = index
            .latest_received_dispatch_before_turn("tape-a", 8)
            .expect("query")
            .expect("link");
        assert_eq!(link.uuid, "b");
        assert_eq!(link.direction, DispatchDirection::Received);
    }

    #[test]
    fn dispatch_links_are_idempotent_on_repeat_ingest() {
        let index = SqliteIndex::open_in_memory().expect("sqlite");
        let events = vec![read_event("anchor-1", "src/lib.rs", 0)];
        let links = vec![DispatchLink {
            uuid: "33333333-3333-4333-8333-333333333333".to_string(),
            first_turn_index: 2,
            direction: DispatchDirection::Received,
        }];

        index
            .ingest_tape_events_with_dispatch(
                "tape-repeat",
                &events,
                &links,
                LINK_THRESHOLD_DEFAULT,
            )
            .expect("first ingest");
        index
            .ingest_tape_events_with_dispatch(
                "tape-repeat",
                &events,
                &links,
                LINK_THRESHOLD_DEFAULT,
            )
            .expect("second ingest");

        let by_tape = index.dispatch_links_for_tape("tape-repeat").expect("query");
        assert_eq!(by_tape.len(), 1);
        assert_eq!(by_tape[0].uuid, "33333333-3333-4333-8333-333333333333");
    }

    #[test]
    fn sent_dispatch_for_uuid_is_none_when_uuid_is_only_received() {
        let index = SqliteIndex::open_in_memory().expect("sqlite");
        index
            .insert_dispatch_link(
                "tape-r1",
                &DispatchLink {
                    uuid: "44444444-4444-4444-8444-444444444444".to_string(),
                    first_turn_index: 0,
                    direction: DispatchDirection::Received,
                },
            )
            .expect("insert");
        index
            .insert_dispatch_link(
                "tape-r2",
                &DispatchLink {
                    uuid: "44444444-4444-4444-8444-444444444444".to_string(),
                    first_turn_index: 1,
                    direction: DispatchDirection::Received,
                },
            )
            .expect("insert");

        let parent = index
            .sent_dispatch_for_uuid("44444444-4444-4444-8444-444444444444")
            .expect("query parent");
        assert!(parent.is_none());
    }

    #[test]
    fn latest_received_dispatch_before_turn_handles_long_running_tapes() {
        let index = SqliteIndex::open_in_memory().expect("sqlite");
        for i in 0..25 {
            index
                .insert_dispatch_link(
                    "tape-long",
                    &DispatchLink {
                        uuid: format!("long-{i:02}"),
                        first_turn_index: i,
                        direction: DispatchDirection::Received,
                    },
                )
                .expect("insert");
        }

        let picked = index
            .latest_received_dispatch_before_turn("tape-long", 21)
            .expect("query")
            .expect("picked");
        assert_eq!(picked.first_turn_index, 20);
        assert_eq!(picked.uuid, "long-20");
    }

    #[test]
    fn latest_received_dispatch_before_turn_returns_none_when_edit_precedes_dispatch() {
        let index = SqliteIndex::open_in_memory().expect("sqlite");
        index
            .insert_dispatch_link(
                "tape-pre",
                &DispatchLink {
                    uuid: "later-dispatch".to_string(),
                    first_turn_index: 10,
                    direction: DispatchDirection::Received,
                },
            )
            .expect("insert");

        let picked = index
            .latest_received_dispatch_before_turn("tape-pre", 3)
            .expect("query");
        assert!(picked.is_none());
    }

    #[test]
    fn query_results_and_feedback_round_trip() {
        let index = SqliteIndex::open_in_memory().expect("sqlite");
        index
            .record_query_result(
                "result_123",
                "explain",
                "{\"query\":{\"command\":\"explain\"}}",
                "2026-04-03T00:00:00Z",
            )
            .expect("record query result");

        assert!(
            index
                .query_result_exists("result_123")
                .expect("query result exists"),
            "expected recorded result to be queryable"
        );

        index
            .upsert_result_feedback(
                "result_123",
                "found_answer",
                Some("prevented a bad edit"),
                "2026-04-03T00:01:00Z",
            )
            .expect("insert feedback");
        index
            .upsert_result_feedback(
                "result_123",
                "partially_helped",
                None,
                "2026-04-03T00:02:00Z",
            )
            .expect("update feedback");

        let (outcome, note, rated_at): (String, Option<String>, String) = index
            .conn
            .query_row(
                "SELECT outcome, note, rated_at FROM result_feedback WHERE result_id = ?1",
                params!["result_123"],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("feedback row");
        assert_eq!(outcome, "partially_helped");
        assert_eq!(note, None);
        assert_eq!(rated_at, "2026-04-03T00:02:00Z");
    }
}
