use serde_json::Error;

pub fn cursor_jsonl_to_tape_jsonl(input: &str) -> Result<String, Error> {
    super::super::harness::cursor_jsonl_to_tape_jsonl(input)
}
