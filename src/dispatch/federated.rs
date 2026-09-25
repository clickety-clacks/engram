use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Received,
    Sent,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DispatchRow {
    pub store: String,
    pub tape_id: String,
    pub uuid: String,
    pub first_turn_index: i64,
    pub direction: Direction,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Segment {
    pub tape_id: String,
    pub message_turn_start: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct History {
    pub tip: String,
    /// Newest to oldest, matching the owner `tape_facts` contract.
    pub segments: Vec<Segment>,
    pub complete: bool,
    pub missing: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FirstOccurrence {
    pub row: DispatchRow,
    pub global_turn: i64,
}

impl History {
    pub fn from_tape_facts(facts: &Value) -> Result<Self, String> {
        if facts.get("status").and_then(Value::as_str) != Some("ok") {
            return Err("tape facts are not successful".into());
        }
        let tip = facts
            .get("tape_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "tape facts omitted tape_id".to_string())?
            .to_string();
        let values = facts
            .get("predecessor_chain")
            .and_then(Value::as_array)
            .ok_or_else(|| "tape facts omitted predecessor_chain".to_string())?;
        let mut segments = Vec::with_capacity(values.len());
        for value in values {
            let tape_id = value
                .get("tape_id")
                .and_then(Value::as_str)
                .ok_or_else(|| "predecessor segment omitted tape_id".to_string())?;
            let message_turn_start = value
                .get("message_turn_start")
                .and_then(Value::as_i64)
                .ok_or_else(|| "predecessor segment omitted message_turn_start".to_string())?;
            segments.push(Segment {
                tape_id: tape_id.to_string(),
                message_turn_start,
            });
        }
        if segments.first().map(|segment| segment.tape_id.as_str()) != Some(tip.as_str()) {
            return Err("predecessor_chain does not start at its requested tape".into());
        }
        let chain_status = facts
            .get("chain_status")
            .and_then(Value::as_str)
            .ok_or_else(|| "tape facts omitted chain_status".to_string())?;
        let complete = chain_status == "complete";
        let missing = facts
            .get("unresolved_predecessor")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        Ok(Self {
            tip,
            segments,
            complete,
            missing,
        })
    }

    pub fn tape_ids(&self) -> Vec<String> {
        self.segments
            .iter()
            .map(|segment| segment.tape_id.clone())
            .collect()
    }

    pub fn contains(&self, tape_id: &str) -> bool {
        self.segments
            .iter()
            .any(|segment| segment.tape_id == tape_id)
    }

    pub fn first_occurrences(
        &self,
        rows_by_tape: &HashMap<String, Vec<DispatchRow>>,
    ) -> BTreeMap<String, FirstOccurrence> {
        let mut first = BTreeMap::<String, FirstOccurrence>::new();
        for segment in self.segments.iter().rev() {
            let Some(rows) = rows_by_tape.get(&segment.tape_id) else {
                continue;
            };
            for row in rows {
                let global_turn = segment.message_turn_start + row.first_turn_index;
                let occurrence = FirstOccurrence {
                    row: row.clone(),
                    global_turn,
                };
                let replace = match first.get(&row.uuid) {
                    None => true,
                    Some(previous) => {
                        global_turn < previous.global_turn
                            || (global_turn == previous.global_turn
                                && row.direction == Direction::Received
                                && previous.row.direction == Direction::Sent)
                    }
                };
                if replace {
                    first.insert(row.uuid.clone(), occurrence);
                }
            }
        }
        first
    }

    pub fn latest_received_before(
        &self,
        rows_by_tape: &HashMap<String, Vec<DispatchRow>>,
        cutoff_global_turn: i64,
    ) -> Option<FirstOccurrence> {
        self.first_occurrences(rows_by_tape)
            .into_values()
            .filter(|occurrence| {
                occurrence.row.direction == Direction::Received
                    && occurrence.global_turn < cutoff_global_turn
            })
            .min_by(|left, right| {
                right
                    .global_turn
                    .cmp(&left.global_turn)
                    .then_with(|| left.row.uuid.cmp(&right.row.uuid))
            })
    }
}

pub fn dispatch_rows_from_values(
    values: &[Value],
    expected_store: &str,
) -> Result<Vec<DispatchRow>, String> {
    let mut rows = Vec::with_capacity(values.len());
    for value in values {
        let store = value
            .get("store")
            .and_then(Value::as_str)
            .ok_or_else(|| "dispatch row omitted store".to_string())?;
        let tape_id = value
            .get("tape_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "dispatch row omitted tape_id".to_string())?;
        let uuid = value
            .get("uuid")
            .and_then(Value::as_str)
            .ok_or_else(|| "dispatch row omitted uuid".to_string())?;
        let first_turn_index = value
            .get("first_turn_index")
            .and_then(Value::as_i64)
            .filter(|turn| *turn >= 0)
            .ok_or_else(|| "dispatch row omitted a non-negative first turn".to_string())?;
        let direction = match value.get("direction").and_then(Value::as_str) {
            Some("received") => Direction::Received,
            Some("sent") => Direction::Sent,
            _ => return Err("dispatch row has an invalid direction".into()),
        };
        if store != expected_store || tape_id.is_empty() || uuid.is_empty() {
            return Err("dispatch row has a different store or an empty identity".into());
        }
        rows.push(DispatchRow {
            store: store.to_string(),
            tape_id: tape_id.to_string(),
            uuid: uuid.to_string(),
            first_turn_index,
            direction,
        });
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn folds_first_occurrences_across_predecessor_segments_before_selecting_receive() {
        let facts = json!({
            "status":"ok",
            "tape_id":"tip",
            "chain_status":"complete",
            "predecessor_chain":[
                {"tape_id":"tip","message_turn_start":20},
                {"tape_id":"base","message_turn_start":0}
            ]
        });
        let history = History::from_tape_facts(&facts).expect("valid history");
        let mut rows = HashMap::new();
        rows.insert(
            "base".into(),
            vec![DispatchRow {
                store: "owner/default".into(),
                tape_id: "base".into(),
                uuid: "same".into(),
                first_turn_index: 5,
                direction: Direction::Sent,
            }],
        );
        rows.insert(
            "tip".into(),
            vec![DispatchRow {
                store: "owner/default".into(),
                tape_id: "tip".into(),
                uuid: "same".into(),
                first_turn_index: 1,
                direction: Direction::Received,
            }],
        );
        assert!(history.latest_received_before(&rows, 30).is_none());
        assert_eq!(
            history.first_occurrences(&rows)["same"].row.direction,
            Direction::Sent
        );
    }

    #[test]
    fn chooses_latest_received_and_uses_uuid_for_equal_turns() {
        let facts = json!({
            "status":"ok",
            "tape_id":"one",
            "chain_status":"complete",
            "predecessor_chain":[{"tape_id":"one","message_turn_start":0}]
        });
        let history = History::from_tape_facts(&facts).expect("valid history");
        let rows = HashMap::from([(
            "one".into(),
            ["z", "a"]
                .into_iter()
                .map(|uuid| DispatchRow {
                    store: "owner/default".into(),
                    tape_id: "one".into(),
                    uuid: uuid.into(),
                    first_turn_index: 2,
                    direction: Direction::Received,
                })
                .collect(),
        )]);
        assert_eq!(
            history.latest_received_before(&rows, 3).unwrap().row.uuid,
            "a"
        );
    }
}
