use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    MsgIn,
    MsgOut,
    ToolCall,
    ToolResult,
    CodeRead,
    CodeEdit,
    SpanLink,
    Meta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRange {
    pub start: u32,
    pub end: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TapeEvent {
    pub timestamp: String,
    pub data: TapeEventData,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TapeEventData {
    CodeRead(CodeReadEvent),
    CodeEdit(CodeEditEvent),
    SpanLink(SpanLinkEvent),
    Meta(MetaEvent),
    Other { kind: EventKind },
}

#[derive(Debug, Clone, PartialEq)]
pub struct MetaEvent {
    pub model: Option<String>,
    pub repo_head: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodeReadEvent {
    pub file: String,
    pub range: FileRange,
    pub anchor_hashes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CodeEditEvent {
    pub file: String,
    pub before_range: Option<FileRange>,
    pub after_range: Option<FileRange>,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpanLinkEvent {
    pub from_file: String,
    pub from_range: FileRange,
    pub to_file: String,
    pub to_range: FileRange,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TapeEventAt {
    pub offset: u64,
    pub event: TapeEvent,
}

pub fn parse_jsonl_events(input: &str) -> Result<Vec<TapeEventAt>, serde_json::Error> {
    let mut out = Vec::new();

    for (idx, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let raw: RawEvent = serde_json::from_str(line)?;
        out.push(TapeEventAt {
            offset: idx as u64,
            event: raw.into_tape_event(),
        });
    }

    Ok(out)
}

#[derive(Debug, Deserialize)]
struct RawEvent {
    #[serde(rename = "t")]
    timestamp: String,
    #[serde(rename = "k")]
    kind: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    repo_head: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    file: Option<String>,
    #[serde(default)]
    range: Option<[u32; 2]>,
    #[serde(default)]
    before_range: Option<[u32; 2]>,
    #[serde(default)]
    after_range: Option<[u32; 2]>,
    #[serde(default)]
    before_hash: Option<String>,
    #[serde(default)]
    after_hash: Option<String>,
    #[serde(default)]
    anchor_hashes: Option<Vec<String>>,
    #[serde(default)]
    from_file: Option<String>,
    #[serde(default)]
    from_range: Option<[u32; 2]>,
    #[serde(default)]
    to_file: Option<String>,
    #[serde(default)]
    to_range: Option<[u32; 2]>,
    #[serde(default)]
    note: Option<String>,
}

impl RawEvent {
    fn into_tape_event(self) -> TapeEvent {
        let data = match self.kind.as_str() {
            "code.read" => TapeEventData::CodeRead(CodeReadEvent {
                file: self.file.unwrap_or_default(),
                range: self
                    .range
                    .map(file_range)
                    .unwrap_or(FileRange { start: 0, end: 0 }),
                anchor_hashes: self.anchor_hashes.unwrap_or_default(),
            }),
            "code.edit" => TapeEventData::CodeEdit(CodeEditEvent {
                file: self.file.unwrap_or_default(),
                before_range: self.before_range.map(file_range),
                after_range: self.after_range.map(file_range),
                before_hash: self.before_hash,
                after_hash: self.after_hash,
            }),
            "span.link" => TapeEventData::SpanLink(SpanLinkEvent {
                from_file: self.from_file.unwrap_or_default(),
                from_range: self
                    .from_range
                    .map(file_range)
                    .unwrap_or(FileRange { start: 0, end: 0 }),
                to_file: self.to_file.unwrap_or_default(),
                to_range: self
                    .to_range
                    .map(file_range)
                    .unwrap_or(FileRange { start: 0, end: 0 }),
                note: self.note,
            }),
            "meta" => TapeEventData::Meta(MetaEvent {
                model: self.model,
                repo_head: self.repo_head,
                label: self.label,
            }),
            _ => TapeEventData::Other {
                kind: to_kind(&self.kind),
            },
        };

        TapeEvent {
            timestamp: self.timestamp,
            data,
        }
    }
}

fn file_range(raw: [u32; 2]) -> FileRange {
    FileRange {
        start: raw[0],
        end: raw[1],
    }
}

fn to_kind(kind: &str) -> EventKind {
    match kind {
        "msg.in" => EventKind::MsgIn,
        "msg.out" => EventKind::MsgOut,
        "tool.call" => EventKind::ToolCall,
        "tool.result" => EventKind::ToolResult,
        "code.read" => EventKind::CodeRead,
        "code.edit" => EventKind::CodeEdit,
        "span.link" => EventKind::SpanLink,
        "meta" => EventKind::Meta,
        _ => EventKind::Meta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_offsets_and_supported_events() {
        let jsonl = r#"{"t":"2026-02-22T00:00:00Z","k":"meta","model":"gpt","repo_head":"abc"}
{"t":"2026-02-22T00:00:01Z","k":"code.read","file":"src/lib.rs","range":[1,3],"anchor_hashes":["h1","h2"]}
{"t":"2026-02-22T00:00:02Z","k":"code.edit","file":"src/lib.rs","before_range":[1,3],"after_range":[1,4],"before_hash":"a","after_hash":"b"}
{"t":"2026-02-22T00:00:03Z","k":"span.link","from_file":"a.rs","from_range":[1,2],"to_file":"b.rs","to_range":[3,4],"note":"moved"}"#;

        let events = parse_jsonl_events(jsonl).expect("valid JSONL");
        assert_eq!(events.len(), 4);
        assert_eq!(events[2].offset, 2);

        match &events[1].event.data {
            TapeEventData::CodeRead(read) => {
                assert_eq!(read.file, "src/lib.rs");
                assert_eq!(read.anchor_hashes, vec!["h1", "h2"]);
            }
            _ => panic!("expected code.read"),
        }
    }

    #[test]
    fn parses_unknown_event_as_other() {
        let jsonl =
            r#"{"t":"2026-02-22T00:00:00Z","k":"tool.result","tool":"cargo test","exit":0}"#;
        let events = parse_jsonl_events(jsonl).expect("valid JSONL");
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].event.data,
            TapeEventData::Other {
                kind: EventKind::ToolResult
            }
        ));
    }
}
