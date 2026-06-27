//! Git transport: thin async wrappers over the installed `git` (KTD2).
//!
//! We shell out rather than embed gix/git2 so the user's existing git config,
//! credential helper, and SSH agent all work for free. Every invocation forces
//! a C locale and disables terminal prompts (KTD8) so output is machine-stable
//! and a missing credential never hangs the poll loop.

use std::path::{Path, PathBuf};
use tokio::process::Command;

pub struct Git {
    repo: PathBuf,
}

/// Result of a push attempt. `Rejected` specifically means a non-fast-forward
/// collision (retry via rebase); `Failed` is anything else (surface, never loop).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    Pushed,
    Rejected,
    Failed(String),
}

#[derive(Debug)]
pub enum GitError {
    /// `git` could not be spawned at all (not on PATH).
    Spawn(String),
    /// `git` ran but returned non-zero for a command we require to succeed.
    Failed {
        cmd: String,
        code: Option<i32>,
        stderr: String,
    },
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Spawn(e) => write!(f, "could not run git (is it on PATH?): {e}"),
            GitError::Failed { cmd, code, stderr } => {
                write!(f, "git {cmd} failed (code {code:?}): {stderr}")
            }
        }
    }
}

impl std::error::Error for GitError {}

impl Git {
    pub fn new(repo: impl Into<PathBuf>) -> Git {
        Git { repo: repo.into() }
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    /// Clone `url` into `dest` and return a handle to it.
    pub async fn clone(url: &str, dest: &Path) -> Result<Git, GitError> {
        let dest_s = dest.to_string_lossy().to_string();
        // `--` stops option parsing so a `url` like `--upload-pack=...` can't
        // smuggle a flag (argv injection hardening).
        let out = base_command()
            .args(["clone", "--", url, &dest_s])
            .output()
            .await
            .map_err(|e| GitError::Spawn(e.to_string()))?;
        if !out.status.success() {
            return Err(GitError::Failed {
                cmd: format!("clone {url}"),
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(Git::new(dest))
    }

    /// Run git in this repo, returning the raw output. Non-zero exit is NOT an
    /// error here — callers that need success use `run_checked`; `push` inspects
    /// the output itself.
    async fn run(&self, args: &[&str]) -> Result<std::process::Output, GitError> {
        base_command()
            .args(args)
            .current_dir(&self.repo)
            .output()
            .await
            .map_err(|e| GitError::Spawn(e.to_string()))
    }

    /// Run git and require success; returns stdout.
    async fn run_checked(&self, args: &[&str]) -> Result<String, GitError> {
        let out = self.run(args).await?;
        if !out.status.success() {
            return Err(GitError::Failed {
                cmd: args.join(" "),
                code: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    pub async fn add(&self, paths: &[&str]) -> Result<(), GitError> {
        let mut args = vec!["add", "--"];
        args.extend_from_slice(paths);
        self.run_checked(&args).await.map(|_| ())
    }

    pub async fn commit(&self, message: &str) -> Result<(), GitError> {
        self.run_checked(&["commit", "-m", message]).await.map(|_| ())
    }

    pub async fn current_branch(&self) -> Result<String, GitError> {
        Ok(self
            .run_checked(&["rev-parse", "--abbrev-ref", "HEAD"])
            .await?
            .trim()
            .to_string())
    }

    /// Rebase the current branch onto its remote counterpart.
    pub async fn pull_rebase(&self) -> Result<(), GitError> {
        let branch = self.current_branch().await?;
        self.run_checked(&["pull", "--rebase", "origin", &branch])
            .await
            .map(|_| ())
    }

    /// Rebase preferring our (replayed) side on conflict — last-write-wins for
    /// scribe docs (KTD9). During a rebase the replayed commits are "theirs",
    /// so `-X theirs` keeps the just-written local content.
    pub async fn pull_rebase_prefer_local(&self) -> Result<(), GitError> {
        let branch = self.current_branch().await?;
        self.run_checked(&[
            "-c",
            "core.editor=true",
            "pull",
            "--rebase",
            "-X",
            "theirs",
            "origin",
            &branch,
        ])
        .await
        .map(|_| ())
    }

    /// Check out `branch`, creating it from the current HEAD if it exists
    /// nowhere (local or remote-tracking). Leaves other branches untouched (R4).
    pub async fn ensure_branch(&self, branch: &str) -> Result<(), GitError> {
        // A leading-dash ref would be parsed as an option by `checkout` (argv
        // injection); reject it. Real branches here are always `collab/<topic>`.
        if branch.starts_with('-') {
            return Err(GitError::Failed {
                cmd: "ensure_branch".into(),
                code: None,
                stderr: format!("refusing branch name starting with '-': {branch}"),
            });
        }
        if self.run(&["checkout", branch]).await?.status.success() {
            return Ok(());
        }
        self.run_checked(&["checkout", "-b", branch]).await.map(|_| ())
    }

    /// Push current HEAD with machine-readable output (KTD8).
    pub async fn push(&self) -> Result<PushOutcome, GitError> {
        let out = self.run(&["push", "--porcelain", "origin", "HEAD"]).await?;
        Ok(classify_push(
            out.status.success(),
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        ))
    }

    /// Push, auto-resolving non-fast-forward collisions by rebasing and
    /// retrying, bounded by `max_attempts`. `Failed` returns immediately —
    /// never a rebase loop (KTD8).
    pub async fn push_with_retry(&self, max_attempts: u32) -> Result<PushOutcome, GitError> {
        self.push_retry(max_attempts, false).await
    }

    /// Like `push_with_retry`, but resolves collisions in favor of the local
    /// (just-written) content — for scribe docs where both peers may edit the
    /// same file (KTD9).
    pub async fn push_with_retry_local_wins(
        &self,
        max_attempts: u32,
    ) -> Result<PushOutcome, GitError> {
        self.push_retry(max_attempts, true).await
    }

    async fn push_retry(
        &self,
        max_attempts: u32,
        prefer_local: bool,
    ) -> Result<PushOutcome, GitError> {
        let mut attempt: u32 = 0;
        loop {
            match self.push().await? {
                PushOutcome::Pushed => return Ok(PushOutcome::Pushed),
                PushOutcome::Failed(e) => return Ok(PushOutcome::Failed(e)),
                PushOutcome::Rejected => {
                    attempt += 1;
                    if attempt >= max_attempts {
                        return Ok(PushOutcome::Rejected);
                    }
                    if prefer_local {
                        self.pull_rebase_prefer_local().await?;
                    } else {
                        self.pull_rebase().await?;
                    }
                }
            }
        }
    }
}

/// Base `git` command with a forced C locale and no terminal prompts (KTD8).
fn base_command() -> Command {
    let mut c = Command::new("git");
    c.env("LC_ALL", "C")
        .env("LANG", "C")
        .env("GIT_TERMINAL_PROMPT", "0");
    c
}

/// Classify a push result from its exit status and output (KTD8).
///
/// The signal is the `--porcelain` stdout (which git keeps in English and
/// machine-stable), NOT the localized stderr. A leading `!` ref line with a
/// non-fast-forward / rejected marker means a collision worth a rebase-retry;
/// any other non-zero exit is a hard failure to surface.
fn classify_push(success: bool, stdout: &str, stderr: &str) -> PushOutcome {
    if success {
        return PushOutcome::Pushed;
    }
    let non_ff = stdout.lines().any(|l| {
        l.starts_with('!')
            && (l.contains("non-fast-forward")
                || l.contains("[rejected]")
                || l.contains("fetch first"))
    });
    if non_ff {
        PushOutcome::Rejected
    } else {
        PushOutcome::Failed(stderr.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_is_pushed() {
        assert_eq!(classify_push(true, "", ""), PushOutcome::Pushed);
    }

    #[test]
    fn porcelain_reject_detected_despite_localized_stderr() {
        // Porcelain stdout carries the machine marker; stderr is German here to
        // prove we do NOT depend on locale-fragile stderr matching (KTD8).
        let stdout = "To origin\n!\trefs/heads/collab/x:refs/heads/collab/x\t[rejected] (non-fast-forward)\nDone\n";
        let stderr = "fehler: fehlgeschlagen beim Versenden einiger Referenzen";
        assert_eq!(classify_push(false, stdout, stderr), PushOutcome::Rejected);
    }

    #[test]
    fn auth_failure_is_failed_not_rejected() {
        // No '!' ref line in stdout -> not a collision -> surface as Failed.
        let stderr = "fatal: Authentication failed for 'https://example/'";
        match classify_push(false, "", stderr) {
            PushOutcome::Failed(e) => assert!(e.contains("Authentication failed")),
            other => panic!("expected Failed, got {other:?}"),
        }
    }
}
