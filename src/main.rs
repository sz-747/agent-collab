//! agent-collab: git-transport two-human planning room with an @vice AI scribe.
//!
//! v1 is an all-Rust thin text face. Two subcommands, hand-parsed (clap is a
//! GUI-milestone nicety):
//!   agent-collab join <repo-url> <topic>   clone a room and start chatting
//!   agent-collab start <topic>             start in an existing clone (cwd)
//!
//! The engine lives in the library crate (`src/lib.rs`).

use agent_collab::{app, session};
use std::path::Path;

#[tokio::main]
async fn main() {
    if let Err(e) = real_main().await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn real_main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("join") => {
            let (url, topic) = match (args.get(2), args.get(3)) {
                (Some(u), Some(t)) => (u, t),
                _ => return Err("usage: agent-collab join <repo-url> <topic>".into()),
            };
            let dir = session::derive_clone_dir(url);
            println!("cloning {url} -> {dir} ...");
            let s = session::join_session(url, topic, Path::new(&dir)).await?;
            run_session(s).await
        }
        Some("start") => {
            let topic = args
                .get(2)
                .ok_or("usage: agent-collab start <topic>")?;
            let s = session::start_session(Path::new("."), topic).await?;
            run_session(s).await
        }
        _ => {
            eprintln!(
                "usage:\n  agent-collab join <repo-url> <topic>\n  agent-collab start <topic>"
            );
            std::process::exit(2);
        }
    }
}

async fn run_session(s: session::Session) -> Result<(), Box<dyn std::error::Error>> {
    if s.model.is_none() {
        eprintln!(
            "note: @vice disabled — add a .vice.toml (provider/model/api_key_env) to enable it"
        );
    }
    app::run(s.engine, s.model).await
}
