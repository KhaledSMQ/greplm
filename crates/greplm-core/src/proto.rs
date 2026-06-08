//! Wire protocol shared by the daemon and its clients.
//!
//! Messages are newline-delimited JSON over a Unix domain socket. Each request
//! is one JSON object on a line; each response is one JSON object on a line.

use serde::{Deserialize, Serialize};

use crate::search::{SearchQuery, SymbolQuery};

/// Default socket path relative to a project's `.greplm` directory.
pub const SOCKET_NAME: &str = "greplmd.sock";

/// A request from a client to the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
    Summary,
    Reindex {
        force: bool,
    },
    Search(SearchQuery),
    Symbols(SymbolQuery),
    Refs {
        name: String,
        limit: usize,
        offset: usize,
    },
    /// Resolved references (definitions + call sites + imports) from the
    /// structural reference index.
    RefsResolved {
        name: String,
        limit: usize,
        offset: usize,
    },
    /// Call sites that target a symbol (who calls it).
    Callers {
        name: String,
        limit: usize,
        offset: usize,
    },
    /// Call sites inside a symbol's body (what it calls).
    Callees {
        name: String,
        limit: usize,
        offset: usize,
    },
    /// Symbols transitively affected by changing a symbol (reverse call graph).
    BlastRadius {
        name: String,
        depth: u32,
        limit: usize,
    },
    /// Typed go-to-definition for the identifier at a source position.
    Definition {
        file: String,
        line: u32,
        col: u32,
    },
    /// Resolved references for the identifier at a source position.
    ReferencesAt {
        file: String,
        line: u32,
        col: u32,
    },
    /// Structural (AST) search by tree-sitter query or meta-variable pattern.
    Structural {
        pattern: String,
        lang: String,
        limit: usize,
        offset: usize,
    },
    /// Build a token-budgeted context pack for a task.
    ContextPack {
        task: String,
        budget: u64,
    },
    /// Git blame for a single line.
    Blame {
        file: String,
        line: u32,
    },
    /// Commit history of a symbol's definition.
    History {
        name: String,
        limit: usize,
    },
    /// Files (with symbols) changed since a revision.
    ChangedSince {
        rev: String,
    },
    Outline {
        file: String,
    },
    Snippet {
        file: String,
        start: u32,
        end: u32,
        context: u32,
    },
}

/// A response from the daemon to a client.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(value: serde_json::Value) -> Self {
        Response {
            ok: true,
            result: Some(value),
            error: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Response {
            ok: false,
            result: None,
            error: Some(message.into()),
        }
    }
}
