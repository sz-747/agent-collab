//! U6 verification. Oracle r3: a fake provider returns a canned tool_call->text
//! sequence; the loop runs the *real* search_chat against a seeded store. Live
//! provider calls are never asserted here (would be r1).

use agent_collab::config::{author_hash, Config, Identity};
use agent_collab::git::Git;
use agent_collab::message::{Kind, Message};
use agent_collab::store::Store;
use agent_collab::sync::SyncEngine;
use agent_collab::vice::client::GenaiClient;
use agent_collab::vice::tools::exec_tool;
use agent_collab::vice::{
    dispatch, run_tool_loop, Convo, ModelClient, ToolCall, Turn, ViceCommand, ViceError,
};
use serde_json::json;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

/// A scripted model: each `next_turn` pops the next canned turn.
struct FakeModel {
    turns: Mutex<VecDeque<Turn>>,
    model: String,
}

impl FakeModel {
    fn new(model: &str, turns: Vec<Turn>) -> FakeModel {
        FakeModel {
            turns: Mutex::new(turns.into_iter().collect()),
            model: model.to_string(),
        }
    }
}

impl ModelClient for FakeModel {
    async fn next_turn(&self, _convo: &Convo) -> Result<Turn, ViceError> {
        let next = self.turns.lock().unwrap().pop_front();
        next.ok_or_else(|| ViceError::Model("fake script exhausted".into()))
    }
    fn model_id(&self) -> &str {
        &self.model
    }
}

fn seed_store() -> Store {
    let s = Store::open_in_memory().unwrap();
    s.upsert_message(&Message::new(
        "m1",
        "Alice",
        Kind::Human,
        None,
        100,
        "we need a payment gateway",
    ))
    .unwrap();
    s
}

#[tokio::test]
async fn loop_runs_real_tool_then_returns_final_text() {
    let store = seed_store();
    let model = FakeModel::new(
        "fake-1",
        vec![
            Turn::ToolCalls(vec![ToolCall {
                id: "c1".into(),
                name: "search_chat".into(),
                args: json!({ "query": "payment" }),
            }]),
            Turn::Final("Based on the chat, you discussed a payment gateway.".into()),
        ],
    );
    let out = run_tool_loop(&model, &store, "sys", "what about payments?", 8)
        .await
        .unwrap();
    assert!(out.contains("payment gateway"));
}

#[test]
fn exec_tool_search_hits_the_store() {
    let store = seed_store();
    let out = exec_tool(&store, "search_chat", &json!({ "query": "payment" })).unwrap();
    assert!(out.contains("payment gateway"));
    assert!(out.contains("m1"));
}

#[test]
fn exec_tool_rejects_unknown_tool() {
    let store = seed_store();
    assert!(exec_tool(&store, "nope", &json!({})).is_err());
}

#[test]
fn genai_client_errors_when_key_env_missing() {
    let cfg = Config {
        provider: "anthropic".into(),
        model: "claude-opus-4-8".into(),
        base_url: None,
        api_key_env: "DEFINITELY_UNSET_VICE_TEST_KEY".into(),
    };
    // Must fail before any network call (R9).
    assert!(matches!(
        GenaiClient::new(&cfg),
        Err(ViceError::Config(_))
    ));
}

// --- git-backed dispatch / scribe tests -------------------------------------

const BRANCH: &str = "collab/topic";

fn git(cwd: &Path, args: &[&str]) {
    let st = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .status()
        .expect("git spawnable");
    assert!(st.success(), "git {args:?} failed in {cwd:?}");
}

fn origin_with_collab() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("origin.git");
    git(tmp.path(), &["init", "--bare", "-b", "main", "origin.git"]);
    let seed = tmp.path().join("seed");
    git(
        tmp.path(),
        &["clone", bare.to_str().unwrap(), seed.to_str().unwrap()],
    );
    git(&seed, &["config", "user.name", "Seed"]);
    git(&seed, &["config", "user.email", "seed@example.com"]);
    std::fs::write(seed.join("README.md"), "hi").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "init"]);
    git(&seed, &["push", "origin", "main"]);
    git(&seed, &["checkout", "-b", BRANCH]);
    git(&seed, &["push", "-u", "origin", BRANCH]);
    (tmp, bare)
}

async fn engine(bare: &Path, dir: &Path, name: &str, email: &str) -> SyncEngine {
    git(
        dir.parent().unwrap(),
        &["clone", bare.to_str().unwrap(), dir.to_str().unwrap()],
    );
    git(dir, &["config", "user.name", name]);
    git(dir, &["config", "user.email", email]);
    let g = Git::new(dir);
    g.ensure_branch(BRANCH).await.unwrap();
    let id = Identity {
        name: name.to_string(),
        email: email.to_string(),
        author_hash: author_hash(email),
    };
    SyncEngine::new(dir.to_path_buf(), g, Store::open_in_memory().unwrap(), id)
}

#[tokio::test]
async fn reply_posts_ai_message_tagged_with_model() {
    let (tmp, bare) = origin_with_collab();
    let a = engine(&bare, &tmp.path().join("a"), "Alice", "alice@example.com").await;
    let b = engine(&bare, &tmp.path().join("b"), "Bob", "bob@example.com").await;

    let model = FakeModel::new("deepseek-chat", vec![Turn::Final("noted".into())]);
    dispatch(ViceCommand::Reply("hi".into()), &model, &a)
        .await
        .unwrap();

    let new = b.poll().await.unwrap();
    let ai: Vec<_> = new.iter().filter(|m| m.kind == Kind::Ai).collect();
    assert_eq!(ai.len(), 1);
    assert_eq!(ai[0].body, "noted");
    assert_eq!(ai[0].model.as_deref(), Some("deepseek-chat")); // R10
}

#[tokio::test]
async fn write_notes_creates_then_overwrites_and_indexes() {
    let (tmp, bare) = origin_with_collab();
    let a = engine(&bare, &tmp.path().join("a"), "Alice", "alice@example.com").await;

    let model = FakeModel::new(
        "fake",
        vec![
            Turn::Final("# Notes\nWe will use postgres.".into()),
            Turn::Final("# Notes\nWe switched to sqlite.".into()),
        ],
    );

    dispatch(ViceCommand::WriteNotes("first".into()), &model, &a)
        .await
        .unwrap();
    assert_eq!(a.store().search_chat("postgres").unwrap().len(), 1);
    let path = tmp.path().join("a").join("docs").join("notes.md");
    assert!(path.exists());

    // Second write overwrites in place (R14).
    dispatch(ViceCommand::WriteNotes("second".into()), &model, &a)
        .await
        .unwrap();
    assert!(a.store().search_chat("postgres").unwrap().is_empty());
    assert_eq!(a.store().search_chat("sqlite").unwrap().len(), 1);
    assert!(std::fs::read_to_string(&path).unwrap().contains("sqlite"));
}

#[tokio::test]
async fn scribe_doc_collision_is_last_write_wins() {
    let (tmp, bare) = origin_with_collab();
    let a = engine(&bare, &tmp.path().join("a"), "Alice", "alice@example.com").await;
    let b = engine(&bare, &tmp.path().join("b"), "Bob", "bob@example.com").await;

    // A writes the shared doc first.
    a.write_doc("docs/shared.md", "A version").await.unwrap();
    // B is stale and writes the same path -> push reject -> LWW local wins (KTD9).
    b.write_doc("docs/shared.md", "B version").await.unwrap();

    // A fresh clone of origin must see B's content.
    let verify = tmp.path().join("verify");
    git(
        tmp.path(),
        &["clone", "-b", BRANCH, bare.to_str().unwrap(), verify.to_str().unwrap()],
    );
    let content = std::fs::read_to_string(verify.join("docs").join("shared.md")).unwrap();
    assert_eq!(content, "B version");
}
