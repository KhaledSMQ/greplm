//! greplm MCP server (stdio transport).
//!
//! Exposes the greplm trigram code index to LLM agents over the Model Context
//! Protocol. All logging goes to stderr; stdout is reserved for the protocol.

use std::collections::BTreeSet;
use std::path::PathBuf;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo,
};
use rmcp::{tool, tool_handler, tool_router, ErrorData, ServerHandler, ServiceExt};
use schemars::JsonSchema;
use serde::Deserialize;

use greplm_core::client::Client;
use greplm_core::proto::{global_socket_path, Request};
use greplm_core::search::{SearchQuery, SymbolQuery};
use greplm_core::Greplm;

#[derive(Debug, Deserialize, JsonSchema, Default)]
struct IndexArgs {
    /// Project root to index. Defaults to the server's working directory.
    #[serde(default)]
    root: Option<String>,
    /// Rebuild the whole index from scratch.
    #[serde(default)]
    force: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchArgs {
    /// The query string (literal by default).
    query: String,
    /// Treat the query as a regular expression.
    #[serde(default)]
    regex: bool,
    /// Case-insensitive matching.
    #[serde(default)]
    ignore_case: bool,
    /// Match whole identifiers only (word boundaries).
    #[serde(default)]
    whole_word: bool,
    /// Restrict results to a language id (e.g. "rust", "python", "swift").
    #[serde(default)]
    lang: Option<String>,
    /// Restrict results to paths containing this substring.
    #[serde(default)]
    path: Option<String>,
    /// Maximum number of results (default 50).
    #[serde(default)]
    limit: Option<usize>,
    /// Skip the first N results (pagination).
    #[serde(default)]
    offset: Option<usize>,
    /// Return EVERY match (grep parity): no ranking, no limit, no per-file cap.
    /// Use when completeness matters more than relevance; `limit`/`offset` are
    /// ignored.
    #[serde(default)]
    exhaustive: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RefsArgs {
    /// Identifier to find references for.
    name: String,
    /// Maximum number of results (default 100).
    #[serde(default)]
    limit: Option<usize>,
    /// Skip the first N results (pagination).
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct CallArgs {
    /// Symbol name (function/method) to analyze.
    name: String,
    /// Maximum number of results (default 100).
    #[serde(default)]
    limit: Option<usize>,
    /// Skip the first N results (pagination).
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ImpactArgs {
    /// Symbol name to analyze.
    name: String,
    /// Maximum number of caller hops to follow (default 3).
    #[serde(default)]
    depth: Option<u32>,
    /// Maximum number of affected symbols to report (default 200).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct BlameArgs {
    /// File path relative to the project root.
    file: String,
    /// Line number (1-based).
    line: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HistoryArgs {
    /// Symbol name to show history for.
    name: String,
    /// Maximum number of commits (default 20).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ChangedArgs {
    /// Git revision to diff against (e.g. main, HEAD~5, a tag).
    rev: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PackArgs {
    /// The task or question to assemble relevant code context for.
    task: String,
    /// Token budget for the assembled context (default 8000).
    #[serde(default)]
    budget: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AstArgs {
    /// A tree-sitter query S-expression (with @captures and #eq?/#match?
    /// predicates), or a friendly `$NAME` meta-variable pattern like
    /// `fn $NAME() {}`.
    pattern: String,
    /// Language id to search (required; node kinds are language-specific).
    lang: String,
    /// Maximum number of results (default 50).
    #[serde(default)]
    limit: Option<usize>,
    /// Skip the first N results (pagination).
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DefArgs {
    /// File path relative to the project root.
    file: String,
    /// Line of the identifier (1-based).
    line: u32,
    /// Column of the identifier (1-based).
    col: u32,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SnippetArgs {
    /// File path relative to the project root.
    file: String,
    /// Start line (1-based).
    start: u32,
    /// End line (1-based). Defaults to start.
    #[serde(default)]
    end: Option<u32>,
    /// Context lines around the range (default 3).
    #[serde(default)]
    context: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
struct SummaryArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
struct SymbolArgs {
    /// Symbol name or fuzzy fragment.
    name: String,
    /// Restrict to a symbol kind (function, class, struct, ...).
    #[serde(default)]
    kind: Option<String>,
    /// Require an exact name match.
    #[serde(default)]
    exact: bool,
    /// Maximum number of results (default 50).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct OutlineArgs {
    /// File path relative to the project root.
    file: String,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
struct StatusArgs {}

#[derive(Clone)]
struct GreplmServer {
    root: PathBuf,
    #[allow(dead_code)]
    tool_router: ToolRouter<GreplmServer>,
}

fn internal(e: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(e.to_string(), None)
}

/// Run a read query against the warm daemon if one is listening, else fall back
/// to an in-process (cold) query. Routing through the daemon turns a ~28ms
/// cold index-open per call into a ~0.7ms warm socket round-trip, and the
/// daemon's response is already the JSON we return — so we forward it verbatim.
///
/// The cold fallback self-heals a missing index (so a fresh checkout still
/// works without `greplm index`) before running `fallback` in-process.
fn served<F>(
    root: &std::path::Path,
    req: Request,
    fallback: F,
) -> Result<serde_json::Value, ErrorData>
where
    F: FnOnce(&Greplm) -> greplm_core::Result<serde_json::Value>,
{
    let g = Greplm::discover(root).map_err(internal)?;
    // Global multi-root daemon first (one process serves every project).
    if let Some(mut c) = Client::try_connect(&global_socket_path()) {
        if let Ok(resp) = c.request_routed(g.root(), &req) {
            if resp.ok {
                return Ok(resp.result.unwrap_or(serde_json::Value::Null));
            }
        }
    }
    // Per-project daemon.
    if let Some(mut c) = Client::try_connect(&g.socket_path()) {
        if let Ok(resp) = c.request(&req) {
            if resp.ok {
                return Ok(resp.result.unwrap_or(serde_json::Value::Null));
            }
        }
    }
    // No daemon could serve it: self-heal a missing index, then run in-process.
    let _ = g.ensure_indexed();
    fallback(&g).map_err(internal)
}

/// Record token savings for a query that returns location-style hits: the
/// unique result files (grep+read baseline) vs. the compact payload returned.
fn record_savings<T: serde::Serialize>(
    g: &Greplm,
    kind: &str,
    hits: &[T],
    path_of: impl Fn(&T) -> String,
) {
    let files: BTreeSet<String> = hits.iter().map(&path_of).collect();
    let returned = serde_json::to_string(hits).map(|s| s.len()).unwrap_or(0) as u64;
    g.record_savings(kind, &files, returned, hits.len() as u64);
}

/// Serialize a tool result for the agent. Output is compact (no pretty-print
/// indentation/newlines): the consumer is an LLM, not a human, so every byte of
/// whitespace is wasted context. This also keeps the wire payload in lockstep
/// with the savings accounting, which measures the compact `to_string` length.
fn ok_json<T: serde::Serialize>(value: &T) -> Result<CallToolResult, ErrorData> {
    let text = serde_json::to_string(value).map_err(internal)?;
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

#[tool_router]
impl GreplmServer {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            tool_router: Self::tool_router(),
        }
    }

    /// Resolve the project root for an index request. A caller-supplied `root`
    /// is only honored when it stays within the server's configured root, so an
    /// agent can't drive greplm to index arbitrary directories on the host.
    fn resolve(&self, root: &Option<String>) -> Result<PathBuf, ErrorData> {
        let requested = match root {
            None => return Ok(self.root.clone()),
            Some(r) => PathBuf::from(r),
        };
        let base = self
            .root
            .canonicalize()
            .unwrap_or_else(|_| self.root.clone());
        let target = requested
            .canonicalize()
            .unwrap_or_else(|_| requested.clone());
        if target.starts_with(&base) {
            Ok(requested)
        } else {
            Err(ErrorData::invalid_params(
                format!(
                    "root {} is outside the server root {}",
                    requested.display(),
                    self.root.display()
                ),
                None,
            ))
        }
    }

    #[tool(
        description = "Build or refresh the greplm index for a project. Run this once before \
                          searching, or after large changes. Incremental by default."
    )]
    async fn index_project(
        &self,
        Parameters(args): Parameters<IndexArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.resolve(&args.root)?;
        let force = args.force;
        let stats = tokio::task::spawn_blocking(move || -> greplm_core::Result<_> {
            let g = Greplm::open(&root)?;
            g.index(force)
        })
        .await
        .map_err(internal)?
        .map_err(internal)?;
        ok_json(&serde_json::json!({
            "files_indexed": stats.files_indexed,
            "files_skipped": stats.files_skipped,
            "skipped_by_reason": stats.skipped_by_reason,
            "files_removed": stats.files_removed,
            "symbols": stats.symbols,
            "segments": stats.segments,
        }))
    }

    #[tool(
        description = "Search file contents using the trigram index. Fast exact, substring, \
                          and regex search across the codebase. Returns ranked matches with \
                          path, line, column, and the matching line text."
    )]
    async fn search_code(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let query = SearchQuery {
            pattern: args.query,
            regex: args.regex,
            case_insensitive: args.ignore_case,
            whole_word: args.whole_word,
            lang: args.lang,
            path: args.path,
            limit: args.limit.unwrap_or(50),
            offset: args.offset.unwrap_or(0),
            max_per_file: 20,
            exhaustive: args.exhaustive,
        };
        let v = tokio::task::spawn_blocking(move || {
            served(&root, Request::Search(query.clone()), |g| {
                // Falls back to an index-free walk if the index is unavailable.
                let hits = g.search_or_grep(&query)?;
                record_savings(g, "search", &hits, |h| h.path.clone());
                Ok(serde_json::to_value(hits)?)
            })
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Find symbol definitions (functions, classes, structs, etc.) by name. \
                          Supports exact, prefix, substring, and fuzzy matching."
    )]
    async fn find_symbol(
        &self,
        Parameters(args): Parameters<SymbolArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let query = SymbolQuery {
            name: args.name,
            kind: args.kind,
            exact: args.exact,
            limit: args.limit.unwrap_or(50),
            offset: 0,
        };
        let v = tokio::task::spawn_blocking(move || {
            served(&root, Request::Symbols(query.clone()), |g| {
                let hits = g.searcher()?.symbols(&query)?;
                record_savings(g, "symbols", &hits, |h| h.path.clone());
                Ok(serde_json::to_value(hits)?)
            })
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Get the symbol outline (definitions in order) of a single file, given \
                          its path relative to the project root."
    )]
    async fn get_file_outline(
        &self,
        Parameters(args): Parameters<OutlineArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let file = args.file;
        let v = tokio::task::spawn_blocking(move || {
            served(&root, Request::Outline { file: file.clone() }, |g| {
                Ok(serde_json::to_value(g.searcher()?.outline(&file)?)?)
            })
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Find references to an identifier: whole-word occurrences across the \
                          codebase (definitions rank first). Good for 'who uses X'."
    )]
    async fn find_references(
        &self,
        Parameters(args): Parameters<RefsArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let name = args.name;
        let limit = args.limit.unwrap_or(100);
        let offset = args.offset.unwrap_or(0);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::Refs {
                    name: name.clone(),
                    limit,
                    offset,
                },
                |g| {
                    let hits = g.searcher()?.references(&name, limit, offset)?;
                    record_savings(g, "refs", &hits, |h| h.path.clone());
                    Ok(serde_json::to_value(hits)?)
                },
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Resolved references for a symbol from the structural index: its \
                          definitions, call sites, and imports (not text matching). Each result \
                          carries the enclosing symbol. Prefer this over find_references for code \
                          intelligence."
    )]
    async fn resolved_references(
        &self,
        Parameters(args): Parameters<CallArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let name = args.name;
        let limit = args.limit.unwrap_or(100);
        let offset = args.offset.unwrap_or(0);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::RefsResolved {
                    name: name.clone(),
                    limit,
                    offset,
                },
                |g| {
                    let hits = g.searcher()?.references_resolved(&name, limit, offset);
                    record_savings(g, "xref", &hits, |h| h.path.clone());
                    Ok(serde_json::to_value(hits)?)
                },
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Find who calls a function/method: every call site that targets the \
                          named symbol, attributed to its enclosing caller symbol. Use to trace \
                          where behavior originates."
    )]
    async fn find_callers(
        &self,
        Parameters(args): Parameters<CallArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let name = args.name;
        let limit = args.limit.unwrap_or(100);
        let offset = args.offset.unwrap_or(0);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::Callers {
                    name: name.clone(),
                    limit,
                    offset,
                },
                |g| {
                    let hits = g.searcher()?.callers(&name, limit, offset);
                    record_savings(g, "callers", &hits, |h| h.path.clone());
                    Ok(serde_json::to_value(hits)?)
                },
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Find what a function/method calls: every call site inside the named \
                          symbol's body. Use to understand a function's outgoing dependencies."
    )]
    async fn find_callees(
        &self,
        Parameters(args): Parameters<CallArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let name = args.name;
        let limit = args.limit.unwrap_or(100);
        let offset = args.offset.unwrap_or(0);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::Callees {
                    name: name.clone(),
                    limit,
                    offset,
                },
                |g| {
                    let hits = g.searcher()?.callees(&name, limit, offset);
                    record_savings(g, "callees", &hits, |h| h.path.clone());
                    Ok(serde_json::to_value(hits)?)
                },
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Blast radius: the symbols transitively affected if the named symbol \
                          changes, found by walking the reverse call graph up to `depth` hops. \
                          Use before editing to gauge impact. Resolution is by name, so treat \
                          results as a guide."
    )]
    async fn impact_of(
        &self,
        Parameters(args): Parameters<ImpactArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let name = args.name;
        let depth = args.depth.unwrap_or(3);
        let limit = args.limit.unwrap_or(200);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::BlastRadius {
                    name: name.clone(),
                    depth,
                    limit,
                },
                |g| {
                    let nodes = g.searcher()?.blast_radius(&name, depth, limit);
                    record_savings(g, "impact", &nodes, |n| n.path.clone());
                    Ok(serde_json::to_value(nodes)?)
                },
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Git blame for a single line: the commit, author, and summary that last \
                          changed it. Use to learn why a line exists."
    )]
    async fn git_blame(
        &self,
        Parameters(args): Parameters<BlameArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let (file, line) = (args.file, args.line);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::Blame {
                    file: file.clone(),
                    line,
                },
                |g| Ok(serde_json::to_value(g.searcher()?.blame(&file, line)?)?),
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Show the commit history of a symbol: resolve it to its definition and \
                          list the commits that touched its line range, newest first."
    )]
    async fn symbol_history(
        &self,
        Parameters(args): Parameters<HistoryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let name = args.name;
        let limit = args.limit.unwrap_or(20);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::History {
                    name: name.clone(),
                    limit,
                },
                |g| {
                    Ok(serde_json::to_value(
                        g.searcher()?.symbol_history(&name, limit)?,
                    )?)
                },
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "List files changed since a git revision (e.g. main, HEAD~5), each \
                          annotated with the symbols it defines. Use to scope a review or \
                          understand what a branch touched."
    )]
    async fn changed_since(
        &self,
        Parameters(args): Parameters<ChangedArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let rev = args.rev;
        let v = tokio::task::spawn_blocking(move || {
            served(&root, Request::ChangedSince { rev: rev.clone() }, |g| {
                Ok(serde_json::to_value(g.searcher()?.changed_since(&rev)?)?)
            })
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Build a token-budgeted context pack for a task: the most relevant \
                          symbols (with signatures and code snippets) plus their dependency \
                          neighborhood, ranked by lexical relevance and call-graph centrality. \
                          Call this FIRST when starting a task to load exactly the code you need \
                          instead of grepping and reading whole files."
    )]
    async fn build_context(
        &self,
        Parameters(args): Parameters<PackArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let task = args.task;
        let budget = args.budget.unwrap_or(8000);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::ContextPack {
                    task: task.clone(),
                    budget,
                },
                |g| {
                    let pack = g.searcher()?.context_pack(&task, budget);
                    let files: std::collections::BTreeSet<String> =
                        pack.items.iter().map(|i| i.path.clone()).collect();
                    let returned =
                        serde_json::to_string(&pack).map(|s| s.len()).unwrap_or(0) as u64;
                    g.record_savings("pack", &files, returned, pack.items.len() as u64);
                    Ok(serde_json::to_value(pack)?)
                },
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Structural (AST) search: match a tree-sitter query S-expression \
                          (with @captures and #eq?/#match? predicates) or a friendly `$NAME` \
                          meta-variable pattern (e.g. `fn $NAME() {}`) across one language. More \
                          precise than regex for code shapes. `lang` is required."
    )]
    async fn structural_search(
        &self,
        Parameters(args): Parameters<AstArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let pattern = args.pattern;
        let lang = args.lang;
        let limit = args.limit.unwrap_or(50);
        let offset = args.offset.unwrap_or(0);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::Structural {
                    pattern: pattern.clone(),
                    lang: lang.clone(),
                    limit,
                    offset,
                },
                |g| {
                    let hits = g
                        .searcher()?
                        .structural_search(&pattern, &lang, limit, offset)?;
                    record_savings(g, "ast", &hits, |h| h.path.clone());
                    Ok(serde_json::to_value(hits)?)
                },
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Typed go-to-definition: resolve the identifier at file:line:col to its \
                          most likely definition(s), using scope, usage context, and imports. \
                          The unambiguous target is flagged `resolved`; otherwise ranked \
                          candidates are returned. Falls back to text matches when unindexed."
    )]
    async fn goto_definition(
        &self,
        Parameters(args): Parameters<DefArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let (file, line, col) = (args.file, args.line, args.col);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::Definition {
                    file: file.clone(),
                    line,
                    col,
                },
                |g| {
                    let hits = g.searcher()?.definition(&file, line, col)?;
                    record_savings(g, "def", &hits, |h| h.path.clone());
                    Ok(serde_json::to_value(hits)?)
                },
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Resolved references for the identifier at file:line:col: its definitions, \
                          call sites, and imports across the repo, from the structural reference \
                          index. Use after locating an identifier to see everywhere it is used."
    )]
    async fn references_at(
        &self,
        Parameters(args): Parameters<DefArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let (file, line, col) = (args.file, args.line, args.col);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::ReferencesAt {
                    file: file.clone(),
                    line,
                    col,
                },
                |g| {
                    let hits = g.searcher()?.references_of(&file, line, col)?;
                    record_savings(g, "refs-at", &hits, |h| h.path.clone());
                    Ok(serde_json::to_value(hits)?)
                },
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Read a slice of a file with surrounding context lines. Use the line \
                          numbers returned by search_code/find_symbol to fetch exact code."
    )]
    async fn read_snippet(
        &self,
        Parameters(args): Parameters<SnippetArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let file = args.file;
        let start = args.start;
        let end = args.end.unwrap_or(start);
        let context = args.context.unwrap_or(3);
        let v = tokio::task::spawn_blocking(move || {
            served(
                &root,
                Request::Snippet {
                    file: file.clone(),
                    start,
                    end,
                    context,
                },
                |g| {
                    let snip = g.searcher()?.read_snippet(&file, start, end, context)?;
                    let returned: u64 = snip.text.len() as u64;
                    let files: BTreeSet<String> = [snip.path.clone()].into_iter().collect();
                    g.record_savings("snippet", &files, returned, 1);
                    Ok(serde_json::to_value(snip)?)
                },
            )
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Summarize the indexed repository: file/symbol counts, language \
                          breakdown, and top-level directories."
    )]
    async fn repo_summary(
        &self,
        Parameters(_args): Parameters<SummaryArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let v = tokio::task::spawn_blocking(move || {
            served(&root, Request::Summary, |g| {
                Ok(serde_json::to_value(g.searcher()?.summary())?)
            })
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }

    #[tool(
        description = "Report greplm index status: whether the project is indexed, segment \
                          count, document and symbol counts, and last index time."
    )]
    async fn index_status(
        &self,
        Parameters(_args): Parameters<StatusArgs>,
    ) -> Result<CallToolResult, ErrorData> {
        let root = self.root.clone();
        let v = tokio::task::spawn_blocking(move || -> Result<serde_json::Value, ErrorData> {
            // Report the index as-is; never build/refresh just to read status.
            let g = Greplm::discover(&root).map_err(internal)?;
            if let Some(mut c) = Client::try_connect(&g.socket_path()) {
                if let Ok(resp) = c.request(&Request::Status) {
                    if resp.ok {
                        return Ok(resp.result.unwrap_or(serde_json::Value::Null));
                    }
                }
            }
            serde_json::to_value(g.status().map_err(internal)?).map_err(internal)
        })
        .await
        .map_err(internal)??;
        ok_json(&v)
    }
}

#[tool_handler]
impl ServerHandler for GreplmServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.protocol_version = ProtocolVersion::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info.server_info = Implementation::from_build_env();
        info.instructions = Some(
            "greplm is an extreme-performance code index with code intelligence. Call \
             `index_project` first. To start a task, call `build_context` to load exactly the \
             relevant code on a token budget instead of reading whole files. Use `search_code` \
             for content/regex search, `find_symbol`/`goto_definition` for definitions, \
             `find_callers`/`find_callees`/`impact_of` to navigate the call graph and gauge the \
             blast radius before editing, `structural_search` for AST patterns, and \
             `git_blame`/`symbol_history`/`changed_since` for history. Prefer these over raw grep."
                .to_string(),
        );
        info
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("GREPLM_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Optional first positional arg sets the project root; default to cwd.
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);

    tracing::info!("greplm-mcp serving root {}", root.display());

    let service = GreplmServer::new(root)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
