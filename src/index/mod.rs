pub mod lineage;

use std::collections::HashSet;
use std::ops::Deref;
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::anchor::{
    FingerprintedWindow, fingerprint_similarity, fingerprint_text, fingerprint_windows,
};
use crate::index::lineage::{
    Cardinality, EvidenceFragmentRef, EvidenceKind, LINK_THRESHOLD_DEFAULT, LocationDelta,
    SpanEdge, StoredEdgeClass, Tombstone,
};
use crate::tape::event::{FileRange, TapeEventAt, TapeEventData};

const SCHEMA_VERSION: i64 = 4;
const EXACT_EVIDENCE_SQL: &str =
    "SELECT evidence_id, anchor, tape_id, event_offset, kind, file_path, timestamp
     FROM evidence_windows
     WHERE anchor = ?1";
const FEATURE_EVIDENCE_SQL: &str = "SELECT w.evidence_id, w.anchor, w.tape_id, w.event_offset,
            w.kind, w.file_path, w.timestamp
     FROM evidence_features f
     JOIN evidence_windows w ON w.evidence_id = f.evidence_id
     WHERE f.feature_hash = ?1";
const FEATURE_WINDOW_ANCHORS_SQL: &str = "SELECT w.anchor
     FROM evidence_features f
     JOIN evidence_windows w ON w.evidence_id = f.evidence_id
     WHERE f.feature_hash = ?1";

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
pub enum EdgeSourceKind {
    Edit,
    SpanLink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeSource {
    pub source_kind: EdgeSourceKind,
    pub tape_id: String,
    pub event_offset: u64,
    pub pair_ordinal: u32,
    pub from_window_ordinal: i64,
    pub to_window_ordinal: i64,
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
    access_kind: AccessKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AccessKind {
    Reader,
    Writer,
}

impl SqliteIndex {
    pub fn open_writer(path: &str) -> rusqlite::Result<Self> {
        let existed = Path::new(path).exists();
        let conn = Connection::open(path)?;
        let index = Self {
            conn,
            access_kind: AccessKind::Writer,
        };
        let version = index.user_version()?;
        if existed && version != SCHEMA_VERSION {
            return Err(rusqlite::Error::InvalidQuery);
        }
        index.configure_writer()?;
        if !existed {
            index.create_schema_v4()?;
        }
        Ok(index)
    }

    pub fn open_reader(path: &str) -> rusqlite::Result<Self> {
        let wal_path = format!("{path}-wal");
        let shm_path = format!("{path}-shm");
        let conn = if Path::new(&wal_path).exists() || Path::new(&shm_path).exists() {
            Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?
        } else {
            let absolute = std::fs::canonicalize(path)
                .map_err(|_| rusqlite::Error::InvalidPath(Path::new(path).to_path_buf()))?;
            let uri = format!(
                "file:{}?mode=ro&immutable=1",
                encode_sqlite_uri_path(&absolute.to_string_lossy())
            );
            Connection::open_with_flags(
                uri,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
            )?
        };
        let index = Self {
            conn,
            access_kind: AccessKind::Reader,
        };
        if index.user_version()? != SCHEMA_VERSION {
            return Err(rusqlite::Error::InvalidQuery);
        }
        Ok(index)
    }

    pub fn open_in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        let index = Self {
            conn,
            access_kind: AccessKind::Writer,
        };
        index.configure_writer()?;
        index.create_schema_v4()?;
        Ok(index)
    }

    pub fn with_read_transaction<T>(
        &self,
        operation: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        if self.access_kind != AccessKind::Reader {
            return Err(rusqlite::Error::InvalidQuery);
        }
        self.conn.execute_batch("BEGIN DEFERRED")?;
        match operation(&self.conn) {
            Ok(value) => match self.conn.execute_batch("COMMIT") {
                Ok(()) => Ok(value),
                Err(error) => {
                    let _ = self.conn.execute_batch("ROLLBACK");
                    Err(error)
                }
            },
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    fn configure_writer(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = FULL;
            ",
        )
    }

    fn user_version(&self) -> rusqlite::Result<i64> {
        self.conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
    }

    fn create_schema_v4(&self) -> rusqlite::Result<()> {
        self.conn.execute_batch(
            "
            CREATE TABLE evidence_windows (
              evidence_id INTEGER PRIMARY KEY,
              anchor TEXT NOT NULL,
              tape_id TEXT NOT NULL,
              event_offset INTEGER NOT NULL,
              kind TEXT NOT NULL CHECK (kind IN ('read','edit')),
              file_path TEXT NOT NULL,
              timestamp TEXT NOT NULL,
              window_ordinal INTEGER NOT NULL,
              UNIQUE(tape_id, event_offset, kind, window_ordinal)
            );

            CREATE TABLE evidence_features (
              feature_hash TEXT NOT NULL,
              evidence_id INTEGER NOT NULL REFERENCES evidence_windows(evidence_id) ON DELETE CASCADE,
              PRIMARY KEY(feature_hash, evidence_id)
            ) WITHOUT ROWID;
            CREATE INDEX idx_evidence_windows_anchor ON evidence_windows(anchor);

            CREATE TABLE edges (
              edge_id INTEGER PRIMARY KEY,
              source_kind TEXT NOT NULL CHECK (source_kind IN ('edit','span_link')),
              tape_id TEXT NOT NULL,
              event_offset INTEGER NOT NULL,
              pair_ordinal INTEGER NOT NULL,
              from_window_ordinal INTEGER NOT NULL,
              to_window_ordinal INTEGER NOT NULL,
              from_anchor TEXT NOT NULL,
              to_anchor TEXT NOT NULL,
              confidence REAL NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
              location_delta TEXT NOT NULL,
              cardinality TEXT NOT NULL,
              agent_link INTEGER NOT NULL CHECK (agent_link IN (0,1)),
              note TEXT NOT NULL DEFAULT '',
              UNIQUE(tape_id, event_offset, source_kind, pair_ordinal)
            );
            CREATE INDEX idx_edges_from_anchor ON edges(from_anchor);
            CREATE INDEX idx_edges_to_anchor ON edges(to_anchor);

            CREATE TABLE tombstones (
              tombstone_id INTEGER PRIMARY KEY,
              anchor TEXT NOT NULL,
              tape_id TEXT NOT NULL,
              event_offset INTEGER NOT NULL,
              file_path TEXT NOT NULL,
              range_start INTEGER NOT NULL,
              range_end INTEGER NOT NULL,
              timestamp TEXT NOT NULL,
              window_ordinal INTEGER NOT NULL,
              UNIQUE(tape_id, event_offset, window_ordinal)
            );
            CREATE TABLE tombstone_features (
              feature_hash TEXT NOT NULL,
              tombstone_id INTEGER NOT NULL REFERENCES tombstones(tombstone_id) ON DELETE CASCADE,
              PRIMARY KEY(feature_hash, tombstone_id)
            ) WITHOUT ROWID;
            CREATE INDEX idx_tombstones_anchor ON tombstones(anchor);

            CREATE TABLE tapes (tape_id TEXT PRIMARY KEY);

            CREATE TABLE dispatch_links (
              tape_id TEXT NOT NULL,
              uuid TEXT NOT NULL,
              first_turn_index INTEGER NOT NULL,
              direction TEXT NOT NULL CHECK(direction IN ('received','sent')),
              PRIMARY KEY(tape_id, uuid)
            );
            CREATE INDEX idx_dispatch_links_uuid ON dispatch_links(uuid);
            CREATE INDEX idx_dispatch_links_tape ON dispatch_links(tape_id);
            CREATE INDEX idx_dispatch_links_received
              ON dispatch_links(tape_id, direction, first_turn_index);

            PRAGMA user_version = 4;
            ",
        )
    }

    fn insert_evidence_window_on(
        conn: &Connection,
        window: &FingerprintedWindow,
        fragment: &EvidenceFragmentRef,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT OR IGNORE INTO evidence_windows
             (anchor, tape_id, event_offset, kind, file_path, timestamp, window_ordinal)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                window.anchor,
                fragment.tape_id,
                fragment.event_offset,
                encode_evidence_kind(fragment.kind),
                fragment.file_path,
                fragment.timestamp,
                window.ordinal
            ],
        )?;
        let evidence_id: i64 = conn.query_row(
            "SELECT evidence_id FROM evidence_windows
             WHERE tape_id = ?1 AND event_offset = ?2 AND kind = ?3 AND window_ordinal = ?4",
            params![
                fragment.tape_id,
                fragment.event_offset,
                encode_evidence_kind(fragment.kind),
                window.ordinal
            ],
            |row| row.get(0),
        )?;
        for feature in &window.features {
            conn.execute(
                "INSERT OR IGNORE INTO evidence_features (feature_hash, evidence_id)
                 VALUES (?1, ?2)",
                params![feature, evidence_id],
            )?;
        }
        Ok(())
    }

    pub fn insert_edge(&self, source: &EdgeSource, edge: &SpanEdge) -> rusqlite::Result<()> {
        Self::insert_edge_on(&self.conn, source, edge)
    }

    fn insert_edge_on(
        conn: &Connection,
        source: &EdgeSource,
        edge: &SpanEdge,
    ) -> rusqlite::Result<()> {
        Self::validate_anchor(&edge.from_anchor)?;
        Self::validate_anchor(&edge.to_anchor)?;
        Self::validate_confidence(edge.confidence)?;
        conn.execute(
            "INSERT OR IGNORE INTO edges (
                source_kind, tape_id, event_offset, pair_ordinal,
                from_window_ordinal, to_window_ordinal,
                from_anchor, to_anchor, confidence, location_delta, cardinality,
                agent_link, note
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                encode_edge_source_kind(source.source_kind),
                source.tape_id,
                source.event_offset,
                source.pair_ordinal,
                source.from_window_ordinal,
                source.to_window_ordinal,
                edge.from_anchor,
                edge.to_anchor,
                edge.confidence,
                encode_location_delta(edge.location_delta),
                encode_cardinality(edge.cardinality),
                i64::from(edge.agent_link),
                edge.note.as_deref().unwrap_or("")
            ],
        )?;
        Ok(())
    }

    fn insert_tombstone_window_on(
        conn: &Connection,
        window: &FingerprintedWindow,
        tombstone: &Tombstone,
    ) -> rusqlite::Result<()> {
        conn.execute(
            "INSERT OR IGNORE INTO tombstones (
                anchor, tape_id, event_offset, file_path, range_start, range_end,
                timestamp, window_ordinal
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                window.anchor,
                tombstone.tape_id,
                tombstone.event_offset,
                tombstone.file_path,
                tombstone.range_at_deletion.start,
                tombstone.range_at_deletion.end,
                tombstone.timestamp,
                window.ordinal
            ],
        )?;
        let tombstone_id: i64 = conn.query_row(
            "SELECT tombstone_id FROM tombstones
             WHERE tape_id = ?1 AND event_offset = ?2 AND window_ordinal = ?3",
            params![tombstone.tape_id, tombstone.event_offset, window.ordinal],
            |row| row.get(0),
        )?;
        for feature in &window.features {
            conn.execute(
                "INSERT OR IGNORE INTO tombstone_features (feature_hash, tombstone_id)
                 VALUES (?1, ?2)",
                params![feature, tombstone_id],
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

    pub fn evidence_for_anchor(&self, anchor: &str) -> rusqlite::Result<Vec<EvidenceFragmentRef>> {
        let mut matches = self.evidence_window_matches(anchor)?;
        sort_evidence_window_matches(&mut matches);
        Ok(matches
            .into_iter()
            .map(|(_, _, fragment)| fragment)
            .collect())
    }

    pub fn evidence_for_anchors(
        &self,
        anchors: &[String],
    ) -> rusqlite::Result<Vec<EvidenceFragmentRef>> {
        let mut seen_evidence_ids = HashSet::new();
        let mut out = Vec::new();
        for anchor in anchors {
            for (evidence_id, _, fragment) in self.evidence_window_matches(anchor)? {
                if seen_evidence_ids.insert(evidence_id) {
                    out.push(fragment);
                }
            }
        }
        out.sort_by(|a, b| {
            a.timestamp
                .cmp(&b.timestamp)
                .then_with(|| a.tape_id.cmp(&b.tape_id))
                .then_with(|| a.event_offset.cmp(&b.event_offset))
        });
        Ok(out)
    }

    fn evidence_window_matches(
        &self,
        anchor: &str,
    ) -> rusqlite::Result<Vec<(i64, String, EvidenceFragmentRef)>> {
        if anchor.starts_with("span:") || !anchor.starts_with("winnow:") {
            return Ok(Vec::new());
        }
        let sql = if anchor.contains(',') {
            EXACT_EVIDENCE_SQL
        } else {
            FEATURE_EVIDENCE_SQL
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(params![anchor], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                EvidenceFragmentRef {
                    tape_id: row.get(2)?,
                    event_offset: row.get(3)?,
                    kind: decode_evidence_kind(&row.get::<_, String>(4)?),
                    file_path: row.get(5)?,
                    timestamp: row.get(6)?,
                },
            ))
        })?;
        rows.collect()
    }

    pub fn matching_window_anchors(&self, anchor: &str) -> rusqlite::Result<Vec<String>> {
        if anchor.starts_with("span:") {
            return Ok(vec![anchor.to_string()]);
        }
        if !anchor.starts_with("winnow:") {
            return Ok(Vec::new());
        }
        if anchor.contains(',') {
            return Ok(vec![anchor.to_string()]);
        }
        let mut stmt = self.conn.prepare(FEATURE_WINDOW_ANCHORS_SQL)?;
        let rows = stmt.query_map(params![anchor], |row| row.get(0))?;
        let mut anchors = rows.collect::<rusqlite::Result<HashSet<String>>>()?;
        let mut anchors = anchors.drain().collect::<Vec<_>>();
        anchors.sort();
        Ok(anchors)
    }

    pub fn window_anchor_stats_for_file(
        &self,
        file_path: &str,
    ) -> rusqlite::Result<Vec<(String, u64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT anchor, COUNT(*) AS hits
             FROM evidence_windows
             WHERE file_path = ?1
             GROUP BY anchor
             ORDER BY hits DESC, anchor ASC",
        )?;
        stmt.query_map(params![file_path], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }

    pub fn outbound_edges(
        &self,
        from_anchor: &str,
        min_confidence: f32,
        include_forensics: bool,
    ) -> rusqlite::Result<Vec<EdgeRow>> {
        self.edges_for_anchor(
            "from_anchor",
            from_anchor,
            min_confidence,
            include_forensics,
        )
    }

    pub fn inbound_edges(
        &self,
        to_anchor: &str,
        min_confidence: f32,
        include_forensics: bool,
    ) -> rusqlite::Result<Vec<EdgeRow>> {
        self.edges_for_anchor("to_anchor", to_anchor, min_confidence, include_forensics)
    }

    fn edges_for_anchor(
        &self,
        column: &str,
        anchor: &str,
        min_confidence: f32,
        include_forensics: bool,
    ) -> rusqlite::Result<Vec<EdgeRow>> {
        let sql = format!(
            "SELECT from_anchor, to_anchor, confidence, location_delta, cardinality,
                    agent_link, note
             FROM edges
             WHERE {column} = ?1
             ORDER BY confidence DESC, edge_id ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params![anchor])?;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let edge = decode_edge_row(row)?;
            if !seen.insert(semantic_edge_key(&edge)) {
                continue;
            }
            if !include_forensics
                && !edge.agent_link
                && (edge.stored_class == StoredEdgeClass::LocationOnly
                    || edge.confidence < min_confidence)
            {
                continue;
            }
            out.push(edge);
        }
        Ok(out)
    }

    pub fn tombstones_for_anchor(&self, anchor: &str) -> rusqlite::Result<Vec<Tombstone>> {
        if anchor.starts_with("span:") || !anchor.starts_with("winnow:") {
            return Ok(Vec::new());
        }
        let sql = if anchor.contains(',') {
            "SELECT anchor, tape_id, event_offset, file_path, range_start, range_end, timestamp
             FROM tombstones
             WHERE anchor = ?1
             ORDER BY timestamp ASC, tape_id ASC, event_offset ASC, window_ordinal ASC"
        } else {
            "SELECT t.anchor, t.tape_id, t.event_offset, t.file_path,
                    t.range_start, t.range_end, t.timestamp
             FROM tombstone_features f
             JOIN tombstones t ON t.tombstone_id = f.tombstone_id
             WHERE f.feature_hash = ?1
             ORDER BY t.timestamp ASC, t.tape_id ASC, t.event_offset ASC, t.window_ordinal ASC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let mut rows = stmt.query(params![anchor])?;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let tombstone = Tombstone {
                anchor_hashes: vec![row.get(0)?],
                tape_id: row.get(1)?,
                event_offset: row.get(2)?,
                file_path: row.get(3)?,
                range_at_deletion: FileRange {
                    start: row.get(4)?,
                    end: row.get(5)?,
                },
                timestamp: row.get(6)?,
            };
            let key = (
                tombstone.tape_id.clone(),
                tombstone.event_offset,
                tombstone.file_path.clone(),
                tombstone.range_at_deletion.start,
                tombstone.range_at_deletion.end,
                tombstone.timestamp.clone(),
            );
            if seen.insert(key) {
                out.push(tombstone);
            }
        }
        Ok(out)
    }

    pub fn referenced_tape_ids(&self) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT tape_id FROM evidence_windows
             UNION
             SELECT tape_id FROM tombstones",
        )?;
        stmt.query_map([], |row| row.get(0))?.collect()
    }

    pub fn has_tape(&self, tape_id: &str) -> rusqlite::Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT 1 FROM tapes WHERE tape_id = ?1 LIMIT 1",
                params![tape_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
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
        _link_threshold: f32,
    ) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        for item in events {
            match &item.event.data {
                TapeEventData::CodeRead(read) => {
                    let Some(text) = read.text.as_deref() else {
                        continue;
                    };
                    let fragment = EvidenceFragmentRef {
                        tape_id: tape_id.to_string(),
                        event_offset: item.offset,
                        kind: EvidenceKind::Read,
                        file_path: read.file.clone(),
                        timestamp: item.event.timestamp.clone(),
                    };
                    for window in fingerprint_windows(text) {
                        Self::insert_evidence_window_on(tx.deref(), &window, &fragment)?;
                    }
                }
                TapeEventData::CodeEdit(edit) => {
                    let before_windows = edit
                        .before_text
                        .as_deref()
                        .map(fingerprint_windows)
                        .unwrap_or_default();
                    let after_windows = edit
                        .after_text
                        .as_deref()
                        .map(fingerprint_windows)
                        .unwrap_or_default();

                    let fragment = EvidenceFragmentRef {
                        tape_id: tape_id.to_string(),
                        event_offset: item.offset,
                        kind: EvidenceKind::Edit,
                        file_path: edit.file.clone(),
                        timestamp: item.event.timestamp.clone(),
                    };
                    for window in &after_windows {
                        Self::insert_evidence_window_on(tx.deref(), window, &fragment)?;
                    }

                    if !before_windows.is_empty() && !after_windows.is_empty() {
                        let confidence = edit_similarity(edit);
                        Self::validate_confidence(confidence)?;
                        for (pair_ordinal, (before, after)) in
                            proportional_window_pairs(&before_windows, &after_windows)
                                .into_iter()
                                .enumerate()
                        {
                            Self::insert_edge_on(
                                tx.deref(),
                                &EdgeSource {
                                    source_kind: EdgeSourceKind::Edit,
                                    tape_id: tape_id.to_string(),
                                    event_offset: item.offset,
                                    pair_ordinal: pair_ordinal as u32,
                                    from_window_ordinal: i64::from(before.ordinal),
                                    to_window_ordinal: i64::from(after.ordinal),
                                },
                                &SpanEdge {
                                    from_anchor: before.anchor.clone(),
                                    to_anchor: after.anchor.clone(),
                                    confidence,
                                    location_delta: LocationDelta::Same,
                                    cardinality: Cardinality::OneToOne,
                                    agent_link: false,
                                    note: None,
                                },
                            )?;
                        }
                    } else if !before_windows.is_empty() && after_windows.is_empty() {
                        let range = edit
                            .before_range
                            .or(edit.after_range)
                            .unwrap_or(FileRange { start: 0, end: 0 });
                        let tombstone = Tombstone {
                            anchor_hashes: Vec::new(),
                            tape_id: tape_id.to_string(),
                            event_offset: item.offset,
                            file_path: edit.file.clone(),
                            range_at_deletion: range,
                            timestamp: item.event.timestamp.clone(),
                        };
                        for window in &before_windows {
                            Self::insert_tombstone_window_on(tx.deref(), window, &tombstone)?;
                        }
                    }
                }
                TapeEventData::SpanLink(link) => {
                    Self::insert_edge_on(
                        tx.deref(),
                        &EdgeSource {
                            source_kind: EdgeSourceKind::SpanLink,
                            tape_id: tape_id.to_string(),
                            event_offset: item.offset,
                            pair_ordinal: 0,
                            from_window_ordinal: -1,
                            to_window_ordinal: -1,
                        },
                        &SpanEdge {
                            from_anchor: encode_span_link_anchor(&link.from_file, link.from_range),
                            to_anchor: encode_span_link_anchor(&link.to_file, link.to_range),
                            confidence: 1.0,
                            location_delta: LocationDelta::Moved,
                            cardinality: Cardinality::OneToOne,
                            agent_link: true,
                            note: link.note.clone(),
                        },
                    )?;
                }
                TapeEventData::Textual(_)
                | TapeEventData::Meta(_)
                | TapeEventData::Other { .. } => {}
            }
        }

        for link in dispatch_links {
            Self::insert_dispatch_link_on(tx.deref(), tape_id, link)?;
        }
        tx.execute(
            "INSERT OR IGNORE INTO tapes (tape_id) VALUES (?1)",
            params![tape_id],
        )?;
        tx.commit()
    }

    pub fn dispatch_links_for_tape(&self, tape_id: &str) -> rusqlite::Result<Vec<DispatchLink>> {
        let mut stmt = self.conn.prepare(
            "SELECT uuid, first_turn_index, direction
             FROM dispatch_links
             WHERE tape_id = ?1
             ORDER BY first_turn_index ASC, uuid ASC",
        )?;
        stmt.query_map(params![tape_id], |row| {
            Ok(DispatchLink {
                uuid: row.get(0)?,
                first_turn_index: row.get(1)?,
                direction: decode_dispatch_direction(&row.get::<_, String>(2)?),
            })
        })?
        .collect()
    }

    pub fn dispatch_links_for_uuid(&self, uuid: &str) -> rusqlite::Result<Vec<DispatchLinkRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT tape_id, uuid, first_turn_index, direction
             FROM dispatch_links
             WHERE uuid = ?1
             ORDER BY first_turn_index ASC, tape_id ASC",
        )?;
        stmt.query_map(params![uuid], decode_dispatch_link_row)?
            .collect()
    }

    pub fn latest_received_dispatch_before_turn(
        &self,
        tape_id: &str,
        turn_index: i64,
    ) -> rusqlite::Result<Option<DispatchLink>> {
        self.conn
            .query_row(
                "SELECT uuid, first_turn_index, direction
                 FROM dispatch_links
                 WHERE tape_id = ?1 AND direction = 'received' AND first_turn_index < ?2
                 ORDER BY first_turn_index DESC, uuid ASC
                 LIMIT 1",
                params![tape_id, turn_index],
                |row| {
                    Ok(DispatchLink {
                        uuid: row.get(0)?,
                        first_turn_index: row.get(1)?,
                        direction: decode_dispatch_direction(&row.get::<_, String>(2)?),
                    })
                },
            )
            .optional()
    }

    pub fn sent_dispatch_for_uuid(&self, uuid: &str) -> rusqlite::Result<Option<DispatchLinkRow>> {
        self.conn
            .query_row(
                "SELECT tape_id, uuid, first_turn_index, direction
                 FROM dispatch_links
                 WHERE uuid = ?1 AND direction = 'sent'
                 ORDER BY first_turn_index DESC, tape_id ASC
                 LIMIT 1",
                params![uuid],
                decode_dispatch_link_row,
            )
            .optional()
    }

    fn validate_anchor(anchor: &str) -> rusqlite::Result<()> {
        if anchor.is_empty() {
            return Err(rusqlite::Error::InvalidParameterName(
                "anchor must not be empty".to_string(),
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

    #[cfg(test)]
    fn scalar_i64(&self, sql: &str) -> i64 {
        self.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }
}

fn encode_sqlite_uri_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b'~' | b':' => {
                out.push(char::from(byte))
            }
            _ => {
                use std::fmt::Write as _;
                let _ = write!(&mut out, "%{byte:02X}");
            }
        }
    }
    out
}

fn proportional_window_pairs<'a>(
    before: &'a [FingerprintedWindow],
    after: &'a [FingerprintedWindow],
) -> Vec<(&'a FingerprintedWindow, &'a FingerprintedWindow)> {
    let pair_count = before.len().max(after.len());
    (0..pair_count)
        .map(|index| {
            let before_index = index * before.len() / pair_count;
            let after_index = index * after.len() / pair_count;
            (&before[before_index], &after[after_index])
        })
        .collect()
}

fn edit_similarity(edit: &crate::tape::event::CodeEditEvent) -> f32 {
    match (edit.before_text.as_deref(), edit.after_text.as_deref()) {
        (Some(before), Some(after)) => {
            let before = fingerprint_text(before);
            let after = fingerprint_text(after);
            fingerprint_similarity(&before.fingerprint, &after.fingerprint)
                .or(edit.similarity)
                .unwrap_or(0.0)
        }
        _ => edit.similarity.unwrap_or(0.0),
    }
}

fn encode_span_link_anchor(file: &str, range: FileRange) -> String {
    format!("span:{file}:{}-{}", range.start, range.end)
}

pub(crate) fn semantic_edge_key(
    edge: &EdgeRow,
) -> (
    String,
    String,
    u32,
    LocationDelta,
    Cardinality,
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

fn decode_edge_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<EdgeRow> {
    let confidence = row.get(2)?;
    let agent_link = row.get::<_, i64>(5)? != 0;
    let note: String = row.get(6)?;
    Ok(EdgeRow {
        from_anchor: row.get(0)?,
        to_anchor: row.get(1)?,
        confidence,
        location_delta: decode_location_delta(&row.get::<_, String>(3)?),
        cardinality: decode_cardinality(&row.get::<_, String>(4)?),
        agent_link,
        note: (!note.is_empty()).then_some(note),
        stored_class: derive_stored_class(agent_link, confidence),
    })
}

fn decode_dispatch_link_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<DispatchLinkRow> {
    Ok(DispatchLinkRow {
        tape_id: row.get(0)?,
        uuid: row.get(1)?,
        first_turn_index: row.get(2)?,
        direction: decode_dispatch_direction(&row.get::<_, String>(3)?),
    })
}

fn encode_evidence_kind(kind: EvidenceKind) -> &'static str {
    match kind {
        EvidenceKind::Edit => "edit",
        EvidenceKind::Read => "read",
    }
}

fn decode_evidence_kind(raw: &str) -> EvidenceKind {
    match raw {
        "edit" => EvidenceKind::Edit,
        _ => EvidenceKind::Read,
    }
}

fn encode_edge_source_kind(kind: EdgeSourceKind) -> &'static str {
    match kind {
        EdgeSourceKind::Edit => "edit",
        EdgeSourceKind::SpanLink => "span_link",
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

fn sort_evidence_window_matches(matches: &mut [(i64, String, EvidenceFragmentRef)]) {
    matches.sort_by(|left, right| {
        left.2
            .timestamp
            .cmp(&right.2.timestamp)
            .then_with(|| left.2.tape_id.cmp(&right.2.tape_id))
            .then_with(|| left.2.event_offset.cmp(&right.2.event_offset))
            .then_with(|| left.0.cmp(&right.0))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tape::event::{CodeEditEvent, CodeReadEvent, EventKind, TapeEvent, TextualEvent};

    fn source_text(prefix: &str, lines: usize) -> String {
        (1..=lines)
            .map(|line| format!("fn {prefix}_{line}() {{ value_{line}(); }}\n"))
            .collect()
    }

    fn event(offset: u64, data: TapeEventData) -> TapeEventAt {
        TapeEventAt {
            offset,
            event: TapeEvent {
                timestamp: format!("2026-07-29T00:00:{offset:02}Z"),
                data,
            },
        }
    }

    fn query_plan(index: &SqliteIndex, sql: &str) -> Vec<String> {
        let mut stmt = index
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        stmt.query_map(params!["winnow:feature"], |row| row.get(3))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap()
    }

    #[test]
    fn schema_v4_uses_wide_windows_and_narrow_postings_only() {
        let index = SqliteIndex::open_in_memory().unwrap();
        assert_eq!(index.scalar_i64("PRAGMA user_version"), 4);
        assert_eq!(
            index.scalar_i64(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name IN ('query_results','result_feedback','evidence')"
            ),
            0
        );
    }

    #[test]
    fn direct_touch_plans_use_bounded_indexes_without_temporary_sorting() {
        let index = SqliteIndex::open_in_memory().unwrap();

        let exact = query_plan(&index, EXACT_EVIDENCE_SQL);
        assert!(
            exact
                .iter()
                .any(|detail| detail.contains("idx_evidence_windows_anchor")),
            "exact plan must use the composite-anchor index: {exact:?}"
        );

        for (name, plan) in [
            ("feature evidence", query_plan(&index, FEATURE_EVIDENCE_SQL)),
            (
                "feature window anchors",
                query_plan(&index, FEATURE_WINDOW_ANCHORS_SQL),
            ),
        ] {
            assert!(
                plan.iter()
                    .any(|detail| detail.contains("PRIMARY KEY (feature_hash=?)")),
                "{name} plan must use the posting-table primary key: {plan:?}"
            );
            assert!(
                plan.iter()
                    .any(|detail| detail.contains("INTEGER PRIMARY KEY (rowid=?)")),
                "{name} plan must use evidence-window primary-key lookup: {plan:?}"
            );
        }

        for (name, plan) in [
            ("exact evidence", exact),
            ("feature evidence", query_plan(&index, FEATURE_EVIDENCE_SQL)),
            (
                "feature window anchors",
                query_plan(&index, FEATURE_WINDOW_ANCHORS_SQL),
            ),
        ] {
            assert!(
                plan.iter().all(|detail| {
                    !detail.contains("USE TEMP B-TREE")
                        && !detail.contains("AUTOMATIC")
                        && !detail.contains("SCAN evidence_windows")
                }),
                "{name} plan must not scan or sort direct touches: {plan:?}"
            );
        }
    }

    #[test]
    fn ingest_stores_physical_windows_without_cross_window_dedupe() {
        let index = SqliteIndex::open_in_memory().unwrap();
        let block = source_text("repeat", 24);
        let text = format!("{block}{block}");
        index
            .ingest_tape_events(
                "tape",
                &[event(
                    1,
                    TapeEventData::CodeRead(CodeReadEvent {
                        file: "src/lib.rs".into(),
                        range: FileRange { start: 1, end: 48 },
                        text: Some(text),
                        anchor_hashes: Vec::new(),
                    }),
                )],
                LINK_THRESHOLD_DEFAULT,
            )
            .unwrap();
        assert_eq!(index.scalar_i64("SELECT COUNT(*) FROM evidence_windows"), 3);
        assert_eq!(
            index.scalar_i64(
                "SELECT COUNT(*) FROM evidence_windows
                 WHERE window_ordinal IN (0, 2)"
            ),
            2
        );
    }

    #[test]
    fn evidence_is_after_edit_and_read_only() {
        let index = SqliteIndex::open_in_memory().unwrap();
        let before = source_text("before", 24);
        let after = source_text("after", 24);
        let tool = source_text("tool", 24);
        let before_feature = fingerprint_windows(&before)[0].features[0].clone();
        index
            .ingest_tape_events(
                "tape",
                &[
                    event(
                        1,
                        TapeEventData::CodeEdit(CodeEditEvent {
                            file: "src/lib.rs".into(),
                            before_range: None,
                            after_range: None,
                            before_text: Some(before),
                            after_text: Some(after),
                            before_hash: None,
                            after_hash: None,
                            before_anchor_hashes: Vec::new(),
                            after_anchor_hashes: Vec::new(),
                            similarity: None,
                        }),
                    ),
                    event(
                        2,
                        TapeEventData::Textual(TextualEvent {
                            kind: EventKind::ToolResult,
                            text: tool,
                        }),
                    ),
                ],
                LINK_THRESHOLD_DEFAULT,
            )
            .unwrap();
        assert_eq!(index.scalar_i64("SELECT COUNT(*) FROM evidence_windows"), 1);
        assert_eq!(
            index.scalar_i64("SELECT COUNT(*) FROM evidence_windows WHERE kind = 'edit'"),
            1
        );
        assert!(
            index
                .evidence_for_anchor(&before_feature)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn physical_equal_edges_survive_and_queries_semantically_dedupe() {
        let index = SqliteIndex::open_in_memory().unwrap();
        let edge = SpanEdge {
            from_anchor: "winnow:a,b".into(),
            to_anchor: "winnow:c,d".into(),
            confidence: 0.8,
            location_delta: LocationDelta::Same,
            cardinality: Cardinality::OneToOne,
            agent_link: false,
            note: None,
        };
        for pair_ordinal in 0..2 {
            index
                .insert_edge(
                    &EdgeSource {
                        source_kind: EdgeSourceKind::Edit,
                        tape_id: "tape".into(),
                        event_offset: 7,
                        pair_ordinal,
                        from_window_ordinal: i64::from(pair_ordinal),
                        to_window_ordinal: i64::from(pair_ordinal),
                    },
                    &edge,
                )
                .unwrap();
        }
        assert_eq!(index.scalar_i64("SELECT COUNT(*) FROM edges"), 2);
        assert_eq!(
            index
                .outbound_edges("winnow:a,b", 0.5, false)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn writer_rejects_schema_v3_without_mutating_it() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE legacy_marker(value TEXT);
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let before = std::fs::read(&path).unwrap();

        assert!(SqliteIndex::open_writer(path.to_str().unwrap()).is_err());

        assert_eq!(std::fs::read(&path).unwrap(), before);
        let conn = Connection::open(&path).unwrap();
        assert_eq!(
            conn.query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table' AND name = 'legacy_marker'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn reader_opens_schema_v4_without_wal_shm_or_file_mutation() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite");
        {
            let writer = SqliteIndex::open_writer(path.to_str().unwrap()).unwrap();
            assert_eq!(writer.user_version().unwrap(), 4);
        }
        let before = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        {
            let reader = SqliteIndex::open_reader(path.to_str().unwrap()).unwrap();
            assert!(reader.referenced_tape_ids().unwrap().is_empty());
        }

        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!path.with_extension("sqlite-wal").exists());
        assert!(!path.with_extension("sqlite-shm").exists());
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn read_transaction_pins_snapshot_while_writer_commits() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite");
        let writer = SqliteIndex::open_writer(path.to_str().unwrap()).unwrap();
        writer
            .conn
            .execute("INSERT INTO tapes (tape_id) VALUES ('before')", [])
            .unwrap();
        let reader = SqliteIndex::open_reader(path.to_str().unwrap()).unwrap();

        reader
            .with_read_transaction(|conn| {
                let before =
                    conn.query_row("SELECT COUNT(*) FROM tapes", [], |row| row.get::<_, i64>(0))?;
                assert_eq!(before, 1);

                writer
                    .conn
                    .execute("INSERT INTO tapes (tape_id) VALUES ('during')", [])?;

                let pinned =
                    conn.query_row("SELECT COUNT(*) FROM tapes", [], |row| row.get::<_, i64>(0))?;
                assert_eq!(pinned, 1, "reader must retain its established snapshot");
                Ok(())
            })
            .unwrap();

        let fresh_reader = SqliteIndex::open_reader(path.to_str().unwrap()).unwrap();
        assert_eq!(
            fresh_reader
                .with_read_transaction(|conn| {
                    conn.query_row("SELECT COUNT(*) FROM tapes", [], |row| row.get::<_, i64>(0))
                })
                .unwrap(),
            2,
            "a fresh transaction must observe the committed writer row"
        );
    }

    #[test]
    fn read_transaction_rejects_non_reader_indexes_before_invoking_operation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite");
        let writer = SqliteIndex::open_writer(path.to_str().unwrap()).unwrap();
        let in_memory = SqliteIndex::open_in_memory().unwrap();

        for index in [&writer, &in_memory] {
            let invoked = std::cell::Cell::new(false);
            let result = index.with_read_transaction(|_| {
                invoked.set(true);
                Ok(())
            });
            assert!(matches!(result, Err(rusqlite::Error::InvalidQuery)));
            assert!(!invoked.get(), "non-reader closure must not be invoked");
            assert!(index.conn.is_autocommit(), "no transaction may be opened");
        }
    }

    #[cfg(unix)]
    #[test]
    fn read_transaction_rolls_back_errors_without_store_mutation() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("index.sqlite");
        {
            let writer = SqliteIndex::open_writer(path.to_str().unwrap()).unwrap();
            writer
                .conn
                .execute("INSERT INTO tapes (tape_id) VALUES ('existing')", [])
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        let before_entries = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        {
            let reader = SqliteIndex::open_reader(path.to_str().unwrap()).unwrap();
            let operation_error = reader.with_read_transaction(|conn| -> rusqlite::Result<()> {
                assert_eq!(
                    conn.query_row("SELECT COUNT(*) FROM tapes", [], |row| row.get::<_, i64>(0))?,
                    1
                );
                Err(rusqlite::Error::InvalidQuery)
            });
            assert!(matches!(
                operation_error,
                Err(rusqlite::Error::InvalidQuery)
            ));
            assert!(
                reader.conn.is_autocommit(),
                "operation error must leave no transaction open"
            );

            let write_error = reader.with_read_transaction(|conn| -> rusqlite::Result<()> {
                conn.execute("INSERT INTO tapes (tape_id) VALUES ('forbidden')", [])?;
                Ok(())
            });
            assert!(
                write_error.is_err(),
                "read-only transaction must reject writes"
            );
            assert!(
                reader.conn.is_autocommit(),
                "read-only write error must leave no transaction open"
            );
            assert_eq!(
                reader
                    .with_read_transaction(|conn| {
                        conn.query_row("SELECT COUNT(*) FROM tapes", [], |row| row.get::<_, i64>(0))
                    })
                    .unwrap(),
                1,
                "reader must remain usable after rollback"
            );
        }

        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::read_dir(temp.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            before_entries
        );
        assert!(!path.with_extension("sqlite-wal").exists());
        assert!(!path.with_extension("sqlite-shm").exists());
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
