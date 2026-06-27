//! U4 verification. Oracle r5: real git temp repos define propagation truth;
//! the dedup/new-set logic (r3) is asserted against scripted send/poll order.

use agent_collab::config::{author_hash, Identity};
use agent_collab::git::Git;
use agent_collab::message::Kind;
use agent_collab::store::Store;
use agent_collab::sync::SyncEngine;
use std::path::Path;

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

fn set_identity(repo: &Path, name: &str, email: &str) {
    git(repo, &["config", "user.name", name]);
    git(repo, &["config", "user.email", email]);
}

/// Bare origin with `main` and an empty `collab/topic` branch already pushed.
fn origin_with_collab() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("origin.git");
    git(tmp.path(), &["init", "--bare", "-b", "main", "origin.git"]);

    let seed = tmp.path().join("seed");
    git(
        tmp.path(),
        &["clone", bare.to_str().unwrap(), seed.to_str().unwrap()],
    );
    set_identity(&seed, "Seed", "seed@example.com");
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
    set_identity(dir, name, email);
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
async fn a_sends_b_receives_exactly_once() {
    let (tmp, bare) = origin_with_collab();
    let a = engine(&bare, &tmp.path().join("a"), "Alice", "alice@example.com").await;
    let b = engine(&bare, &tmp.path().join("b"), "Bob", "bob@example.com").await;

    a.send("hello from A", Kind::Human, None).await.unwrap();

    let new = b.poll().await.unwrap();
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].body, "hello from A");
    assert_eq!(new[0].author, "Alice");
    assert_eq!(new[0].kind, Kind::Human);

    // Second poll with no new pushes -> empty (dedup via upsert).
    assert!(b.poll().await.unwrap().is_empty());
}

#[tokio::test]
async fn interleaved_sends_no_loss_no_dup() {
    let (tmp, bare) = origin_with_collab();
    let a = engine(&bare, &tmp.path().join("a"), "Alice", "alice@example.com").await;
    let b = engine(&bare, &tmp.path().join("b"), "Bob", "bob@example.com").await;

    a.send("A1", Kind::Human, None).await.unwrap();
    b.send("B1", Kind::Human, None).await.unwrap(); // stale -> retry rebases

    let a_new = a.poll().await.unwrap();
    let b_new = b.poll().await.unwrap();

    assert_eq!(a_new.iter().map(|m| &m.body).collect::<Vec<_>>(), vec!["B1"]);
    assert_eq!(b_new.iter().map(|m| &m.body).collect::<Vec<_>>(), vec!["A1"]);
    // Both stores converge; re-polls are empty.
    assert!(a.poll().await.unwrap().is_empty());
    assert!(b.poll().await.unwrap().is_empty());
}

#[tokio::test]
async fn ai_message_keeps_model_tag() {
    let (tmp, bare) = origin_with_collab();
    let a = engine(&bare, &tmp.path().join("a"), "Alice", "alice@example.com").await;
    let b = engine(&bare, &tmp.path().join("b"), "Bob", "bob@example.com").await;

    a.send("noted", Kind::Ai, Some("deepseek-chat".into()))
        .await
        .unwrap();

    let new = b.poll().await.unwrap();
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].kind, Kind::Ai);
    assert_eq!(new[0].model.as_deref(), Some("deepseek-chat"));
}

#[tokio::test]
async fn malformed_jsonl_line_is_skipped() {
    let (tmp, bare) = origin_with_collab();
    let b = engine(&bare, &tmp.path().join("b"), "Bob", "bob@example.com").await;

    // Hand-write a chat file with a valid line, a garbage line, then a partial
    // (truncated) trailing line — simulating an interrupted write.
    let chat = tmp.path().join("b").join("chat");
    std::fs::create_dir_all(&chat).unwrap();
    let good = r#"{"id":"x1","author":"Alice","kind":"human","ts":5,"body":"real msg"}"#;
    std::fs::write(
        chat.join("deadbeef.jsonl"),
        format!("{good}\nnot json at all\n{{\"id\":\"x2\",\"aut"),
    )
    .unwrap();

    let new = b.reconcile().unwrap();
    assert_eq!(new.len(), 1);
    assert_eq!(new[0].id, "x1");
}
