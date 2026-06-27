//! Sync engine (KTD6): per-author append-only JSONL is the git-tracked source of
//! truth; the SQLite store is the derived cache. `send` appends to *our own*
//! file (so two peers never touch the same file — conflict-free, R2/R3) and
//! pushes; `poll` pulls, reconciles every author file into the store, and
//! returns just the newly-arrived messages to print.

use crate::config::Identity;
use crate::git::{Git, GitError, PushOutcome};
use crate::message::{Kind, Message};
use crate::store::Store;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use uuid::Uuid;

pub struct SyncEngine {
    repo: PathBuf,
    git: Git,
    store: Store,
    identity: Identity,
}

#[derive(Debug)]
pub enum SyncError {
    Io(String),
    Json(String),
    Git(GitError),
    Sql(String),
    PushRejected,
    PushFailed(String),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Io(e) => write!(f, "io: {e}"),
            SyncError::Json(e) => write!(f, "json: {e}"),
            SyncError::Git(e) => write!(f, "{e}"),
            SyncError::Sql(e) => write!(f, "sqlite: {e}"),
            SyncError::PushRejected => write!(f, "push still rejected after retries"),
            SyncError::PushFailed(e) => write!(f, "push failed: {e}"),
        }
    }
}

impl std::error::Error for SyncError {}

impl From<std::io::Error> for SyncError {
    fn from(e: std::io::Error) -> Self {
        SyncError::Io(e.to_string())
    }
}
impl From<serde_json::Error> for SyncError {
    fn from(e: serde_json::Error) -> Self {
        SyncError::Json(e.to_string())
    }
}
impl From<GitError> for SyncError {
    fn from(e: GitError) -> Self {
        SyncError::Git(e)
    }
}
impl From<rusqlite::Error> for SyncError {
    fn from(e: rusqlite::Error) -> Self {
        SyncError::Sql(e.to_string())
    }
}

impl SyncEngine {
    pub fn new(repo: PathBuf, git: Git, store: Store, identity: Identity) -> SyncEngine {
        SyncEngine {
            repo,
            git,
            store,
            identity,
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Compose, persist, and push a new message. Returns the stored message.
    pub async fn send(
        &self,
        body: &str,
        kind: Kind,
        model: Option<String>,
    ) -> Result<Message, SyncError> {
        let m = Message::new(
            Uuid::new_v4().to_string(),
            &self.identity.name, // human-readable author for display; id is the dedup key
            kind,
            model,
            now_ts(),
            body,
        );
        self.append_own(&m)?;
        self.store.upsert_message(&m)?;

        let rel = format!("chat/{}.jsonl", self.identity.author_hash);
        self.git.add(&[&rel]).await?;
        self.git.commit(&format!("msg {}", m.id)).await?;
        match self.git.push_with_retry(5).await? {
            PushOutcome::Pushed => Ok(m),
            PushOutcome::Rejected => Err(SyncError::PushRejected),
            PushOutcome::Failed(e) => Err(SyncError::PushFailed(e)),
        }
    }

    /// Pull, then reconcile. Pull is best-effort: a brand-new room has no remote
    /// branch yet, and an offline peer should still see local state.
    pub async fn poll(&self) -> Result<Vec<Message>, SyncError> {
        if let Err(e) = self.git.pull_rebase().await {
            eprintln!("warning: pull failed (continuing with local state): {e}");
        }
        self.reconcile()
    }

    /// Read every `chat/*.jsonl`, upsert into the store, and return the messages
    /// that were newly inserted (dedup by id, KTD6), oldest first. Own messages
    /// are already in the store from `send`, so they never re-surface here.
    /// Malformed/partial trailing lines are skipped, not fatal (resilience).
    pub fn reconcile(&self) -> Result<Vec<Message>, SyncError> {
        let chat = self.repo.join("chat");
        let mut fresh: Vec<Message> = Vec::new();
        if chat.is_dir() {
            for entry in std::fs::read_dir(&chat)? {
                let path = entry?.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let text = std::fs::read_to_string(&path)?;
                for line in text.lines() {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Message>(line) {
                        Ok(m) => {
                            if self.store.upsert_message(&m)? {
                                fresh.push(m);
                            }
                        }
                        Err(_) => continue, // skip malformed/partial line
                    }
                }
            }
        }
        fresh.sort_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
        Ok(fresh)
    }

    fn append_own(&self, m: &Message) -> Result<(), SyncError> {
        let dir = self.repo.join("chat");
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{}.jsonl", self.identity.author_hash));
        let mut f = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(f, "{}", serde_json::to_string(m)?)?;
        Ok(())
    }
}

fn now_ts() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
