use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::tape::adapter::AdapterId;
use crate::tape::adapters::codex::{CodexState, codex_jsonl_incremental};
use crate::tape::harness::{ClaudeState, claude_jsonl_incremental};

/// Only unfinished native calls and session identity survive a poll, not prior events.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "harness", content = "state")]
pub(super) enum NativeState {
    Codex(CodexState),
    Claude(ClaudeState),
}

impl NativeState {
    pub(super) fn new(adapter: AdapterId) -> Option<Self> {
        match adapter {
            AdapterId::CodexCli => Some(Self::Codex(CodexState::default())),
            AdapterId::ClaudeCode => Some(Self::Claude(ClaudeState::default())),
            _ => None,
        }
    }

    pub(super) fn convert(&mut self, input: &str) -> Result<String, serde_json::Error> {
        match self {
            Self::Codex(state) => codex_jsonl_incremental(input, state),
            Self::Claude(state) => claude_jsonl_incremental(input, state),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Continuity {
    pub native: NativeState,
    pub message_turns: i64,
    pub tape_id: String,
    #[serde(default)]
    pub last_message_timestamp: Option<String>,
}

pub(super) fn last_message_timestamp(normalized: &str) -> Option<String> {
    normalized
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(crate::dispatch::is_message_row)
        .filter_map(|row| row["t"].as_str().map(ToOwned::to_owned))
        .last()
}

pub(super) fn annotate(
    normalized: &str,
    previous: Option<&Continuity>,
    context_only: bool,
) -> Result<(String, i64), serde_json::Error> {
    let mut rows: Vec<Value> = normalized
        .lines()
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let turns = rows
        .iter()
        .filter(|row| crate::dispatch::is_message_row(row))
        .count() as i64;
    if context_only {
        // A one-time legacy bootstrap is a chronology projection, not another
        // evidence tape. Rebuilding the index must not duplicate prior edits.
        rows = rows
            .into_iter()
            .enumerate()
            .filter_map(|(offset, mut row)| {
                if matches!(
                    row["k"].as_str(),
                    Some("meta" | "msg.in" | "msg.out" | "tool.call")
                ) {
                    row["context_source_normalized_offset"] = json!(offset);
                    Some(row)
                } else {
                    None
                }
            })
            .collect();
    }
    if let Some(meta) = rows.iter_mut().find(|row| row["k"] == "meta") {
        if let Some(previous) = previous {
            meta["ingest_continuation"] = json!({
                "previous_tape_id": previous.tape_id,
                "message_turn_start": previous.message_turns,
                "last_message_timestamp": previous.last_message_timestamp,
            });
        }
        if context_only {
            meta["ingest_context_only"] = json!(true);
        }
    }
    let mut out = String::new();
    for row in rows {
        out.push_str(&serde_json::to_string(&row)?);
        out.push('\n');
    }
    Ok((out, turns))
}
