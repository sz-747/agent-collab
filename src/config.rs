//! Provider/model/key config (R8) + git-derived author identity (R6).
//!
//! Load order: per-repo `.vice.toml` -> global config -> error (R8).
//! The api key is never stored: config holds the *name* of an env var, and the
//! key is read from that env var at call time (R9).

use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Provider/model/key-source, deserialized from `.vice.toml`.
///
/// Unknown extra keys are ignored (serde default), so adding fields in a newer
/// version won't break an older config and vice versa.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Name of the env var holding the api key — not the key itself.
    pub api_key_env: String,
}

#[derive(Debug)]
pub enum ConfigError {
    /// Neither per-repo nor global config found.
    NotFound,
    /// File present but failed to parse.
    Parse(String),
    Io(String),
    /// `api_key_env` names an env var that isn't set (raised at resolve time).
    MissingKeyEnv(String),
    /// git identity could not be read.
    Identity(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::NotFound => write!(
                f,
                "no config found: create .vice.toml in the repo (see .vice.toml.example) or a global config"
            ),
            ConfigError::Parse(e) => write!(f, "config parse error: {e}"),
            ConfigError::Io(e) => write!(f, "config io error: {e}"),
            ConfigError::MissingKeyEnv(var) => {
                write!(f, "api key env var `{var}` is not set")
            }
            ConfigError::Identity(e) => write!(f, "git identity error: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    /// Load using the default paths: per-repo `./.vice.toml`, then the global
    /// config (`<config-dir>/vice/config.toml`).
    pub fn load() -> Result<Config, ConfigError> {
        Config::load_from(Path::new(".vice.toml"), global_config_path().as_deref())
    }

    /// Load using `<dir>/.vice.toml` as the per-repo config, then the global
    /// fallback. Used when the room repo is not the process's cwd (e.g. `join`).
    pub fn load_in_dir(dir: &Path) -> Result<Config, ConfigError> {
        Config::load_from(&dir.join(".vice.toml"), global_config_path().as_deref())
    }

    /// Load with explicit paths — pure and testable. `global` is optional so the
    /// "no global available" case is representable.
    pub fn load_from(per_repo: &Path, global: Option<&Path>) -> Result<Config, ConfigError> {
        if per_repo.exists() {
            return parse_file(per_repo);
        }
        if let Some(g) = global {
            if g.exists() {
                return parse_file(g);
            }
        }
        Err(ConfigError::NotFound)
    }

    /// Read the api key from the env var named by `api_key_env`, at call time.
    /// Never persisted (R9).
    pub fn resolve_api_key(&self) -> Result<String, ConfigError> {
        std::env::var(&self.api_key_env)
            .map_err(|_| ConfigError::MissingKeyEnv(self.api_key_env.clone()))
    }
}

fn parse_file(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
    toml::from_str(&text).map_err(|e| ConfigError::Parse(e.to_string()))
}

/// `<config-dir>/vice/config.toml`, or None if the platform has no config dir.
fn global_config_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "vice")
        .map(|d| d.config_dir().join("config.toml"))
}

/// Git-derived author identity (R6). No separate login.
#[derive(Debug, Clone)]
pub struct Identity {
    pub name: String,
    pub email: String,
    /// Stable, filesystem-safe handle for this author's JSONL file (KTD6).
    pub author_hash: String,
}

/// Read `user.name` / `user.email` from git (in the cwd) and derive a stable
/// author hash.
pub fn git_identity() -> Result<Identity, ConfigError> {
    git_identity_in(Path::new("."))
}

/// Like `git_identity`, but reads git config from `dir` (the room repo).
pub fn git_identity_in(dir: &Path) -> Result<Identity, ConfigError> {
    let name = git_config(dir, "user.name")?;
    let email = git_config(dir, "user.email")?;
    let author_hash = author_hash(&email);
    Ok(Identity {
        name,
        email,
        author_hash,
    })
}

fn git_config(dir: &Path, key: &str) -> Result<String, ConfigError> {
    let out = std::process::Command::new("git")
        .args(["config", "--get", key])
        .current_dir(dir)
        .output()
        .map_err(|e| ConfigError::Identity(format!("git not runnable: {e}")))?;
    if !out.status.success() {
        return Err(ConfigError::Identity(format!(
            "`git config --get {key}` failed — is git identity set?"
        )));
    }
    let val = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if val.is_empty() {
        return Err(ConfigError::Identity(format!("git {key} is empty")));
    }
    Ok(val)
}

/// FNV-1a 64-bit hex of the email. Deterministic forever (no hashmap-seed
/// instability), filesystem-safe, short. Stable for the same email across runs.
pub fn author_hash(email: &str) -> String {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x00000100000001b3;
    let mut h = OFFSET;
    for b in email.trim().to_ascii_lowercase().bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(PRIME);
    }
    format!("{h:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    const GOOD: &str = r#"
        provider = "anthropic"
        model = "claude-opus-4-8"
        api_key_env = "ANTHROPIC_API_KEY"
    "#;

    #[test]
    fn uses_per_repo_when_present() {
        let d = tempfile::tempdir().unwrap();
        let per = write(d.path(), "repo.toml", GOOD);
        let glob = write(d.path(), "global.toml", &GOOD.replace("anthropic", "openai"));
        let c = Config::load_from(&per, Some(&glob)).unwrap();
        assert_eq!(c.provider, "anthropic"); // per-repo wins
    }

    #[test]
    fn falls_back_to_global_when_per_repo_absent() {
        let d = tempfile::tempdir().unwrap();
        let glob = write(d.path(), "global.toml", &GOOD.replace("anthropic", "openai"));
        let missing = d.path().join("nope.toml");
        let c = Config::load_from(&missing, Some(&glob)).unwrap();
        assert_eq!(c.provider, "openai");
    }

    #[test]
    fn errors_when_both_absent() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a.toml");
        let b = d.path().join("b.toml");
        assert!(matches!(
            Config::load_from(&a, Some(&b)),
            Err(ConfigError::NotFound)
        ));
    }

    #[test]
    fn unknown_keys_do_not_break_parsing() {
        let d = tempfile::tempdir().unwrap();
        let body = format!("{GOOD}\nfuture_feature = true\nnested = {{ x = 1 }}\n");
        let per = write(d.path(), "repo.toml", &body);
        assert!(Config::load_from(&per, None).is_ok());
    }

    #[test]
    fn missing_env_var_errors_at_resolve_time() {
        let d = tempfile::tempdir().unwrap();
        let body = GOOD.replace("ANTHROPIC_API_KEY", "DEFINITELY_NOT_SET_VICE_TEST_VAR");
        let per = write(d.path(), "repo.toml", &body);
        let c = Config::load_from(&per, None).unwrap(); // load succeeds...
        assert!(matches!(
            c.resolve_api_key(), // ...resolve fails
            Err(ConfigError::MissingKeyEnv(_))
        ));
    }

    #[test]
    fn author_hash_is_stable_and_case_insensitive() {
        let a = author_hash("dev@example.com");
        let b = author_hash("dev@example.com");
        let c = author_hash("DEV@example.com  ");
        assert_eq!(a, b);
        assert_eq!(a, c);
        assert_eq!(a.len(), 16);
        assert_ne!(a, author_hash("other@example.com"));
    }
}
