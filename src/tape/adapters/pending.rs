use std::collections::HashMap;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What an OpenClaw or Cursor conversion holds at the end of its input that a
/// conversion of the bytes after it needs: tool calls not yet paired with a
/// result, and, for Cursor only, the session cwd and first timestamp. Carrying
/// it across appends lets a tail pair a result with a call from an earlier
/// tail, as one conversion of the whole file does.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PendingCalls {
    /// Each entry is the reader's call context with its `call_id`, in
    /// `call_id` order.
    #[serde(default)]
    pub calls: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_timestamp: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Entry<T> {
    call_id: String,
    #[serde(flatten)]
    context: T,
}

pub(crate) fn load_calls<T: DeserializeOwned>(
    calls: &[Value],
) -> Result<HashMap<String, T>, serde_json::Error> {
    calls
        .iter()
        .map(|value| {
            let entry = Entry::<T>::deserialize(value)?;
            Ok((entry.call_id, entry.context))
        })
        .collect()
}

pub(crate) fn store_calls<'a, T: Serialize + 'a>(
    calls: impl IntoIterator<Item = (&'a String, &'a T)>,
) -> Result<Vec<Value>, serde_json::Error> {
    let mut calls: Vec<_> = calls.into_iter().collect();
    calls.sort_by(|a, b| a.0.cmp(b.0));
    calls
        .into_iter()
        .map(|(call_id, context)| {
            serde_json::to_value(Entry {
                call_id: call_id.clone(),
                context,
            })
        })
        .collect()
}
