//! The two retrieval tools the model may call (R11), genai-free so the loop is
//! testable with a fake provider. `tool_specs` describes them; `exec_tool` runs
//! them against the real store and returns a JSON string for the model.

use crate::store::{Store, TimeRange};
use crate::vice::ViceError;
use serde_json::{json, Value};

/// A provider-agnostic tool description; `client.rs` maps it to a genai `Tool`.
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: Value,
}

pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "search_chat",
            description: "Full-text keyword search over all past chat messages \
                          and scribe notes. Write your own multi-keyword query; \
                          words are AND-ed. Returns the best matches.",
            schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Keywords to search for" }
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: "get_messages",
            description: "Fetch chat messages in a unix-epoch-seconds time \
                          window, oldest first. All fields optional.",
            schema: json!({
                "type": "object",
                "properties": {
                    "start": { "type": "integer", "description": "Earliest ts (inclusive)" },
                    "end":   { "type": "integer", "description": "Latest ts (inclusive)" },
                    "limit": { "type": "integer", "description": "Max messages to return" }
                }
            }),
        },
    ]
}

/// Execute a tool call against the store, returning a JSON string result.
pub fn exec_tool(store: &Store, name: &str, args: &Value) -> Result<String, ViceError> {
    match name {
        "search_chat" => {
            let query = args
                .get("query")
                .and_then(Value::as_str)
                .ok_or_else(|| ViceError::Tool("search_chat: missing `query`".into()))?;
            let hits = store
                .search_chat(query)
                .map_err(|e| ViceError::Sql(e.to_string()))?;
            let out: Vec<Value> = hits
                .into_iter()
                .map(|h| {
                    json!({
                        "ref_id": h.ref_id,
                        "kind": h.kind,
                        "author": h.author,
                        "ts": h.ts,
                        "source": h.source,
                        "body": h.body,
                    })
                })
                .collect();
            Ok(serde_json::to_string(&out)?)
        }
        "get_messages" => {
            let range = TimeRange {
                start: args.get("start").and_then(Value::as_i64),
                end: args.get("end").and_then(Value::as_i64),
                limit: args.get("limit").and_then(Value::as_i64),
            };
            let msgs = store
                .get_messages(&range)
                .map_err(|e| ViceError::Sql(e.to_string()))?;
            Ok(serde_json::to_string(&msgs)?)
        }
        other => Err(ViceError::Tool(format!("unknown tool `{other}`"))),
    }
}
