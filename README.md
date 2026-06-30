# agent-collab

A lightweight, local-first, model-agnostic planning room for **two developers**.
No server — **git is the transport**. Two humans chat; an AI scribe (`@vice`)
writes decisions down, recalls earlier points, and looks things up. Best during
the architecture/planning phase.

This is **v1**: an all-Rust CLI with a thin text face. A Tauri GUI and
multi-provider polish are follow-on milestones.

## How it works

- The shared repo's `collab/<topic>` branch is the room. `main` is left clean.
- Each peer appends only to its own `chat/<author-hash>.jsonl` (conflict-free),
  pushes on send, and polls (~1s) for the other peer's pushes.
- A **gitignored** local SQLite cache (FTS5) is rebuilt from the JSONL files and
  powers full-text recall.
- `@vice` runs on **your** machine with **your** own API key (read from a local
  env var, never stored). The AI message is tagged with the actual model used.

## Build

Requires the Rust toolchain **and a C compiler** (SQLite is compiled in via
`rusqlite`'s `bundled` feature). `git` must be on `PATH` at runtime.

```sh
cargo build --release
```

## Configure `@vice` (optional)

Copy `.vice.toml.example` to `.vice.toml` in the room repo (gitignored), or place
it at the global config path. Set `provider`, `model`, and `api_key_env` (the
*name* of the env var holding your key). Without it, chat works and `@vice` is
disabled.

## Use

```sh
# First time: clone a room and start chatting
agent-collab join <repo-url> <topic>

# In an existing clone
agent-collab start <topic>
```

In the prompt:

- type anything to chat
- `@vice <question>` — read-only answer grounded in the chat history
- `@vice write notes <instruction>` — (over)write the scribe doc `docs/notes.md`
- `@vice write <path> <instruction>` — (over)write a doc under `docs/`
