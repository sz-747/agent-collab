//! The gitignored, derived local cache (R1, KTD6): messages + scribe-doc text,
//! a single FTS5 index over both, and the two retrieval functions the AI calls
//! (`search_chat`, `get_messages` — R11).
//!
//! The source of truth is the per-author JSONL files (U4); this store is rebuilt
//! from them, so every write is idempotent on message id.

use crate::message::{Kind, Message};
use rusqlite::{params, Connection, Result as SqlResult};

pub struct Store {
    conn: Connection,
}

/// One hit from `search_chat` — a message or a scribe doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Message id, or doc path for `kind == "doc"`.
    pub ref_id: String,
    /// "human" | "ai" | "doc".
    pub kind: String,
    pub author: Option<String>,
    pub ts: Option<i64>,
    /// Source path for scribe docs (R12); None for messages.
    pub source: Option<String>,
    pub body: String,
}

/// Inclusive time window for `get_messages`, with an optional cap.
#[derive(Debug, Clone, Default)]
pub struct TimeRange {
    pub start: Option<i64>,
    pub end: Option<i64>,
    pub limit: Option<i64>,
}

impl Store {
    pub fn open(path: &std::path::Path) -> SqlResult<Store> {
        let conn = Connection::open(path)?;
        Store::init(conn)
    }

    pub fn open_in_memory() -> SqlResult<Store> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> SqlResult<Store> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                id     TEXT PRIMARY KEY,
                author TEXT NOT NULL,
                kind   TEXT NOT NULL,
                model  TEXT,
                ts     INTEGER NOT NULL,
                body   TEXT NOT NULL,
                tags   TEXT
            );
            CREATE VIRTUAL TABLE IF NOT EXISTS search USING fts5(
                body,
                kind   UNINDEXED,
                author UNINDEXED,
                ts     UNINDEXED,
                source UNINDEXED,
                ref_id UNINDEXED
            );",
        )?;
        Ok(Store { conn })
    }

    /// Insert a message if its id is new. Returns true when a row was actually
    /// inserted (false on duplicate) — the derived-cache idempotency (KTD6).
    /// The FTS row is written only on genuine insert, keeping the index in sync.
    pub fn upsert_message(&self, m: &Message) -> SqlResult<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO messages (id, author, kind, model, ts, body, tags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![m.id, m.author, m.kind.as_str(), m.model, m.ts, m.body, m.tags],
        )?;
        if n > 0 {
            // Tags are appended to the indexed text so a bare `#tag` is findable
            // even though the unicode61 tokenizer would split it the same way.
            let indexed = match &m.tags {
                Some(t) => format!("{} {}", m.body, t),
                None => m.body.clone(),
            };
            self.conn.execute(
                "INSERT INTO search (body, kind, author, ts, source, ref_id)
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5)",
                params![indexed, m.kind.as_str(), m.author, m.ts, m.id],
            )?;
        }
        Ok(n > 0)
    }

    /// Index (or re-index) a scribe doc's text so it is searchable alongside
    /// messages (R12). Update-in-place: a second call for the same path replaces
    /// the prior index entry (LWW — R14).
    pub fn index_doc(&self, path: &str, body: &str) -> SqlResult<()> {
        self.conn.execute(
            "DELETE FROM search WHERE kind = 'doc' AND ref_id = ?1",
            params![path],
        )?;
        self.conn.execute(
            "INSERT INTO search (body, kind, author, ts, source, ref_id)
             VALUES (?1, 'doc', NULL, NULL, ?2, ?2)",
            params![body, path],
        )?;
        Ok(())
    }

    /// FTS5 keyword search over messages + scribe docs, best match first (R11).
    /// The query is escaped to phrase literals (KTD7) so operator characters in
    /// user input never throw an FTS5 syntax error. Empty query -> no hits.
    pub fn search_chat(&self, query: &str) -> SqlResult<Vec<SearchHit>> {
        let match_expr = escape_fts_query(query);
        if match_expr.is_empty() {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT ref_id, kind, author, ts, source, body
             FROM search WHERE search MATCH ?1 ORDER BY rank",
        )?;
        let rows = stmt.query_map(params![match_expr], |r| {
            Ok(SearchHit {
                ref_id: r.get(0)?,
                kind: r.get(1)?,
                author: r.get(2)?,
                ts: r.get(3)?,
                source: r.get(4)?,
                body: r.get(5)?,
            })
        })?;
        rows.collect()
    }

    /// Fetch messages in a time window, oldest first, deterministic on ties.
    pub fn get_messages(&self, range: &TimeRange) -> SqlResult<Vec<Message>> {
        let limit = range.limit.unwrap_or(-1); // -1 = no limit in SQLite
        let mut stmt = self.conn.prepare(
            "SELECT id, author, kind, model, ts, body, tags FROM messages
             WHERE (?1 IS NULL OR ts >= ?1) AND (?2 IS NULL OR ts <= ?2)
             ORDER BY ts ASC, id ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![range.start, range.end, limit], |r| {
            let kind_s: String = r.get(2)?;
            Ok(Message {
                id: r.get(0)?,
                author: r.get(1)?,
                kind: Kind::from_db(&kind_s).unwrap_or(Kind::Human),
                model: r.get(3)?,
                ts: r.get(4)?,
                body: r.get(5)?,
                tags: r.get(6)?,
            })
        })?;
        rows.collect()
    }
}

/// Turn arbitrary user text into a safe FTS5 MATCH expression (KTD7).
///
/// Binding as a parameter protects the surrounding SQL, but the bound string is
/// still parsed as an FTS5 *expression* — `AND`/`OR`/`NEAR`/`*`/`"`/`:` are
/// operators. We wrap each whitespace token as a quoted phrase literal (doubling
/// embedded quotes) and join with spaces, which FTS5 reads as an implicit AND.
fn escape_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_handles_operators_and_quotes() {
        assert_eq!(escape_fts_query("hi"), "\"hi\"");
        assert_eq!(escape_fts_query("a OR b"), "\"a\" \"OR\" \"b\"");
        assert_eq!(escape_fts_query("say \"hi"), "\"say\" \"\"\"hi\"");
        assert_eq!(escape_fts_query("   "), "");
    }
}
