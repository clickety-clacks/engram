use serde_json::Error;

pub fn gemini_json_to_tape_jsonl(input: &str) -> Result<String, Error> {
    super::super::harness::gemini_json_to_tape_jsonl(input)
}
