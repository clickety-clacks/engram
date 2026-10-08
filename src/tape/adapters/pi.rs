use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::openclaw::{OpenClawState, openclaw_jsonl_incremental};
use super::structured::resolve_transcript_path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct PiState {
    session: OpenClawState,
    cwd: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    bash_execution_count: u64,
}

pub fn pi_jsonl_to_tape_jsonl(input: &str) -> Result<String, serde_json::Error> {
    pi_jsonl_incremental(input, &mut PiState::default())
}

pub(crate) fn pi_jsonl_incremental(
    input: &str,
    state: &mut PiState,
) -> Result<String, serde_json::Error> {
    let mut openclaw_input = String::new();
    for line in input.lines().filter(|line| !line.trim().is_empty()) {
        let mut row: Value = serde_json::from_str(line)?;
        match row.get("type").and_then(Value::as_str) {
            Some("session") => {
                if let Some(cwd) = row.get("cwd").and_then(Value::as_str) {
                    state.cwd = Some(cwd.to_owned());
                }
            }
            Some("model_change") => {
                remember_model(
                    &row,
                    "provider",
                    "modelId",
                    &mut state.provider,
                    &mut state.model,
                );
            }
            Some("message") => {
                let role = row
                    .get("message")
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str);
                if role == Some("assistant") {
                    if let Some(message) = row.get("message") {
                        remember_model(
                            message,
                            "provider",
                            "model",
                            &mut state.provider,
                            &mut state.model,
                        );
                        if let Some(response_model) =
                            message.get("responseModel").and_then(Value::as_str)
                        {
                            state.model = Some(response_model.to_owned());
                        }
                    }
                } else if role == Some("user") {
                    normalize_user_content(&mut row);
                } else if role == Some("bashExecution") {
                    append_bash_execution(&row, state, &mut openclaw_input)?;
                    continue;
                }
            }
            _ => {}
        }
        append_row(&mut openclaw_input, &row)?;
    }

    let normalized = openclaw_jsonl_incremental(&openclaw_input, &mut state.session)?;
    annotate_pi_events(&normalized, state)
}

fn remember_model(
    row: &Value,
    provider_key: &str,
    model_key: &str,
    provider: &mut Option<String>,
    model: &mut Option<String>,
) {
    if let Some(value) = row.get(provider_key).and_then(Value::as_str) {
        *provider = Some(value.to_owned());
    }
    if let Some(value) = row.get(model_key).and_then(Value::as_str) {
        *model = Some(value.to_owned());
    }
}

fn normalize_user_content(row: &mut Value) {
    let Some(message) = row.get_mut("message").and_then(Value::as_object_mut) else {
        return;
    };
    let Some(text) = message
        .get("content")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
    else {
        return;
    };
    message.insert("content".to_owned(), json!([{"type":"text", "text":text}]));
}

fn append_bash_execution(
    row: &Value,
    state: &mut PiState,
    output: &mut String,
) -> Result<(), serde_json::Error> {
    let Some(message) = row.get("message") else {
        return Ok(());
    };
    let Some(command) = message.get("command").and_then(Value::as_str) else {
        return Ok(());
    };

    let call_id = format!("pi-bash-{}", state.bash_execution_count);
    state.bash_execution_count = state.bash_execution_count.saturating_add(1);
    let timestamp = row
        .get("timestamp")
        .cloned()
        .or_else(|| message.get("timestamp").cloned());
    let mut arguments = serde_json::Map::new();
    arguments.insert("command".to_owned(), json!(command));
    if let Some(cwd) = &state.cwd {
        arguments.insert("workdir".to_owned(), json!(cwd));
    }

    let call = json!({
        "type": "message",
        "timestamp": timestamp,
        "message": {
            "role": "assistant",
            "content": [{
                "type": "toolCall",
                "id": call_id,
                "name": "bash",
                "arguments": arguments
            }]
        }
    });
    append_row(output, &call)?;

    let text = message
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let is_error = message
        .get("exitCode")
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
        || message
            .get("cancelled")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    let result = json!({
        "type": "message",
        "timestamp": timestamp,
        "message": {
            "role": "toolResult",
            "toolCallId": call_id,
            "toolName": "bash",
            "content": [{"type":"text", "text":text}],
            "isError": is_error
        }
    });
    append_row(output, &result)
}

fn append_row(output: &mut String, row: &Value) -> Result<(), serde_json::Error> {
    output.push_str(&serde_json::to_string(row)?);
    output.push('\n');
    Ok(())
}

fn annotate_pi_events(normalized: &str, state: &PiState) -> Result<String, serde_json::Error> {
    let mut output = String::new();
    for line in normalized.lines() {
        let mut event: Value = serde_json::from_str(line)?;
        if let Some(source) = event.get_mut("source").and_then(Value::as_object_mut) {
            source.insert("harness".to_owned(), json!("pi"));
        }
        let resolved_file = if matches!(
            event.get("k").and_then(Value::as_str),
            Some("code.read" | "code.edit")
        ) {
            state.cwd.as_deref().and_then(|cwd| {
                event
                    .get("file")
                    .and_then(Value::as_str)
                    .map(|file| resolve_transcript_path(file, Some(cwd)))
            })
        } else {
            None
        };
        if let Some(file) = resolved_file {
            event["file"] = json!(file);
        }
        if event.get("k").and_then(Value::as_str) == Some("meta") {
            event["coverage.tool"] = json!("full");
            if let Some(cwd) = &state.cwd {
                event["cwd"] = json!(cwd);
            }
            if let Some(provider) = &state.provider {
                event["provider"] = json!(provider);
            }
            if let Some(model) = &state.model {
                event["model"] = json!(model);
            }
        }
        append_row(&mut output, &event)?;
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use crate::tape::adapter::{AdapterId, adapter_claims_input};
    use crate::tape::event::{TapeEventData, parse_jsonl_events};

    use super::{PiState, pi_jsonl_incremental, pi_jsonl_to_tape_jsonl};

    #[test]
    fn pi_v3_session_maps_messages_tools_and_file_evidence() {
        let normalized =
            pi_jsonl_to_tape_jsonl(include_str!("../../../tests/fixtures/pi/session-v3.jsonl"))
                .expect("Pi session should parse");
        let rows = normalized
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("event JSON"))
            .collect::<Vec<_>>();
        let meta = rows.iter().find(|row| row["k"] == "meta").expect("meta");
        assert_eq!(meta["source"]["harness"], "pi");
        assert_eq!(meta["source"]["session_id"], "pi-session-1");
        assert_eq!(meta["cwd"], "/workspace/demo");
        assert_eq!(meta["provider"], "openai");
        assert_eq!(meta["model"], "gpt-5");
        assert_eq!(meta["coverage.tool"], "full");

        let kinds = rows
            .iter()
            .filter_map(|row| row["k"].as_str())
            .collect::<Vec<_>>();
        assert!(kinds.contains(&"msg.in"));
        assert!(kinds.contains(&"msg.out"));
        assert!(kinds.contains(&"tool.call"));
        assert!(kinds.contains(&"tool.result"));
        assert!(kinds.contains(&"code.read"));
        assert!(kinds.contains(&"code.edit"));

        let events = parse_jsonl_events(&normalized).expect("valid normalized events");
        assert!(events.iter().any(|event| matches!(
            &event.event.data,
            TapeEventData::CodeRead(read)
                if read.file == "/workspace/demo/src/main.rs"
                    && read.range.start == 1
                    && read.range.end == 2
        )));
        assert!(events.iter().any(|event| matches!(
            &event.event.data,
            TapeEventData::CodeEdit(edit)
                if edit.file == "/workspace/demo/src/main.rs"
                    && edit.before_text.as_deref() == Some("fn before() {}")
                    && edit.after_text.as_deref() == Some("fn after() {}")
        )));
        assert!(events.iter().any(|event| matches!(
            &event.event.data,
            TapeEventData::CodeEdit(edit)
                if edit.file == "/workspace/demo/src/new.rs"
                    && edit.before_text.is_none()
                    && edit.after_text.as_deref() == Some("fn created() {}\n")
        )));
        assert!(rows.iter().any(|row| {
            row["k"] == "tool.call" && row["tool"] == "bash" && row["call_id"].as_str().is_some()
        }));
    }

    #[test]
    fn shared_session_and_message_shape_is_accepted_for_versions_one_through_three() {
        for version in 1..=3 {
            let input = format!(
                r#"{{"type":"session","version":{version},"id":"pi-v{version}","timestamp":"2026-10-08T10:00:00Z","cwd":"/repo"}}
{{"type":"message","id":"user","parentId":null,"timestamp":"2026-10-08T10:00:01Z","message":{{"role":"user","content":"hello","timestamp":1791453601000}}}}
"#
            );
            assert!(adapter_claims_input(AdapterId::Pi, &input));
            let normalized = pi_jsonl_to_tape_jsonl(&input).expect("Pi session should parse");
            assert!(normalized.lines().any(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|event| event["k"] == "msg.in")
            }));
        }
    }

    #[test]
    fn tool_pairs_and_session_metadata_survive_incremental_chunks() {
        let mut state = PiState::default();
        let first = concat!(
            r#"{"type":"session","version":3,"id":"split-session","timestamp":"2026-10-08T10:00:00Z","cwd":"/repo"}"#,
            "\n",
            r#"{"type":"message","id":"call-entry","parentId":null,"timestamp":"2026-10-08T10:00:01Z","message":{"role":"assistant","content":[{"type":"toolCall","id":"read-1","name":"read","arguments":{"path":"/repo/src/lib.rs","offset":0,"limit":1}}],"provider":"openai","model":"gpt-5","timestamp":1791453601000}}"#,
            "\n"
        );
        let first_normalized = pi_jsonl_incremental(first, &mut state).expect("first chunk");
        let serialized = serde_json::to_string(&state).expect("state serialization");
        let mut state: PiState = serde_json::from_str(&serialized).expect("state restore");
        let second = concat!(
            r#"{"type":"message","id":"result-entry","parentId":"call-entry","timestamp":"2026-10-08T10:00:02Z","message":{"role":"toolResult","toolCallId":"read-1","toolName":"read","content":[{"type":"text","text":"line 1"}],"isError":false,"timestamp":1791453602000}}"#,
            "\n"
        );
        let second_normalized = pi_jsonl_incremental(second, &mut state).expect("second chunk");
        assert!(first_normalized.contains("\"harness\":\"pi\""));
        assert!(second_normalized.contains("\"session_id\":\"split-session\""));
        assert!(second_normalized.contains("\"k\":\"code.read\""));
    }
}
