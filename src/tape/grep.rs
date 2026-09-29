use std::borrow::Cow;
use std::collections::{BTreeMap, HashSet};
use std::io::{BufRead, BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use serde::Deserializer;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;

const MAX_GREP_SCAN_WORKERS: usize = 4;
const MAX_GREP_SCAN_UNCONSUMED: usize = 1_024;
const MAX_GREP_SCAN_REORDER_BYTES: usize = 4 * 1024 * 1024;
const MAX_GREP_SCAN_REORDER_MATCHES: usize = 64 * 1024;
const GREP_SCAN_REORDER_ENTRY_OVERHEAD_BYTES: usize = 128;

#[derive(Debug)]
pub(crate) struct GrepTapeSummary {
    pub(crate) match_count: usize,
    pub(crate) provenance_match_count: usize,
    pub(crate) provenance_event_count: usize,
    pub(crate) timestamp: String,
    pub(crate) total_lines: usize,
    pub(crate) anchor_line: usize,
    pub(crate) files_touched: Vec<String>,
}

impl GrepTapeSummary {
    pub(crate) fn reorder_weight(&self) -> (usize, usize) {
        let files_touched_bytes = self.files_touched.iter().fold(
            self.files_touched
                .capacity()
                .saturating_mul(std::mem::size_of::<String>()),
            |bytes, path| bytes.saturating_add(path.capacity()).saturating_add(16),
        );
        let timestamp_bytes = if self.timestamp.capacity() == 0 {
            0
        } else {
            self.timestamp.capacity().saturating_add(16)
        };

        (
            std::mem::size_of::<Self>()
                .saturating_add(files_touched_bytes)
                .saturating_add(timestamp_bytes),
            self.match_count,
        )
    }
}

#[derive(Default)]
struct GrepEvent<'a> {
    timestamp: Option<&'a RawValue>,
    kind: Option<&'a RawValue>,
    content_matches: bool,
    args_match: bool,
    stdout_matches: bool,
    stderr_matches: bool,
    tool_matches: bool,
    text_matches: bool,
    before_text_matches: bool,
    after_text_matches: bool,
    note_matches: bool,
    file: Option<&'a RawValue>,
    from_file: Option<&'a RawValue>,
    to_file: Option<&'a RawValue>,
}

impl GrepEvent<'_> {
    fn matches(&self, pattern: &str) -> bool {
        self.content_matches
            || self.args_match
            || self.stdout_matches
            || self.stderr_matches
            || self.tool_matches
            || self.text_matches
            || self.before_text_matches
            || self.after_text_matches
            || self.note_matches
            || [self.file, self.from_file, self.to_file]
                .into_iter()
                .flatten()
                .filter_map(decoded_string)
                .any(|text| text.contains(pattern))
    }
}

pub fn grep_line_matches(line: &str, pattern: &str) -> Result<bool, GrepScanError> {
    if line.trim().is_empty() {
        return Ok(false);
    }
    parse_grep_event(line.as_bytes(), pattern)
        .map(|event| event.matches(pattern))
        .map_err(|error| GrepScanError::new("json_error", error.to_string()))
}

fn parse_grep_event<'de>(
    bytes: &'de [u8],
    pattern: &str,
) -> Result<GrepEvent<'de>, serde_json::Error> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let event = GrepEventSeed { pattern }.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(event)
}

struct GrepEventSeed<'p> {
    pattern: &'p str,
}

impl<'de> DeserializeSeed<'de> for GrepEventSeed<'_> {
    type Value = GrepEvent<'de>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(GrepEventVisitor {
            pattern: self.pattern,
        })
    }
}

struct GrepEventVisitor<'p> {
    pattern: &'p str,
}

impl<'de> Visitor<'de> for GrepEventVisitor<'_> {
    type Value = GrepEvent<'de>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSONL event")
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut event = GrepEvent::default();
        let pattern = self.pattern;
        while let Some(field) = map.next_key_seed(GrepFieldSeed)? {
            match field {
                GrepField::Timestamp => event.timestamp = Some(map.next_value()?),
                GrepField::Kind => event.kind = Some(map.next_value()?),
                GrepField::Content => {
                    event.content_matches = map.next_value_seed(MatchSeed {
                        pattern,
                        mode: SearchMode::Content,
                    })?;
                }
                GrepField::Args => {
                    event.args_match = map.next_value_seed(RootArgsMatchSeed { pattern })?;
                }
                GrepField::Stdout => {
                    event.stdout_matches = map.next_value_seed(StringMatchSeed { pattern })?;
                }
                GrepField::Stderr => {
                    event.stderr_matches = map.next_value_seed(StringMatchSeed { pattern })?;
                }
                GrepField::Tool => {
                    event.tool_matches = map.next_value_seed(StringMatchSeed { pattern })?;
                }
                GrepField::Text => {
                    event.text_matches = map.next_value_seed(StringMatchSeed { pattern })?;
                }
                GrepField::BeforeText => {
                    event.before_text_matches = map.next_value_seed(StringMatchSeed { pattern })?;
                }
                GrepField::AfterText => {
                    event.after_text_matches = map.next_value_seed(StringMatchSeed { pattern })?;
                }
                GrepField::Note => {
                    event.note_matches = map.next_value_seed(StringMatchSeed { pattern })?;
                }
                GrepField::File => event.file = Some(map.next_value()?),
                GrepField::FromFile => event.from_file = Some(map.next_value()?),
                GrepField::ToFile => event.to_file = Some(map.next_value()?),
                GrepField::Other => {
                    let _: IgnoredAny = map.next_value()?;
                }
            }
        }
        Ok(event)
    }

    fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
    where
        S: SeqAccess<'de>,
    {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(GrepEvent::default())
    }

    fn visit_str<E>(self, _: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(GrepEvent::default())
    }

    fn visit_borrowed_str<E>(self, _: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(GrepEvent::default())
    }

    fn visit_string<E>(self, _: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(GrepEvent::default())
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(GrepEvent::default())
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(GrepEvent::default())
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(GrepEvent::default())
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(GrepEvent::default())
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(GrepEvent::default())
    }
}

#[derive(Clone, Copy)]
enum GrepField {
    Timestamp,
    Kind,
    Content,
    Args,
    Stdout,
    Stderr,
    Tool,
    Text,
    BeforeText,
    AfterText,
    Note,
    File,
    FromFile,
    ToFile,
    Other,
}

struct GrepFieldSeed;

impl<'de> DeserializeSeed<'de> for GrepFieldSeed {
    type Value = GrepField;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_identifier(GrepFieldVisitor)
    }
}

struct GrepFieldVisitor;

impl<'de> Visitor<'de> for GrepFieldVisitor {
    type Value = GrepField;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("an event field")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(match value {
            "t" => GrepField::Timestamp,
            "k" => GrepField::Kind,
            "content" => GrepField::Content,
            "args" => GrepField::Args,
            "stdout" => GrepField::Stdout,
            "stderr" => GrepField::Stderr,
            "tool" => GrepField::Tool,
            "text" => GrepField::Text,
            "before_text" => GrepField::BeforeText,
            "after_text" => GrepField::AfterText,
            "note" => GrepField::Note,
            "file" => GrepField::File,
            "from_file" => GrepField::FromFile,
            "to_file" => GrepField::ToFile,
            _ => GrepField::Other,
        })
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&value)
    }
}

fn decoded_string<'a>(raw: &'a RawValue) -> Option<Cow<'a, str>> {
    if let Ok(value) = serde_json::from_str::<&'a str>(raw.get()) {
        return Some(Cow::Borrowed(value));
    }
    serde_json::from_str::<String>(raw.get())
        .ok()
        .map(Cow::Owned)
}

struct StringMatchSeed<'p> {
    pattern: &'p str,
}

impl<'de> DeserializeSeed<'de> for StringMatchSeed<'_> {
    type Value = bool;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StringMatchVisitor {
            pattern: self.pattern,
        })
    }
}

struct StringMatchVisitor<'p> {
    pattern: &'p str,
}

impl<'de> Visitor<'de> for StringMatchVisitor<'_> {
    type Value = bool;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a string event field")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(value.contains(self.pattern))
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&value)
    }

    fn visit_bool<E>(self, _: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(false)
    }

    fn visit_i64<E>(self, _: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(false)
    }

    fn visit_u64<E>(self, _: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(false)
    }

    fn visit_f64<E>(self, _: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(false)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(false)
    }

    fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
    where
        S: SeqAccess<'de>,
    {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(false)
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        while map.next_key::<IgnoredAny>()?.is_some() {
            let _: IgnoredAny = map.next_value()?;
        }
        Ok(false)
    }
}

struct RootArgsMatchSeed<'p> {
    pattern: &'p str,
}

impl<'de> DeserializeSeed<'de> for RootArgsMatchSeed<'_> {
    type Value = bool;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(RootArgsMatchVisitor {
            pattern: self.pattern,
        })
    }
}

struct RootArgsMatchVisitor<'p> {
    pattern: &'p str,
}

impl<'de> Visitor<'de> for RootArgsMatchVisitor<'_> {
    type Value = bool;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("tool arguments")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if value
            .trim_start()
            .as_bytes()
            .first()
            .is_some_and(|byte| matches!(byte, b'{' | b'['))
        {
            Ok(
                json_text_matches(value, self.pattern, SearchMode::Arguments)
                    .unwrap_or_else(|| value.contains(self.pattern)),
            )
        } else if value.contains(self.pattern) {
            Ok(true)
        } else if value.trim_start().starts_with("const ") {
            let Some(arguments) =
                crate::tape::adapters::codex::assigned_exec_command_arguments(value)
            else {
                return Ok(false);
            };
            Ok(json_text_matches(&arguments, self.pattern, SearchMode::Arguments).unwrap_or(false))
        } else {
            Ok(false)
        }
    }

    fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&value)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(value.to_string().contains(self.pattern))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(value.to_string().contains(self.pattern))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(value.to_string().contains(self.pattern))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(value.to_string().contains(self.pattern))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(false)
    }

    fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
    where
        S: SeqAccess<'de>,
    {
        let mut matched = false;
        while let Some(item) = seq.next_element_seed(MatchSeed {
            pattern: self.pattern,
            mode: SearchMode::Arguments,
        })? {
            matched |= item;
        }
        Ok(matched)
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        let mut matched = false;
        while map.next_key::<IgnoredAny>()?.is_some() {
            matched |= map.next_value_seed(MatchSeed {
                pattern: self.pattern,
                mode: SearchMode::Arguments,
            })?;
        }
        Ok(matched)
    }
}

fn json_text_matches(text: &str, pattern: &str, mode: SearchMode) -> Option<bool> {
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let result = MatchSeed { pattern, mode }
        .deserialize(&mut deserializer)
        .and_then(|matched| deserializer.end().map(|()| matched));
    result.ok()
}

#[derive(Clone, Copy)]
enum SearchMode {
    Arguments,
    Content,
    ContentTextOnly,
}

struct MatchSeed<'p> {
    pattern: &'p str,
    mode: SearchMode,
}

impl<'de> DeserializeSeed<'de> for MatchSeed<'_> {
    type Value = bool;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(MatchVisitor {
            pattern: self.pattern,
            mode: self.mode,
        })
    }
}

struct MatchVisitor<'p> {
    pattern: &'p str,
    mode: SearchMode,
}

impl<'de> Visitor<'de> for MatchVisitor<'_> {
    type Value = bool;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("JSON transcript content")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(value.contains(self.pattern))
    }

    fn visit_borrowed_str<E>(self, value: &'_ str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(value)
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(&value)
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(matches!(self.mode, SearchMode::Arguments) && value.to_string().contains(self.pattern))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(matches!(self.mode, SearchMode::Arguments) && value.to_string().contains(self.pattern))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(matches!(self.mode, SearchMode::Arguments) && value.to_string().contains(self.pattern))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(matches!(self.mode, SearchMode::Arguments) && value.to_string().contains(self.pattern))
    }

    fn visit_seq<S>(self, mut seq: S) -> Result<Self::Value, S::Error>
    where
        S: SeqAccess<'de>,
    {
        let mode = self.mode;
        let pattern = self.pattern;
        let mut matched = false;
        while let Some(item) = seq.next_element_seed(MatchSeed { pattern, mode })? {
            matched |= item;
        }
        Ok(matched)
    }

    fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
    where
        M: MapAccess<'de>,
    {
        match self.mode {
            SearchMode::ContentTextOnly => {
                let pattern = self.pattern;
                let mut matched = false;
                while let Some(key) = map.next_key::<String>()? {
                    if matches!(key.as_str(), "text" | "input_text" | "output_text") {
                        matched |= map.next_value_seed(MatchSeed {
                            pattern,
                            mode: SearchMode::ContentTextOnly,
                        })?;
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(matched)
            }
            SearchMode::Arguments => {
                let pattern = self.pattern;
                let mut matched = false;
                while map.next_key::<IgnoredAny>()?.is_some() {
                    matched |= map.next_value_seed(MatchSeed {
                        pattern,
                        mode: SearchMode::Arguments,
                    })?;
                }
                Ok(matched)
            }
            SearchMode::Content => {
                let pattern = self.pattern;
                let mut matched = false;
                while let Some(key) = map.next_key::<String>()? {
                    if matches!(key.as_str(), "text" | "input_text" | "output_text") {
                        matched |= map.next_value_seed(MatchSeed {
                            pattern,
                            mode: SearchMode::ContentTextOnly,
                        })?;
                    } else {
                        let _: IgnoredAny = map.next_value()?;
                    }
                }
                Ok(matched)
            }
        }
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(false)
    }
}

#[derive(Debug)]
pub(crate) struct GrepScanError {
    pub(crate) code: &'static str,
    pub(crate) message: String,
}

impl GrepScanError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

fn read_grep_record<R: BufRead>(
    reader: &mut R,
    record: &mut Vec<u8>,
    limit: Option<u64>,
    line_number: usize,
    start_offset: u64,
) -> Result<usize, GrepScanError> {
    loop {
        let available = reader
            .fill_buf()
            .map_err(|error| GrepScanError::new("decompress_error", error.to_string()))?;
        if available.is_empty() {
            return Ok(record.len());
        }
        let length = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        let complete = available.get(length - 1) == Some(&b'\n');
        let next_length = record.len().checked_add(length).ok_or_else(|| {
            GrepScanError::new("over_limit", "decompressed JSONL record length overflow")
        })?;
        if limit.is_some_and(|limit| next_length as u64 > limit) {
            return Err(GrepScanError::new(
                "over_limit",
                format!(
                    "decompressed JSONL record line {line_number} at byte offset {start_offset} exceeds {} byte limit (including newline); observed at least {next_length} bytes",
                    limit.unwrap_or_default(),
                ),
            ));
        }
        record.extend_from_slice(&available[..length]);
        reader.consume(length);
        if complete {
            return Ok(record.len());
        }
    }
}

/// Scan one compressed JSONL tape without retaining the complete decompressed tape.
/// A missing record limit is used only for the local path, whose existing
/// contract has no configured per-record limit.
pub(crate) fn scan_grep_reader<R: Read>(
    compressed: R,
    record_limit: Option<u64>,
    pattern: &str,
) -> Result<GrepTapeSummary, GrepScanError> {
    let decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(|error| GrepScanError::new("decompress_error", error.to_string()))?;
    let mut reader = BufReader::new(decoder);
    let mut line = Vec::new();
    let mut bytes_read = 0u64;
    let mut total_lines = 0usize;
    let mut match_count = 0usize;
    let mut provenance_match_count = 0usize;
    let mut provenance_event_count = 0usize;
    let mut first_match = None;
    let mut first_provenance_match = None;
    let mut timestamp = String::new();
    let mut files_touched = HashSet::new();

    loop {
        line.clear();
        let bytes = read_grep_record(
            &mut reader,
            &mut line,
            record_limit,
            total_lines.saturating_add(1),
            bytes_read,
        )?;
        if bytes == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(bytes as u64);

        let mut content_end = line.len();
        if line.get(content_end.saturating_sub(1)) == Some(&b'\n') {
            content_end -= 1;
            if line.get(content_end.saturating_sub(1)) == Some(&b'\r') {
                content_end -= 1;
            }
        }
        let text = std::str::from_utf8(&line[..content_end])
            .map_err(|error| GrepScanError::new("decompress_error", error.to_string()))?;
        let line_offset = total_lines as u64;
        total_lines = total_lines.saturating_add(1);
        if text.trim().is_empty() {
            continue;
        }

        let event = parse_grep_event(&line[..content_end], pattern)
            .map_err(|error| GrepScanError::new("json_error", error.to_string()))?;
        if let Some(row_timestamp) = event.timestamp.and_then(decoded_string)
            && row_timestamp.as_ref() > timestamp.as_str()
        {
            timestamp = row_timestamp.into_owned();
        }
        for file in [event.file, event.from_file, event.to_file]
            .into_iter()
            .flatten()
            .filter_map(decoded_string)
        {
            files_touched.insert(file.into_owned());
        }

        let provenance = event.kind.is_some_and(|kind| {
            decoded_string(kind).is_some_and(|kind| {
                matches!(kind.as_ref(), "code.edit" | "code.read" | "span.link")
            })
        });
        if provenance {
            provenance_event_count = provenance_event_count.saturating_add(1);
        }
        if event.matches(pattern) {
            match_count = match_count.saturating_add(1);
            first_match.get_or_insert(line_offset);
            if provenance {
                provenance_match_count = provenance_match_count.saturating_add(1);
                first_provenance_match.get_or_insert(line_offset);
            }
        }
    }

    let mut files_touched = if match_count == 0 {
        Vec::new()
    } else {
        files_touched.into_iter().collect::<Vec<_>>()
    };
    files_touched.sort();
    let anchor_offset = first_provenance_match.or(first_match).unwrap_or_default();
    Ok(GrepTapeSummary {
        match_count,
        provenance_match_count,
        provenance_event_count,
        timestamp,
        total_lines,
        anchor_line: usize::try_from(anchor_offset)
            .unwrap_or_default()
            .saturating_add(1),
        files_touched,
    })
}

/// Run a bounded worker pool over sorted tasks, delivering results in input order.
pub(crate) fn scan_parallel_in_order<T, R, E>(
    tasks: &[T],
    scan: impl Fn(&T) -> R + Sync,
    result_weight: impl Fn(&R) -> (usize, usize),
    mut consume: impl FnMut(&T, R) -> Result<(), E>,
) -> Result<usize, E>
where
    T: Sync,
    R: Send,
{
    if tasks.is_empty() {
        return Ok(0);
    }
    let worker_count = grep_scan_worker_count(tasks.len());
    // Keep a larger task window so short tapes can run ahead of a slow early
    // tape. Bound completed summaries by estimated bytes, match volume, and a
    // hard task count. At most the existing worker count can finish before the
    // next admission check; active scans remain worker-capped.
    let max_unconsumed = MAX_GREP_SCAN_UNCONSUMED.min(tasks.len());

    std::thread::scope(|scope| {
        let (job_tx, job_rx) = mpsc::sync_channel::<usize>(worker_count);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (result_tx, result_rx) = mpsc::sync_channel::<(usize, R)>(worker_count);
        let cancelled = Arc::new(AtomicBool::new(false));
        for _ in 0..worker_count {
            let job_rx = Arc::clone(&job_rx);
            let result_tx = result_tx.clone();
            let cancelled = Arc::clone(&cancelled);
            let scan = &scan;
            scope.spawn(move || {
                loop {
                    let next = job_rx.lock().expect("grep job receiver lock").recv();
                    let Ok(index) = next else { break };
                    if cancelled.load(Ordering::Relaxed) {
                        break;
                    }
                    let result = scan(&tasks[index]);
                    if cancelled.load(Ordering::Relaxed) || result_tx.send((index, result)).is_err()
                    {
                        break;
                    }
                }
            });
        }
        drop(result_tx);

        let mut next_to_dispatch = 0usize;
        let mut next_to_consume = 0usize;
        let mut in_flight = 0usize;
        let mut completed = BTreeMap::new();
        let mut buffered_result_bytes = 0usize;
        let mut buffered_match_count = 0usize;
        while next_to_consume < tasks.len() {
            while next_to_dispatch < tasks.len()
                && in_flight < worker_count
                && next_to_dispatch.saturating_sub(next_to_consume) < max_unconsumed
                && buffered_result_bytes < MAX_GREP_SCAN_REORDER_BYTES
                && buffered_match_count < MAX_GREP_SCAN_REORDER_MATCHES
            {
                job_tx
                    .send(next_to_dispatch)
                    .expect("grep worker pool is alive");
                next_to_dispatch += 1;
                in_flight += 1;
            }

            let (index, result) = result_rx.recv().expect("grep worker returned a result");
            in_flight = in_flight
                .checked_sub(1)
                .expect("grep worker returned a result that was not in flight");
            let (result_bytes, result_matches) = result_weight(&result);
            let result_bytes = result_bytes.saturating_add(GREP_SCAN_REORDER_ENTRY_OVERHEAD_BYTES);
            buffered_result_bytes = buffered_result_bytes.saturating_add(result_bytes);
            buffered_match_count = buffered_match_count.saturating_add(result_matches);
            assert!(
                completed
                    .insert(index, (result, result_bytes, result_matches))
                    .is_none(),
                "grep worker returned a duplicate result"
            );

            while let Some((result, result_bytes, result_matches)) =
                completed.remove(&next_to_consume)
            {
                buffered_result_bytes = buffered_result_bytes.saturating_sub(result_bytes);
                buffered_match_count = buffered_match_count.saturating_sub(result_matches);
                if let Err(error) = consume(&tasks[next_to_consume], result) {
                    cancelled.store(true, Ordering::Relaxed);
                    drop(job_tx);
                    return Err(error);
                }
                next_to_consume += 1;
            }
        }
        drop(job_tx);
        Ok(worker_count)
    })
}

fn grep_scan_worker_count(task_count: usize) -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
        .min(MAX_GREP_SCAN_WORKERS)
        .min(task_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Condvar;

    #[derive(Default)]
    struct ScanPoolGate {
        first_released: bool,
        replacement_started: bool,
        replacement_started_before_release: bool,
        task_one_finished: bool,
        release_later_tasks: bool,
        started: Vec<usize>,
    }

    fn matches(line: &str, pattern: &str) -> bool {
        grep_line_matches(line, pattern).expect("valid event")
    }

    #[test]
    fn matches_decoded_text_without_matching_envelope_keys_or_storage_escapes() {
        let line =
            r#"{"t":"2026-09-27T00:00:00Z","k":"msg.in","content":"café \u2603 literal\\n"}"#;

        assert!(matches(line, "café ☃"));
        assert!(matches(line, r"literal\n"));
        assert!(!matches(line, "CAFÉ ☃"));
        assert!(!matches(line, "\n"));
        assert!(!matches(line, "content"));
        assert!(!matches(line, "msg.in"));
    }

    #[test]
    fn searches_native_and_normalized_tool_arguments_without_joining_keys_or_fields() {
        let normalized = r#"{"k":"tool.call","tool":"exec_command","args":"{\"payload_key\":\"printf \\\"hello\\\"\",\"other\":\"world\"}"}"#;
        assert!(matches(normalized, "hello"));
        assert!(matches(normalized, "world"));
        assert!(!matches(normalized, "payload_key"));
        assert!(!matches(normalized, "helloworld"));

        let native = r#"{"k":"tool.call","args":{"parameter_name":["echo","nested"],"count":12}}"#;
        assert!(matches(native, "nested"));
        assert!(matches(native, "12"));
        assert!(!matches(native, "parameter_name"));
        assert!(!matches(native, "echonested"));
    }

    #[test]
    fn does_not_recursively_decode_json_text_inside_an_argument_value() {
        let line = r#"{"k":"tool.call","args":"{\"cmd\":\"literal \\\\u2603\"}"}"#;

        assert!(matches(line, r"\u2603"));
        assert!(!matches(line, "☃"));
    }

    #[test]
    fn malformed_json_shaped_argument_text_remains_searchable_as_text() {
        let line = r#"{"k":"tool.call","args":"{needle"}"#;

        assert!(matches(line, "needle"));
        assert!(!matches(line, "NEEDLE"));
    }

    #[test]
    fn keeps_stdout_stderr_and_content_block_boundaries() {
        let result = r#"{"k":"tool.result","stdout":"left","stderr":"right"}"#;
        assert!(matches(result, "left"));
        assert!(matches(result, "right"));
        assert!(!matches(result, "leftright"));

        let message = r#"{"k":"msg.out","content":[{"type":"output_text","text":"first"},{"type":"output_text","text":"second"}]}"#;
        assert!(matches(message, "first"));
        assert!(matches(message, "second"));
        assert!(!matches(message, "firstsecond"));
    }

    #[test]
    fn scan_preserves_offsets_rank_and_malformed_tape_failure() {
        let content = concat!(
            "\n",
            "{\"t\":\"2026-09-27T00:00:00Z\",\"k\":\"msg.in\",\"content\":\"needle\"}\n",
            "{\"t\":\"2026-09-27T00:00:01Z\",\"k\":\"code.edit\",\"file\":\"src/lib.rs\",\"after_text\":\"needle code\"}\n",
        );
        let compressed = zstd::stream::encode_all(content.as_bytes(), 0).expect("compress tape");
        let summary = scan_grep_reader(&compressed[..], Some(1024), "needle").expect("scan");
        assert_eq!(summary.match_count, 2);
        assert_eq!(summary.provenance_match_count, 1);
        assert_eq!(summary.provenance_event_count, 1);
        assert_eq!(summary.anchor_line, 3);
        assert_eq!(summary.total_lines, 3);
        assert_eq!(summary.files_touched, vec!["src/lib.rs"]);

        let malformed =
            zstd::stream::encode_all(&b"{bad json}\n"[..], 0).expect("compress bad tape");
        let error = scan_grep_reader(&malformed[..], Some(1024), "needle").unwrap_err();
        assert_eq!(error.code, "json_error");
    }

    #[test]
    fn scan_limits_each_record_without_limiting_the_whole_tape() {
        let record = b"{\"k\":\"msg.in\",\"content\":\"needle\"}\n";
        let content = record.repeat(64);
        let compressed = zstd::stream::encode_all(&content[..], 0).expect("compress tape");
        let summary = scan_grep_reader(&compressed[..], Some(record.len() as u64), "needle")
            .expect("complete tape exceeds per-record limit but each record fits");
        assert_eq!(summary.total_lines, 64);
        assert_eq!(summary.match_count, 64);

        let error = scan_grep_reader(&compressed[..], Some(record.len() as u64 - 1), "needle")
            .expect_err("one over-limit record fails the whole tape");
        assert_eq!(error.code, "over_limit");
        assert!(error.message.contains("including newline"));
        assert!(error.message.contains("line 1 at byte offset 0"));
        assert!(
            error
                .message
                .contains(&format!("observed at least {} bytes", record.len()))
        );

        let unterminated = [record.as_slice(), &vec![b'x'; record.len() + 1]].concat();
        let compressed = zstd::stream::encode_all(&unterminated[..], 0).expect("compress tape");
        let error = scan_grep_reader(&compressed[..], Some(record.len() as u64), "needle")
            .expect_err("unterminated record must be bounded too");
        assert_eq!(error.code, "over_limit");
        assert!(
            error
                .message
                .contains(&format!("line 2 at byte offset {}", record.len()))
        );
        assert!(error.message.contains("observed at least"));
    }

    #[test]
    fn parallel_scan_consumes_results_in_tape_order_with_a_bounded_worker_count() {
        let tasks = (0usize..32).collect::<Vec<_>>();
        let mut seen = Vec::new();
        let workers = scan_parallel_in_order(
            &tasks,
            |task| task.saturating_mul(2),
            |_| (std::mem::size_of::<usize>(), 0),
            |task, result| {
                seen.push((*task, result));
                Ok::<(), ()>(())
            },
        )
        .expect("parallel scan");

        let expected_workers = grep_scan_worker_count(tasks.len());
        assert_eq!(workers, expected_workers);
        assert_eq!(
            seen,
            tasks
                .iter()
                .map(|task| (*task, task * 2))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_scan_refills_around_a_skewed_first_tape_and_consumes_in_order() {
        let tasks = (0usize..32).collect::<Vec<_>>();
        let worker_count = grep_scan_worker_count(tasks.len());
        if worker_count < 2 {
            return;
        }

        let gate = Arc::new((Mutex::new(ScanPoolGate::default()), Condvar::new()));
        let scan_gate = Arc::clone(&gate);
        let consumed = Arc::new(Mutex::new(Vec::new()));
        let scan_consumed = Arc::clone(&consumed);
        let replacement_index = worker_count;
        let task_count = tasks.len();

        let runner = std::thread::spawn(move || {
            scan_parallel_in_order(
                &tasks,
                |task| {
                    let (state_lock, changed) = &*scan_gate;
                    let mut state = state_lock.lock().expect("test gate lock");
                    state.started.push(*task);
                    if *task == 0 {
                        while !state.first_released {
                            state = changed.wait(state).expect("test gate wait");
                        }
                    }
                    if *task == replacement_index {
                        state.replacement_started = true;
                        state.replacement_started_before_release = !state.first_released;
                        changed.notify_all();
                    }
                    *task
                },
                |_| (std::mem::size_of::<usize>(), 0),
                |task, result| {
                    assert_eq!(*task, result);
                    scan_consumed.lock().expect("consumed lock").push(*task);
                    Ok::<(), ()>(())
                },
            )
        });

        let (state_lock, changed) = &*gate;
        let state = state_lock.lock().expect("test gate lock");
        let (mut state, _) = changed
            .wait_timeout_while(state, std::time::Duration::from_secs(2), |state| {
                !state.replacement_started
            })
            .expect("test gate wait");
        let refilled_before_release =
            state.replacement_started && state.replacement_started_before_release;
        state.first_released = true;
        changed.notify_all();
        drop(state);

        let workers = runner.join().expect("scan runner").expect("parallel scan");
        assert!(
            refilled_before_release,
            "a later tape should start before the slow first tape is released"
        );
        assert_eq!(workers, worker_count);
        assert_eq!(
            *consumed.lock().expect("consumed lock"),
            (0..task_count).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_scan_preserves_first_ordered_failure_and_stops_feeding_work() {
        let tasks = (0usize..64).collect::<Vec<_>>();
        let worker_count = grep_scan_worker_count(tasks.len());
        let failure_index = if worker_count > 1 { 1 } else { 0 };
        let gate = Arc::new((Mutex::new(ScanPoolGate::default()), Condvar::new()));
        let scan_gate = Arc::clone(&gate);
        let started = Arc::new(Mutex::new(Vec::new()));
        let scan_started = Arc::clone(&started);
        let consumed = Arc::new(Mutex::new(Vec::new()));
        let scan_consumed = Arc::clone(&consumed);

        let result = scan_parallel_in_order(
            &tasks,
            |task| {
                let (state_lock, changed) = &*scan_gate;
                let mut state = state_lock.lock().expect("test gate lock");
                state.started.push(*task);
                scan_started.lock().expect("started lock").push(*task);
                changed.notify_all();

                match *task {
                    0 if worker_count > 1 => {
                        let (state, _) = changed
                            .wait_timeout_while(state, std::time::Duration::from_secs(2), |state| {
                                state.started.len() < worker_count || !state.task_one_finished
                            })
                            .expect("initial workers wait");
                        assert!(state.started.len() >= worker_count);
                        assert!(state.task_one_finished);
                    }
                    1 if worker_count > 1 => {
                        state.task_one_finished = true;
                        changed.notify_all();
                    }
                    index if index > failure_index => {
                        let (state, _) = changed
                            .wait_timeout_while(state, std::time::Duration::from_secs(2), |state| {
                                !state.release_later_tasks
                            })
                            .expect("later worker release wait");
                        assert!(state.release_later_tasks);
                    }
                    _ => {}
                };
                *task
            },
            |_| (std::mem::size_of::<usize>(), 0),
            |task, result| {
                assert_eq!(*task, result);
                scan_consumed.lock().expect("consumed lock").push(*task);
                if *task == failure_index {
                    let (state_lock, changed) = &*gate;
                    state_lock
                        .lock()
                        .expect("test gate lock")
                        .release_later_tasks = true;
                    changed.notify_all();
                    Err("first ordered failure")
                } else {
                    Ok(())
                }
            },
        );

        assert_eq!(result, Err("first ordered failure"));
        assert_eq!(
            *consumed.lock().expect("consumed lock"),
            (0..=failure_index).collect::<Vec<_>>()
        );
        let started = started.lock().expect("started lock");
        assert!(started.len() <= worker_count + 1);
        assert!(started.iter().all(|index| *index <= worker_count));
    }

    #[test]
    fn parallel_scan_refills_a_large_small_result_window_and_preserves_order() {
        let tasks = (0usize..MAX_GREP_SCAN_UNCONSUMED + 32).collect::<Vec<_>>();
        let worker_count = grep_scan_worker_count(tasks.len());
        if worker_count < 2 {
            return;
        }

        let gate = Arc::new((Mutex::new((false, Vec::<usize>::new())), Condvar::new()));
        let scan_gate = Arc::clone(&gate);
        let consumed = Arc::new(Mutex::new(Vec::new()));
        let scan_consumed = Arc::clone(&consumed);
        let task_count = tasks.len();

        let runner = std::thread::spawn(move || {
            scan_parallel_in_order(
                &tasks,
                |task| {
                    let (state_lock, changed) = &*scan_gate;
                    let mut state = state_lock.lock().expect("test gate lock");
                    state.1.push(*task);
                    changed.notify_all();
                    if *task == 0 {
                        while !state.0 {
                            state = changed.wait(state).expect("test gate wait");
                        }
                    }
                    *task
                },
                |_| (std::mem::size_of::<usize>(), 0),
                |task, result| {
                    assert_eq!(*task, result);
                    scan_consumed.lock().expect("consumed lock").push(*task);
                    Ok::<(), ()>(())
                },
            )
        });

        let (state_lock, changed) = &*gate;
        let state = state_lock.lock().expect("test gate lock");
        let (mut state, _) = changed
            .wait_timeout_while(state, std::time::Duration::from_secs(2), |state| {
                state.1.len() < MAX_GREP_SCAN_UNCONSUMED
            })
            .expect("test gate wait");
        let refilled_past_old_window = state.1.len() >= 20;
        let started_before_release = state.1.len();
        state.0 = true;
        changed.notify_all();
        drop(state);

        runner.join().expect("scan runner").expect("parallel scan");
        assert!(
            refilled_past_old_window,
            "small completed summaries should refill well beyond the old eight-task window"
        );
        assert_eq!(started_before_release, MAX_GREP_SCAN_UNCONSUMED);
        assert_eq!(
            *consumed.lock().expect("consumed lock"),
            (0..task_count).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_scan_bounds_refill_by_retained_bytes_and_match_count() {
        fn assert_weight_limited_refill(weight: (usize, usize)) {
            let tasks = (0usize..32).collect::<Vec<_>>();
            let worker_count = grep_scan_worker_count(tasks.len());
            if worker_count < 2 {
                return;
            }

            let gate = Arc::new((Mutex::new((false, Vec::<usize>::new())), Condvar::new()));
            let scan_gate = Arc::clone(&gate);
            let consumed = Arc::new(Mutex::new(Vec::new()));
            let scan_consumed = Arc::clone(&consumed);
            let task_count = tasks.len();

            let runner = std::thread::spawn(move || {
                scan_parallel_in_order(
                    &tasks,
                    |task| {
                        let (state_lock, changed) = &*scan_gate;
                        let mut state = state_lock.lock().expect("test gate lock");
                        state.1.push(*task);
                        changed.notify_all();
                        if *task == 0 {
                            while !state.0 {
                                state = changed.wait(state).expect("test gate wait");
                            }
                        }
                        *task
                    },
                    |_| weight,
                    |task, result| {
                        assert_eq!(*task, result);
                        scan_consumed.lock().expect("consumed lock").push(*task);
                        Ok::<(), ()>(())
                    },
                )
            });

            let (state_lock, changed) = &*gate;
            let state = state_lock.lock().expect("test gate lock");
            let (mut state, _) = changed
                .wait_timeout_while(state, std::time::Duration::from_secs(2), |state| {
                    state.1.len() < worker_count + 1
                })
                .expect("test gate wait");
            let refill_started = state.1.len() >= worker_count + 1;
            let started_before_release = state.1.len();
            state.0 = true;
            changed.notify_all();
            drop(state);

            runner.join().expect("scan runner").expect("parallel scan");
            assert!(refill_started, "one refill should fit below the weight cap");
            assert!(
                started_before_release <= worker_count + 1,
                "the next refill should stop after retained weight reaches its cap"
            );
            assert_eq!(
                *consumed.lock().expect("consumed lock"),
                (0..task_count).collect::<Vec<_>>()
            );
        }

        assert_weight_limited_refill((MAX_GREP_SCAN_REORDER_BYTES / 2, 0));
        assert_weight_limited_refill((0, MAX_GREP_SCAN_REORDER_MATCHES / 2));
    }
}
