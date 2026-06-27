//! The shared chat message model and its JSONL representation (KTD6).
//!
//! One `Message` per line of a per-author JSONL file. The same struct is the
//! row shape in the derived SQLite store (U2) and the unit of sync (U4).

use serde::{Deserialize, Serialize};

/// Who authored a message. Scribe docs are not messages — they live only in the
/// search index (U2 `index_doc`), so `doc` is not a `Kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Human,
    Ai,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Human => "human",
            Kind::Ai => "ai",
        }
    }

    pub fn from_db(s: &str) -> Option<Kind> {
        match s {
            "human" => Some(Kind::Human),
            "ai" => Some(Kind::Ai),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// uuid v4 — the dedup key for the derived cache (KTD6).
    pub id: String,
    /// git author hash (KTD6) of the sender.
    pub author: String,
    pub kind: Kind,
    /// Actual model id for AI messages (R10); None for human messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Unix epoch seconds.
    pub ts: i64,
    pub body: String,
    /// Space-joined `#hashtags` lifted from the body (R12); None if none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<String>,
}

impl Message {
    /// Build a message, lifting any `#hashtags` from the body into `tags` (R12).
    pub fn new(
        id: impl Into<String>,
        author: impl Into<String>,
        kind: Kind,
        model: Option<String>,
        ts: i64,
        body: impl Into<String>,
    ) -> Message {
        let body = body.into();
        let tags = parse_hashtags(&body);
        Message {
            id: id.into(),
            author: author.into(),
            kind,
            model,
            ts,
            body,
            tags,
        }
    }
}

/// Pull `#hashtags` out of a body. A hashtag is `#` followed by one or more
/// word characters (letters, digits, underscore). Returns the tags space-joined
/// without the leading `#`, or None if there are none.
pub fn parse_hashtags(body: &str) -> Option<String> {
    let mut tags: Vec<String> = Vec::new();
    let mut chars = body.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '#' {
            continue;
        }
        let mut tag = String::new();
        while let Some(&(_, nc)) = chars.peek() {
            if nc.is_alphanumeric() || nc == '_' {
                tag.push(nc);
                chars.next();
            } else {
                break;
            }
        }
        if !tag.is_empty() && !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    if tags.is_empty() {
        None
    } else {
        Some(tags.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_round_trips() {
        let m = Message::new("id1", "auth", Kind::Ai, Some("claude".into()), 42, "hi");
        let line = serde_json::to_string(&m).unwrap();
        let back: Message = serde_json::from_str(&line).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn human_message_omits_model_in_json() {
        let m = Message::new("id1", "auth", Kind::Human, None, 1, "x");
        let line = serde_json::to_string(&m).unwrap();
        assert!(!line.contains("model"));
    }

    #[test]
    fn hashtags_are_lifted() {
        assert_eq!(parse_hashtags("no tags here"), None);
        assert_eq!(parse_hashtags("ship #payment now"), Some("payment".into()));
        assert_eq!(
            parse_hashtags("#auth and #auth_v2 and #auth"),
            Some("auth auth_v2".into()) // dedup, order preserved
        );
        assert_eq!(parse_hashtags("a # b"), None); // bare # is not a tag
    }
}
