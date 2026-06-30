//! `@vice` invocation: verb parsing, the genai-free tool-use loop, and dispatch
//! (reply / file write / scribe notes). The model itself is behind the
//! `ModelClient` trait so the loop is exercised by a fake provider in tests
//! (oracle r3); live calls are never asserted in CI.

pub mod client;
pub mod tools;

use crate::config::ConfigError;
use crate::message::{Kind, Message};
use crate::store::Store;
use crate::sync::{SyncEngine, SyncError};
use serde_json::Value;

#[derive(Debug)]
pub enum ViceError {
    Tool(String),
    /// The model kept calling tools past the step budget.
    LoopExhausted,
    Sync(SyncError),
    Sql(String),
    Json(String),
    Model(String),
    Config(ConfigError),
    BadPath(String),
}

impl std::fmt::Display for ViceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ViceError::Tool(e) => write!(f, "tool error: {e}"),
            ViceError::LoopExhausted => write!(f, "@vice gave up after too many tool calls"),
            ViceError::Sync(e) => write!(f, "{e}"),
            ViceError::Sql(e) => write!(f, "sqlite: {e}"),
            ViceError::Json(e) => write!(f, "json: {e}"),
            ViceError::Model(e) => write!(f, "model error: {e}"),
            ViceError::Config(e) => write!(f, "{e}"),
            ViceError::BadPath(e) => write!(f, "bad doc path: {e}"),
        }
    }
}

impl std::error::Error for ViceError {}

impl From<serde_json::Error> for ViceError {
    fn from(e: serde_json::Error) -> Self {
        ViceError::Json(e.to_string())
    }
}
impl From<SyncError> for ViceError {
    fn from(e: SyncError) -> Self {
        ViceError::Sync(e)
    }
}

// --- Tool-use loop (genai-free) ---------------------------------------------

/// A tool invocation requested by the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// One model turn: either it wants tools run, or it produced final text.
#[derive(Debug, Clone)]
pub enum Turn {
    ToolCalls(Vec<ToolCall>),
    Final(String),
}

/// Result of running one tool, linked back to its call.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
}

/// One completed tool round in the running transcript.
#[derive(Debug, Clone)]
pub struct Step {
    pub calls: Vec<ToolCall>,
    pub results: Vec<ToolResult>,
}

/// The abstract transcript a `ModelClient` is asked to continue.
#[derive(Debug, Clone)]
pub struct Convo {
    pub system: String,
    pub user: String,
    pub steps: Vec<Step>,
}

/// One single-turn model call. Implemented by the real genai client and by the
/// fake provider in tests. Async fn in trait (stable) — used only generically.
pub trait ModelClient {
    fn next_turn(
        &self,
        convo: &Convo,
    ) -> impl std::future::Future<Output = Result<Turn, ViceError>> + Send;
    /// The actual model id, for tagging the AI message (R10).
    fn model_id(&self) -> &str;
}

/// Drive the model<->tool loop until it returns final text or exhausts the
/// step budget. Tools run against the real store (R11).
pub async fn run_tool_loop<M: ModelClient>(
    model: &M,
    store: &Store,
    system: &str,
    user: &str,
    max_steps: usize,
) -> Result<String, ViceError> {
    let mut convo = Convo {
        system: system.to_string(),
        user: user.to_string(),
        steps: Vec::new(),
    };
    for _ in 0..max_steps {
        match model.next_turn(&convo).await? {
            Turn::Final(text) => return Ok(text),
            Turn::ToolCalls(calls) => {
                let mut results = Vec::with_capacity(calls.len());
                for c in &calls {
                    let content = tools::exec_tool(store, &c.name, &c.args)?;
                    results.push(ToolResult {
                        id: c.id.clone(),
                        content,
                    });
                }
                convo.steps.push(Step { calls, results });
            }
        }
    }
    Err(ViceError::LoopExhausted)
}

// --- Verb parsing -----------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum ViceCommand {
    /// Read-only answer in the thread.
    Reply(String),
    /// Scribe doc to the default notes path (`@vice write notes ...`).
    WriteNotes(String),
    /// File write to an explicit path under docs/ (`@vice write <path> ...`).
    Write { path: String, instruction: String },
}

/// Parse the text *after* `@vice`. Two explicit write verbs; everything else is
/// a read-only reply (R7). `write` must be the first whole token — `writeup` is
/// a reply, not a write.
pub fn parse_command(text: &str) -> ViceCommand {
    let trimmed = text.trim();
    let mut tokens = trimmed.splitn(2, char::is_whitespace);
    let first = tokens.next().unwrap_or("");
    let rest = tokens.next().unwrap_or("").trim();

    if first != "write" {
        return ViceCommand::Reply(trimmed.to_string());
    }
    // first == "write"
    let mut rest_tokens = rest.splitn(2, char::is_whitespace);
    let second = rest_tokens.next().unwrap_or("");
    let after = rest_tokens.next().unwrap_or("").trim();

    if second == "notes" {
        ViceCommand::WriteNotes(after.to_string())
    } else {
        ViceCommand::Write {
            path: second.to_string(),
            instruction: after.to_string(),
        }
    }
}

const DEFAULT_NOTES_PATH: &str = "docs/notes.md";

const REPLY_SYSTEM: &str = "You are @vice, a scribe and memory for two developers planning software. \
    Answer concisely. Use the search_chat and get_messages tools to ground answers in what was actually said.";

const NOTES_SYSTEM: &str = "You are @vice, the scribe for two developers. Produce the FULL markdown \
    document body requested — no preamble, no code fences around the whole thing. Use search_chat and \
    get_messages to ground the notes in the real discussion.";

// --- Dispatch ---------------------------------------------------------------

/// Run a parsed command: generate via the model loop, then post the reply or
/// write+sync the doc. Returns the AI message that was posted, so the invoker
/// can print it locally (their own messages never re-surface via poll).
pub async fn dispatch<M: ModelClient>(
    cmd: ViceCommand,
    model: &M,
    engine: &SyncEngine,
) -> Result<Message, ViceError> {
    match cmd {
        ViceCommand::Reply(q) => {
            let text = run_tool_loop(model, engine.store(), REPLY_SYSTEM, &q, 8).await?;
            let m = engine
                .send(&text, Kind::Ai, Some(model.model_id().to_string()))
                .await?;
            Ok(m)
        }
        ViceCommand::WriteNotes(instruction) => {
            write_doc(model, engine, DEFAULT_NOTES_PATH, &instruction).await
        }
        ViceCommand::Write { path, instruction } => {
            let safe = safe_doc_path(&path)?;
            write_doc(model, engine, &safe, &instruction).await
        }
    }
}

async fn write_doc<M: ModelClient>(
    model: &M,
    engine: &SyncEngine,
    path: &str,
    instruction: &str,
) -> Result<Message, ViceError> {
    let content = run_tool_loop(model, engine.store(), NOTES_SYSTEM, instruction, 8).await?;
    engine.write_doc(path, &content).await?; // overwrite + index + commit/push (LWW)
    // Post a short note so the other peer sees that a doc landed.
    let m = engine
        .send(
            &format!("wrote {path}"),
            Kind::Ai,
            Some(model.model_id().to_string()),
        )
        .await?;
    Ok(m)
}

/// Keep doc writes inside the repo under `docs/`; reject traversal/absolute paths.
fn safe_doc_path(path: &str) -> Result<String, ViceError> {
    if path.is_empty() {
        return Err(ViceError::BadPath("empty path".into()));
    }
    // Reject traversal, drive letters, and leading separators. (On Windows,
    // `Path::is_absolute("/abs")` is false — no drive — so check separators too.)
    if path.contains("..")
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains(':')
        || std::path::Path::new(path).is_absolute()
    {
        return Err(ViceError::BadPath(path.to_string()));
    }
    if path.starts_with("docs/") {
        Ok(path.to_string())
    } else {
        Ok(format!("docs/{path}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_verbs() {
        assert_eq!(
            parse_command("what did we decide about auth"),
            ViceCommand::Reply("what did we decide about auth".into())
        );
        assert_eq!(
            parse_command("write notes summary of today"),
            ViceCommand::WriteNotes("summary of today".into())
        );
        assert_eq!(
            parse_command("write design.md the architecture"),
            ViceCommand::Write {
                path: "design.md".into(),
                instruction: "the architecture".into()
            }
        );
        // `writeup` is not the write verb -> reply.
        assert_eq!(
            parse_command("writeup the plan"),
            ViceCommand::Reply("writeup the plan".into())
        );
    }

    #[test]
    fn safe_doc_path_guards_traversal() {
        assert_eq!(safe_doc_path("notes.md").unwrap(), "docs/notes.md");
        assert_eq!(safe_doc_path("docs/x.md").unwrap(), "docs/x.md");
        assert!(safe_doc_path("../etc/passwd").is_err());
        assert!(safe_doc_path("/abs").is_err());
        assert!(safe_doc_path("C:/win").is_err());
    }
}
