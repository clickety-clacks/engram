use serde_json::Error;

pub fn codex_jsonl_to_tape_jsonl(input: &str) -> Result<String, Error> {
    super::super::harness::codex_jsonl_to_tape_jsonl(input)
}
