use std::borrow::Cow;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::sync::{Arc, Mutex, mpsc};

use serde::Deserializer;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;

const MAX_GREP_SCAN_WORKERS: usize = 4;

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
        } else {
            Ok(value.contains(self.pattern))
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

/// Scan one compressed JSONL tape without retaining the complete decompressed tape.
/// A missing decompressed limit is used only for the local path, whose existing
/// contract has no configured per-tape limit.
pub(crate) fn scan_grep_reader<R: Read>(
    compressed: R,
    decompressed_limit: Option<u64>,
    pattern: &str,
) -> Result<GrepTapeSummary, GrepScanError> {
    let decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(|error| GrepScanError::new("decompress_error", error.to_string()))?;
    let decoded: Box<dyn Read> = match decompressed_limit {
        Some(limit) => Box::new(decoder.take(limit.saturating_add(1))),
        None => Box::new(decoder),
    };
    let mut reader = BufReader::new(decoded);
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
        let bytes = reader
            .read_until(b'\n', &mut line)
            .map_err(|error| GrepScanError::new("decompress_error", error.to_string()))?;
        if bytes == 0 {
            break;
        }
        bytes_read = bytes_read.saturating_add(bytes as u64);
        if decompressed_limit.is_some_and(|limit| bytes_read > limit) {
            let limit = decompressed_limit.unwrap_or_default();
            return Err(GrepScanError::new(
                "over_limit",
                format!("decompressed tape exceeds {limit} byte limit"),
            ));
        }

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
    mut consume: impl FnMut(&T, R) -> Result<(), E>,
) -> Result<usize, E>
where
    T: Sync,
    R: Send,
{
    if tasks.is_empty() {
        return Ok(0);
    }
    // Four workers captured most of the measured parallel scan benefit while
    // keeping simultaneous per-tape decompression bounded.
    let worker_count = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
        .min(MAX_GREP_SCAN_WORKERS)
        .min(tasks.len());

    std::thread::scope(|scope| {
        let (job_tx, job_rx) = mpsc::sync_channel::<usize>(worker_count);
        let job_rx = Arc::new(Mutex::new(job_rx));
        let (result_tx, result_rx) = mpsc::sync_channel::<(usize, R)>(worker_count);
        for _ in 0..worker_count {
            let job_rx = Arc::clone(&job_rx);
            let result_tx = result_tx.clone();
            let scan = &scan;
            scope.spawn(move || {
                loop {
                    let next = job_rx.lock().expect("grep job receiver lock").recv();
                    let Ok(index) = next else { break };
                    let result = scan(&tasks[index]);
                    if result_tx.send((index, result)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(result_tx);

        let mut start = 0usize;
        while start < tasks.len() {
            let end = start.saturating_add(worker_count).min(tasks.len());
            for index in start..end {
                job_tx.send(index).expect("grep worker pool is alive");
            }
            let mut completed = Vec::with_capacity(end - start);
            for _ in start..end {
                completed.push(result_rx.recv().expect("grep worker returned a result"));
            }
            completed.sort_by_key(|(index, _)| *index);
            for (index, result) in completed {
                consume(&tasks[index], result)?;
            }
            start = end;
        }
        drop(job_tx);
        Ok(worker_count)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parallel_scan_consumes_results_in_tape_order_with_a_bounded_worker_count() {
        let tasks = (0usize..32).collect::<Vec<_>>();
        let mut seen = Vec::new();
        let workers = scan_parallel_in_order(
            &tasks,
            |task| task.saturating_mul(2),
            |task, result| {
                seen.push((*task, result));
                Ok::<(), ()>(())
            },
        )
        .expect("parallel scan");

        let expected_workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .min(MAX_GREP_SCAN_WORKERS)
            .min(tasks.len());
        assert_eq!(workers, expected_workers);
        assert_eq!(
            seen,
            tasks
                .iter()
                .map(|task| (*task, task * 2))
                .collect::<Vec<_>>()
        );
    }
}
