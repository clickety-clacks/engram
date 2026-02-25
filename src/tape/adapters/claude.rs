use serde_json::Error;

pub fn claude_jsonl_to_tape_jsonl(input: &str) -> Result<String, Error> {
    super::super::harness::claude_jsonl_to_tape_jsonl(input)
}
