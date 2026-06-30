//! U2 verification. Oracle r5: the real bundled SQLite/FTS5 engine is the
//! authority for match/ranking behavior; we assert exact returned ids against a
//! frozen corpus. The escaping transform (KTD7) is r3 — reviewed independently.

use agent_collab::message::{Kind, Message};
use agent_collab::store::{Store, TimeRange};

fn msg(id: &str, ts: i64, body: &str) -> Message {
    Message::new(id, "alice", Kind::Human, None, ts, body)
}

fn seed() -> Store {
    let s = Store::open_in_memory().unwrap();
    for m in [
        msg("m1", 100, "we should add a payment gateway soon"),
        msg("m2", 200, "the login flow needs work"),
        msg("m3", 300, "refactor the payment retry logic"),
        msg("m4", 400, "lunch?"),
    ] {
        s.upsert_message(&m).unwrap();
    }
    s
}

fn ids(hits: &[agent_collab::store::SearchHit]) -> Vec<String> {
    hits.iter().map(|h| h.ref_id.clone()).collect()
}

#[test]
fn search_returns_matching_messages_ranked() {
    let s = seed();
    let hits = s.search_chat("payment").unwrap();
    let got = ids(&hits);
    assert!(got.contains(&"m1".to_string()));
    assert!(got.contains(&"m3".to_string()));
    assert!(!got.contains(&"m2".to_string()));
    assert!(!got.contains(&"m4".to_string()));
}

#[test]
fn embedded_quote_does_not_error_and_matches_literal() {
    let s = Store::open_in_memory().unwrap();
    s.upsert_message(&msg("q1", 1, "she said hi to everyone"))
        .unwrap();
    // Would throw `fts5: syntax error` without KTD7 escaping.
    let hits = s.search_chat("said \"hi").unwrap();
    assert_eq!(ids(&hits), vec!["q1"]);
}

#[test]
fn multi_word_query_is_and() {
    let s = seed();
    // "payment retry" -> only m3 has both words.
    let hits = s.search_chat("payment retry").unwrap();
    assert_eq!(ids(&hits), vec!["m3"]);
}

#[test]
fn empty_query_returns_no_hits() {
    let s = seed();
    assert!(s.search_chat("   ").unwrap().is_empty());
}

#[test]
fn get_messages_over_time_range_in_order() {
    let s = seed();
    let range = TimeRange {
        start: Some(150),
        end: Some(350),
        limit: None,
    };
    let got: Vec<String> = s
        .get_messages(&range)
        .unwrap()
        .into_iter()
        .map(|m| m.id)
        .collect();
    assert_eq!(got, vec!["m2", "m3"]); // exactly in-range, ordered by ts
}

#[test]
fn upsert_is_idempotent_on_id() {
    let s = Store::open_in_memory().unwrap();
    let m = msg("dup", 1, "only once");
    assert!(s.upsert_message(&m).unwrap()); // first insert -> true
    assert!(!s.upsert_message(&m).unwrap()); // duplicate -> false, no-op
    // And no duplicate row in the FTS index.
    assert_eq!(s.search_chat("only").unwrap().len(), 1);
    assert_eq!(s.get_messages(&TimeRange::default()).unwrap().len(), 1);
}

#[test]
fn scribe_doc_is_searchable_and_marked() {
    let s = seed();
    s.index_doc("docs/decisions.md", "decision: use postgres for storage")
        .unwrap();
    let hits = s.search_chat("postgres").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].kind, "doc");
    assert_eq!(hits[0].source.as_deref(), Some("docs/decisions.md"));
}

#[test]
fn scribe_doc_reindex_is_update_in_place() {
    let s = Store::open_in_memory().unwrap();
    s.index_doc("docs/n.md", "alpha content").unwrap();
    s.index_doc("docs/n.md", "beta content").unwrap(); // overwrite (R14 LWW)
    assert!(s.search_chat("alpha").unwrap().is_empty());
    assert_eq!(s.search_chat("beta").unwrap().len(), 1);
}

#[test]
fn hashtag_is_captured_and_searchable() {
    let s = Store::open_in_memory().unwrap();
    let m = msg("h1", 1, "lets ship the #urgent fix");
    assert_eq!(m.tags.as_deref(), Some("urgent")); // captured on construct
    s.upsert_message(&m).unwrap();
    assert_eq!(ids(&s.search_chat("urgent").unwrap()), vec!["h1"]); // searchable
}
