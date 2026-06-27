//! Thin terminal face + app loop (KTD5).
//!
//! A scrolling log plus one editable prompt line — not a TUI. `rustyline`'s
//! `ExternalPrinter` is the load-bearing piece: plain async stdout garbles the
//! user's half-typed line when the poll loop prints an incoming message, so the
//! reader runs on its own blocking thread and the async loop prints only through
//! the external printer.
//!
//! The loop logic needs a real terminal, so it is verified by manual two-terminal
//! smoke (see the plan U5). The pure pieces — line classification and rendering —
//! are unit-tested below.

use crate::message::{Kind, Message};
use crate::sync::SyncEngine;
use crate::vice::{dispatch, parse_command, ModelClient};
use std::time::Duration;

/// What a typed line means.
#[derive(Debug, PartialEq, Eq)]
pub enum Line {
    /// An `@vice ...` invocation; carries the text after the trigger (verb + args).
    Vice(String),
    /// An ordinary chat line.
    Chat(String),
}

/// Classify a typed line. Returns None for blank lines. `@vice` is only the
/// trigger when followed by whitespace or end-of-line, so `@viceroy` is chat.
pub fn classify_line(line: &str) -> Option<Line> {
    let t = line.trim();
    if t.is_empty() {
        return None;
    }
    if let Some(rest) = t.strip_prefix("@vice") {
        if rest.is_empty() || rest.starts_with(char::is_whitespace) {
            return Some(Line::Vice(rest.trim().to_string()));
        }
    }
    Some(Line::Chat(t.to_string()))
}

/// Render a message for the scrolling log: `[author/model · hh:mm] body`.
/// Human messages omit the `/model` suffix.
pub fn render(m: &Message) -> String {
    let who = match &m.model {
        Some(model) => format!("{}/{}", m.author, model),
        None => m.author.clone(),
    };
    let (hh, mm) = hhmm_utc(m.ts);
    format!("[{who} · {hh:02}:{mm:02}] {}", m.body)
}

// ponytail: UTC clock from the epoch — no chrono dep for a hh:mm stamp. Local
// time + date are a GUI-milestone concern.
fn hhmm_utc(ts: i64) -> (i64, i64) {
    let secs_of_day = ts.rem_euclid(86_400);
    (secs_of_day / 3600, (secs_of_day % 3600) / 60)
}

/// Run the chat loop: a blocking readline thread feeding an async select over
/// typed lines and a 1s poll tick. `model` (when present) services `@vice`
/// lines; when absent, `@vice` reports it is disabled.
pub async fn run<M: ModelClient>(
    engine: SyncEngine,
    model: Option<M>,
) -> Result<(), Box<dyn std::error::Error>> {
    use rustyline::{DefaultEditor, ExternalPrinter};
    use tokio::sync::mpsc;

    let mut rl = DefaultEditor::new()?;
    let mut printer = rl.create_external_printer()?;

    // Startup backfill: surface history already in the room (R5/U7).
    match engine.poll().await {
        Ok(news) => {
            for m in &news {
                let _ = printer.print(format!("{}\n", render(m)));
            }
            let _ = printer.print(format!("— caught up ({} message(s)) —\n", news.len()));
        }
        Err(e) => {
            let _ = printer.print(format!("startup poll error: {e}\n"));
        }
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Option<String>>();
    // Blocking reader thread: rustyline owns the prompt line; the printer (made
    // above, before the move) writes above it without garbling input (KTD5).
    std::thread::spawn(move || loop {
        match rl.readline("> ") {
            Ok(line) => {
                let _ = rl.add_history_entry(line.as_str());
                if tx.send(Some(line)).is_err() {
                    break;
                }
            }
            Err(_) => {
                let _ = tx.send(None); // EOF / Ctrl-D / Ctrl-C
                break;
            }
        }
    });

    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            maybe = rx.recv() => match maybe {
                Some(Some(line)) => match classify_line(&line) {
                    Some(Line::Chat(body)) => match engine.send(&body, Kind::Human, None).await {
                        Ok(m) => { let _ = printer.print(format!("{}\n", render(&m))); }
                        Err(e) => { let _ = printer.print(format!("send failed: {e}\n")); }
                    },
                    Some(Line::Vice(text)) => match &model {
                        // Echo the AI reply locally; the invoker's own messages never
                        // re-surface via poll (dedup), so this is the only place they see it.
                        Some(m) => match dispatch(parse_command(&text), m, &engine).await {
                            Ok(msg) => { let _ = printer.print(format!("{}\n", render(&msg))); }
                            Err(e) => { let _ = printer.print(format!("@vice error: {e}\n")); }
                        },
                        None => { let _ = printer.print("@vice disabled — add .vice.toml to enable\n".to_string()); }
                    },
                    None => {}
                },
                Some(None) | None => break, // reader closed -> quit
            },
            _ = tick.tick() => match engine.poll().await {
                // Own messages never re-surface (dedup), so no self-echo guard needed.
                Ok(news) => for m in news { let _ = printer.print(format!("{}\n", render(&m))); }
                Err(e) => { let _ = printer.print(format!("poll error: {e}\n")); }
            },
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    #[test]
    fn classify_routes_vice_and_chat() {
        assert_eq!(
            classify_line("@vice what did we decide"),
            Some(Line::Vice("what did we decide".into()))
        );
        assert_eq!(classify_line("@vice"), Some(Line::Vice("".into())));
        assert_eq!(
            classify_line("@vice write notes x"),
            Some(Line::Vice("write notes x".into()))
        );
        // Not the trigger: prefix match without a boundary.
        assert_eq!(
            classify_line("@viceroy rules"),
            Some(Line::Chat("@viceroy rules".into()))
        );
        assert_eq!(classify_line("hello there"), Some(Line::Chat("hello there".into())));
        assert_eq!(classify_line("   "), None);
    }

    #[test]
    fn render_human_and_ai() {
        let h = Message::new("i", "Alice", Kind::Human, None, 3661, "hi");
        assert_eq!(render(&h), "[Alice · 01:01] hi");
        let a = Message::new("j", "Bob", Kind::Ai, Some("deepseek".into()), 0, "noted");
        assert_eq!(render(&a), "[Bob/deepseek · 00:00] noted");
    }
}
