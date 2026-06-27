//! U3 verification. Oracle r5: real `git` in temp repos is the authority for
//! push/rebase/branch outcomes — no mocks. We drive actual collisions and
//! assert the typed results.

use agent_collab::git::{Git, PushOutcome};
use std::path::Path;

/// Run a git command to success in `cwd` (test setup helper).
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

fn set_identity(repo: &Path) {
    git(repo, &["config", "user.name", "Test User"]);
    git(repo, &["config", "user.email", "test@example.com"]);
}

fn write(repo: &Path, name: &str, body: &str) {
    std::fs::write(repo.join(name), body).unwrap();
}

/// Build a bare "origin" with one commit on `main`, return (tempdir, bare_path).
fn origin_with_main() -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let bare = tmp.path().join("origin.git");
    git(tmp.path(), &["init", "--bare", "-b", "main", "origin.git"]);

    let seed = tmp.path().join("seed");
    git(
        tmp.path(),
        &["clone", bare.to_str().unwrap(), seed.to_str().unwrap()],
    );
    set_identity(&seed);
    write(&seed, "README.md", "hello");
    git(&seed, &["add", "."]);
    git(&seed, &["commit", "-m", "init"]);
    git(&seed, &["push", "origin", "main"]);
    (tmp, bare)
}

fn clone_of(bare: &Path, into: &Path) -> Git {
    git(
        into.parent().unwrap(),
        &["clone", bare.to_str().unwrap(), into.to_str().unwrap()],
    );
    set_identity(into);
    Git::new(into)
}

#[tokio::test]
async fn fresh_clone_push_succeeds() {
    let (tmp, bare) = origin_with_main();
    let c = tmp.path().join("c");
    let g = clone_of(&bare, &c);
    write(&c, "a.txt", "from A");
    g.add(&["a.txt"]).await.unwrap();
    g.commit("add a").await.unwrap();
    assert_eq!(g.push().await.unwrap(), PushOutcome::Pushed);
}

#[tokio::test]
async fn stale_push_is_rejected_then_retry_rebases() {
    let (tmp, bare) = origin_with_main();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    let ga = clone_of(&bare, &a);
    let gb = clone_of(&bare, &b);

    // A advances origin/main.
    write(&a, "a.txt", "A");
    ga.add(&["a.txt"]).await.unwrap();
    ga.commit("A commit").await.unwrap();
    assert_eq!(ga.push().await.unwrap(), PushOutcome::Pushed);

    // B (now stale) commits a *different* file -> non-ff rejection.
    write(&b, "b.txt", "B");
    gb.add(&["b.txt"]).await.unwrap();
    gb.commit("B commit").await.unwrap();
    assert_eq!(gb.push().await.unwrap(), PushOutcome::Rejected);

    // Retry rebases over A's commit and succeeds.
    assert_eq!(gb.push_with_retry(3).await.unwrap(), PushOutcome::Pushed);
    assert!(b.join("a.txt").exists(), "rebase pulled in A's file");
}

#[tokio::test]
async fn bad_remote_is_failed_not_a_loop() {
    let (tmp, bare) = origin_with_main();
    let c = tmp.path().join("c");
    let g = clone_of(&bare, &c);
    // Point origin at a path that is not a repo.
    let bogus = tmp.path().join("does-not-exist.git");
    git(&c, &["remote", "set-url", "origin", bogus.to_str().unwrap()]);
    write(&c, "a.txt", "x");
    g.add(&["a.txt"]).await.unwrap();
    g.commit("c").await.unwrap();

    match g.push_with_retry(3).await.unwrap() {
        PushOutcome::Failed(_) => {} // returns, does not loop on rebase
        other => panic!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn ensure_branch_creates_collab_and_leaves_main() {
    let (tmp, bare) = origin_with_main();
    let c = tmp.path().join("c");
    let g = clone_of(&bare, &c);
    let main_rev_before = std::process::Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(&c)
        .output()
        .unwrap();

    g.ensure_branch("collab/topic").await.unwrap();
    assert_eq!(g.current_branch().await.unwrap(), "collab/topic");

    let main_rev_after = std::process::Command::new("git")
        .args(["rev-parse", "main"])
        .current_dir(&c)
        .output()
        .unwrap();
    assert_eq!(
        main_rev_before.stdout, main_rev_after.stdout,
        "main must be untouched"
    );
}
