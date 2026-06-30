//! U7 verification. Oracle r5: real git for clone/branch/gitignore outcomes;
//! assertions are against actual repo state.

use agent_collab::config::{author_hash, Identity};
use agent_collab::git::Git;
use agent_collab::message::Kind;
use agent_collab::session::{ensure_gitignore, join_session, start_session};
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

fn current_branch(repo: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
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
    set_identity(&seed, "Seed", "seed@example.com");
    std::fs::write(seed.join("README.md"), "hi").unwrap();
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "init"]);
    git(&seed, &["push", "origin", "main"]);
    git(&seed, &["checkout", "-b", BRANCH]);
    git(&seed, &["push", "-u", "origin", BRANCH]);
    (tmp, bare)
}

fn clone(bare: &Path, into: &Path, name: &str, email: &str) {
    git(
        into.parent().unwrap(),
        &["clone", bare.to_str().unwrap(), into.to_str().unwrap()],
    );
    set_identity(into, name, email);
}

async fn sender_engine(dir: &Path, name: &str, email: &str) -> SyncEngine {
    let g = Git::new(dir);
    g.ensure_branch(BRANCH).await.unwrap();
    let id = Identity {
        name: name.to_string(),
        email: email.to_string(),
        author_hash: author_hash(email),
    };
    SyncEngine::new(dir.to_path_buf(), g, Store::open_in_memory().unwrap(), id)
}

#[test]
fn ensure_gitignore_creates_then_is_idempotent() {
    let d = tempfile::tempdir().unwrap();
    assert!(ensure_gitignore(d.path()).unwrap()); // created
    assert!(!ensure_gitignore(d.path()).unwrap()); // no change second time
    let gi = std::fs::read_to_string(d.path().join(".gitignore")).unwrap();
    assert!(gi.contains("chat.db"));
    assert!(gi.contains(".vice.toml"));
}

#[tokio::test]
async fn start_session_checks_out_collab_and_ignores_cache() {
    let (tmp, bare) = origin_with_collab();
    let c = tmp.path().join("c");
    clone(&bare, &c, "Alice", "alice@example.com");

    let s = start_session(&c, "topic").await.unwrap();
    assert_eq!(current_branch(&c), BRANCH); // on collab branch (main untouched, R4)

    let gi = std::fs::read_to_string(c.join(".gitignore")).unwrap();
    assert!(gi.contains("chat.db") && gi.contains(".vice.toml"));
    assert!(s.model.is_none()); // no .vice.toml -> @vice disabled
}

#[tokio::test]
async fn startup_poll_backfills_existing_history() {
    let (tmp, bare) = origin_with_collab();

    // A peer has already said something in the room.
    let a = tmp.path().join("a");
    clone(&bare, &a, "Alice", "alice@example.com");
    let eng = sender_engine(&a, "Alice", "alice@example.com").await;
    eng.send("decided on git transport", Kind::Human, None)
        .await
        .unwrap();

    // A fresh peer starts a session and backfills.
    let b = tmp.path().join("b");
    clone(&bare, &b, "Bob", "bob@example.com");
    let s = start_session(&b, "topic").await.unwrap();
    let news = s.engine.poll().await.unwrap();
    assert!(news.iter().any(|m| m.body == "decided on git transport"));
}

#[tokio::test]
async fn join_session_clones_and_enters_collab_branch() {
    let (tmp, bare) = origin_with_collab();
    let into = tmp.path().join("joined");
    let s = join_session(bare.to_str().unwrap(), "topic", &into)
        .await
        .unwrap();
    assert_eq!(current_branch(&into), BRANCH);
    assert!(into.join(".gitignore").exists());
    drop(s);
}
