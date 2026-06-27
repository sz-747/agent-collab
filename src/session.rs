//! Session bootstrap (U7): clone-or-use a room repo, check out the collab
//! branch, ensure the local cache + config are gitignored, open the store, and
//! build the sync engine + optional @vice model. `main.rs` only parses args and
//! calls in here; the logic lives in the library so it is testable against real
//! git (oracle r5).

use crate::config::{git_identity_in, Config};
use crate::git::{Git, GitError, PushOutcome};
use crate::store::Store;
use crate::sync::SyncEngine;
use crate::vice::client::GenaiClient;
use std::path::Path;

/// Local cache filename inside the room repo (gitignored, R1).
const DB_FILE: &str = "chat.db";

pub struct Session {
    pub engine: SyncEngine,
    /// None when no usable `.vice.toml` is present; `@vice` is then disabled.
    pub model: Option<GenaiClient>,
}

#[derive(Debug)]
pub enum SessionError {
    Git(GitError),
    Io(String),
    Store(String),
    Identity(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Git(e) => write!(f, "{e}"),
            SessionError::Io(e) => write!(f, "io: {e}"),
            SessionError::Store(e) => write!(f, "store: {e}"),
            SessionError::Identity(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<GitError> for SessionError {
    fn from(e: GitError) -> Self {
        SessionError::Git(e)
    }
}
impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        SessionError::Io(e.to_string())
    }
}

/// Derive the local clone directory name from a repo URL: the last path segment
/// with any `.git` suffix removed.
pub fn derive_clone_dir(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    let tail = trimmed
        .rsplit(['/', ':'])
        .next()
        .unwrap_or(trimmed);
    tail.strip_suffix(".git").unwrap_or(tail).to_string()
}

/// Clone `url` into `into`, then bootstrap the collab session.
pub async fn join_session(url: &str, topic: &str, into: &Path) -> Result<Session, SessionError> {
    let git = Git::clone(url, into).await?;
    bootstrap(git, into, topic).await
}

/// Bootstrap a session in an existing clone at `repo_dir`.
pub async fn start_session(repo_dir: &Path, topic: &str) -> Result<Session, SessionError> {
    bootstrap(Git::new(repo_dir), repo_dir, topic).await
}

async fn bootstrap(git: Git, repo_dir: &Path, topic: &str) -> Result<Session, SessionError> {
    let branch = format!("collab/{topic}");
    git.ensure_branch(&branch).await?; // never touches main (R4)

    // Make sure the room ignores the local cache + per-repo config (R1/R8). If
    // we changed it, commit+push so peers inherit the same ignores (best-effort).
    if ensure_gitignore(repo_dir)? {
        git.add(&[".gitignore"]).await?;
        git.commit("chore: ignore local cache and vice config").await?;
        if let Ok(PushOutcome::Failed(e)) = git.push_with_retry(5).await {
            eprintln!("warning: could not push .gitignore: {e}");
        }
    }

    let identity = git_identity_in(repo_dir).map_err(|e| SessionError::Identity(e.to_string()))?;
    let store = Store::open(&repo_dir.join(DB_FILE)).map_err(|e| SessionError::Store(e.to_string()))?;
    let engine = SyncEngine::new(repo_dir.to_path_buf(), git, store, identity);

    // Build the @vice model if config is present and a key resolves; otherwise
    // run chat-only. (Config errors here are non-fatal — @vice just stays off.)
    let model = Config::load_in_dir(repo_dir)
        .ok()
        .and_then(|cfg| GenaiClient::new(&cfg).ok());

    Ok(Session { engine, model })
}

/// Ensure `.gitignore` in `dir` excludes the local cache and `.vice.toml`.
/// Returns true if the file was modified.
pub fn ensure_gitignore(dir: &Path) -> std::io::Result<bool> {
    let path = dir.join(".gitignore");
    let mut content = std::fs::read_to_string(&path).unwrap_or_default();
    let needed = ["chat.db", "chat.db-*", "*.sqlite", ".vice.toml"];
    let mut modified = false;
    for line in needed {
        if !content.lines().any(|l| l.trim() == line) {
            if !content.is_empty() && !content.ends_with('\n') {
                content.push('\n');
            }
            content.push_str(line);
            content.push('\n');
            modified = true;
        }
    }
    if modified {
        std::fs::write(&path, content)?;
    }
    Ok(modified)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_clone_dir_strips_git_suffix() {
        assert_eq!(derive_clone_dir("https://github.com/foo/bar.git"), "bar");
        assert_eq!(derive_clone_dir("git@github.com:foo/baz.git"), "baz");
        assert_eq!(derive_clone_dir("https://x/y/repo/"), "repo");
        assert_eq!(derive_clone_dir("/local/path/room"), "room");
    }
}
