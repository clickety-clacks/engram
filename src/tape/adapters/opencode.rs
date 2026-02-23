use serde_json::Error;

pub fn opencode_json_to_tape_jsonl(input: &str) -> Result<String, Error> {
    super::super::harness::opencode_json_to_tape_jsonl(input)
}
