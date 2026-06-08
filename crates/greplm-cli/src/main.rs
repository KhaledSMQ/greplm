//! greplm command-line interface.

mod agent;

use std::collections::BTreeSet;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use greplm_core::client::Client;
use greplm_core::context::ContextPack;
use greplm_core::git::BlameLine;
use greplm_core::proto::{Request, Response};
use greplm_core::search::{
    CallSite, ChangedSymbols, DefHit, ImpactNode, RefHit, RepoSummary, SearchHit, SearchQuery,
    Snippet, StructHit, SymbolHistory, SymbolHit, SymbolQuery,
};
use greplm_core::{Greplm, Status};
use serde::de::DeserializeOwned;

/// Extreme-performance trigram code indexer for LLM agents.
#[derive(Debug, Parser)]
#[command(name = "greplm", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create the .greplm directory with a default config (no indexing).
    Init(RootArg),
    /// Build or refresh the index.
    Index(IndexArgs),
    /// Search file contents.
    Search(SearchArgs),
    /// Look up symbol definitions by name.
    Symbols(SymbolsArgs),
    /// Find references to an identifier (whole-word occurrences).
    Refs(RefsArgs),
    /// Resolved references (definitions, call sites, imports) for a symbol.
    Xref(RefsArgs),
    /// Find call sites that target a symbol (who calls it).
    Callers(CallArgs),
    /// Find call sites inside a symbol's body (what it calls).
    Callees(CallArgs),
    /// Show the blast radius: symbols affected if a symbol changes.
    Impact(ImpactArgs),
    /// Resolve the definition of the identifier at file:line:col.
    Def(DefArgs),
    /// Resolved references for the identifier at file:line:col.
    RefsAt(DefArgs),
    /// Structural (AST) search: tree-sitter query or `$NAME` meta-var pattern.
    Ast(AstArgs),
    /// Build a token-budgeted context pack of the code relevant to a task.
    Pack(PackArgs),
    /// Git blame for a single line (file:line).
    Blame(BlameArgs),
    /// Show the commit history of a symbol's definition.
    History(HistoryArgs),
    /// List files (and their symbols) changed since a git revision.
    Changed(ChangedArgs),
    /// Print the symbol outline of a single file.
    Outline(OutlineArgs),
    /// Print a file slice with surrounding context.
    Snippet(SnippetArgs),
    /// Summarize the indexed repository.
    Summary(RootArg),
    /// Show index status.
    Status(RootArg),
    /// Watch the project and re-index on changes.
    Watch(WatchArgs),
    /// Run the warm-index daemon (serves queries over a socket).
    Serve(ServeArgs),
    /// Build the optional semantic (vector) index.
    #[cfg(feature = "semantic")]
    SemanticIndex(SemanticIndexArgs),
    /// Search the semantic index by meaning.
    #[cfg(feature = "semantic")]
    SemanticSearch(SemanticSearchArgs),
    /// Show estimated tokens saved vs. grep+read across your queries.
    Savings(SavingsArgs),
    /// Install the bundled greplm agent definition for a coding tool.
    #[command(subcommand)]
    Agent(AgentCommand),
    /// Delete the .greplm index directory.
    Clean(RootArg),
}

#[derive(Debug, Subcommand)]
enum AgentCommand {
    /// Install the agent file for a tool (auto-detects when no tool is given).
    Add(AgentAddArgs),
    /// List supported tools and their destination paths.
    List(AgentListArgs),
}

#[derive(Debug, Args)]
struct AgentAddArgs {
    /// Tool to install for (e.g. cursor, claude, copilot). Omit to auto-detect.
    tool: Option<String>,
    /// Project root (defaults to the current directory).
    #[arg(long, short = 'C')]
    root: Option<PathBuf>,
    /// Install for every project (user home) instead of this project.
    #[arg(long)]
    global: bool,
    /// Overwrite an existing agent file.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct AgentListArgs {
    /// Project root (defaults to the current directory).
    #[arg(long, short = 'C')]
    root: Option<PathBuf>,
    /// Show destinations under the user home (global scope).
    #[arg(long)]
    global: bool,
}

#[cfg(feature = "semantic")]
#[derive(Debug, Args)]
struct SemanticIndexArgs {
    #[command(flatten)]
    root: RootArg,
    /// Path to a Model2Vec model directory (containing `tokenizer.json` and
    /// `model.safetensors`). Defaults to $GREPLM_SEMANTIC_MODEL, then the
    /// built-in offline hash embedder.
    #[arg(long)]
    model: Option<PathBuf>,
}

#[cfg(feature = "semantic")]
#[derive(Debug, Args)]
struct SemanticSearchArgs {
    /// Natural-language or code query.
    query: String,
    #[command(flatten)]
    root: RootArg,
    /// Number of results.
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Path to a Model2Vec model directory (must match the one used to index).
    #[arg(long)]
    model: Option<PathBuf>,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RootArg {
    /// Project root (defaults to the current directory).
    #[arg(long, short = 'C')]
    root: Option<PathBuf>,
    /// Bypass the greplm daemon even if one is running.
    #[arg(long)]
    no_daemon: bool,
}

#[derive(Debug, Args)]
struct IndexArgs {
    #[command(flatten)]
    root: RootArg,
    /// Rebuild the entire index from scratch.
    #[arg(long)]
    force: bool,
    /// Also index binary (NUL-containing) files, like `grep -a`.
    #[arg(long)]
    index_binary: bool,
    /// Also index empty (zero-byte) files.
    #[arg(long)]
    index_empty: bool,
    /// Skip files larger than this many bytes (0 = no limit). Overrides config.
    #[arg(long)]
    max_file_size: Option<u64>,
    /// Print a sample of skipped files and why they were skipped.
    #[arg(long)]
    explain_skips: bool,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SearchArgs {
    /// The query (literal by default, or a regex with --regex).
    query: String,
    #[command(flatten)]
    root: RootArg,
    /// Treat the query as a regular expression.
    #[arg(long, short = 'e')]
    regex: bool,
    /// Case-insensitive matching.
    #[arg(long, short = 'i')]
    ignore_case: bool,
    /// Match whole identifiers only (word boundaries).
    #[arg(long, short = 'w')]
    word: bool,
    /// Restrict to a language id (e.g. rust, python, swift).
    #[arg(long)]
    lang: Option<String>,
    /// Restrict to paths containing this substring.
    #[arg(long)]
    path: Option<String>,
    /// Maximum number of results.
    #[arg(long, default_value_t = 50)]
    limit: usize,
    /// Skip the first N results (pagination).
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Maximum matches reported per file.
    #[arg(long, default_value_t = 20)]
    max_per_file: usize,
    /// Return EVERY match (grep parity): no ranking, no limit, no per-file cap.
    /// Use when completeness matters more than relevance.
    #[arg(long)]
    exhaustive: bool,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SymbolsArgs {
    /// Symbol name (or fuzzy fragment).
    name: String,
    #[command(flatten)]
    root: RootArg,
    /// Restrict to a symbol kind (function, class, struct, ...).
    #[arg(long)]
    kind: Option<String>,
    /// Require an exact name match.
    #[arg(long)]
    exact: bool,
    /// Maximum number of results.
    #[arg(long, default_value_t = 50)]
    limit: usize,
    /// Skip the first N results (pagination).
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct RefsArgs {
    /// Identifier to find references for.
    name: String,
    #[command(flatten)]
    root: RootArg,
    /// Maximum number of results.
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// Skip the first N results (pagination).
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct CallArgs {
    /// Symbol name (function/method) to analyze.
    name: String,
    #[command(flatten)]
    root: RootArg,
    /// Maximum number of results.
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// Skip the first N results (pagination).
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ImpactArgs {
    /// Symbol name to analyze.
    name: String,
    #[command(flatten)]
    root: RootArg,
    /// Maximum number of caller hops to follow.
    #[arg(long, default_value_t = 3)]
    depth: u32,
    /// Maximum number of affected symbols to report.
    #[arg(long, default_value_t = 200)]
    limit: usize,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct BlameArgs {
    /// File path relative to the project root.
    file: String,
    /// Line number (1-based).
    line: u32,
    #[command(flatten)]
    root: RootArg,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct HistoryArgs {
    /// Symbol name to show history for.
    name: String,
    #[command(flatten)]
    root: RootArg,
    /// Maximum number of commits.
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ChangedArgs {
    /// Git revision to diff against (e.g. main, HEAD~5, a tag).
    rev: String,
    #[command(flatten)]
    root: RootArg,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct PackArgs {
    /// The task or question to assemble context for.
    task: String,
    #[command(flatten)]
    root: RootArg,
    /// Token budget for the assembled context.
    #[arg(long, default_value_t = 8000)]
    budget: u64,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct AstArgs {
    /// A tree-sitter query S-expression, or a `$NAME` meta-variable pattern
    /// (e.g. `fn $NAME() {}`).
    pattern: String,
    /// Language id to search (required; node kinds are language-specific).
    #[arg(long)]
    lang: String,
    #[command(flatten)]
    root: RootArg,
    /// Maximum number of results.
    #[arg(long, default_value_t = 50)]
    limit: usize,
    /// Skip the first N results (pagination).
    #[arg(long, default_value_t = 0)]
    offset: usize,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DefArgs {
    /// File path relative to the project root.
    file: String,
    /// Line of the identifier (1-based).
    line: u32,
    /// Column of the identifier (1-based).
    col: u32,
    #[command(flatten)]
    root: RootArg,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SnippetArgs {
    /// File path relative to the project root.
    file: String,
    /// Start line (1-based).
    start: u32,
    /// End line (1-based); defaults to start.
    end: Option<u32>,
    #[command(flatten)]
    root: RootArg,
    /// Context lines to include around the range.
    #[arg(long, default_value_t = 3)]
    context: u32,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct OutlineArgs {
    /// File path relative to the project root.
    file: String,
    #[command(flatten)]
    root: RootArg,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct SavingsArgs {
    #[command(flatten)]
    root: RootArg,
    /// Also show the breakdown by query kind.
    #[arg(long, short = 'v')]
    verbose: bool,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct WatchArgs {
    #[command(flatten)]
    root: RootArg,
    /// Debounce window in milliseconds.
    #[arg(long, default_value_t = 300)]
    debounce_ms: u64,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[command(flatten)]
    root: RootArg,
    /// Serve EVERY project on this machine from one daemon, loading each lazily
    /// on first query and evicting idle ones. Listens on the global socket, so a
    /// single background process covers all your repos. Ignores `-C`.
    #[arg(long)]
    global: bool,
}

fn cwd() -> Result<PathBuf> {
    std::env::current_dir().context("cannot determine current directory")
}

/// Route a request through a running daemon if one can serve it: the global
/// multi-root daemon first, then a per-project daemon. Returns `None` to signal
/// the caller should run the query in-process (no daemon, or neither could
/// serve it).
fn via_daemon<T: DeserializeOwned>(root: &RootArg, req: Request) -> Option<Result<T>> {
    if root.no_daemon {
        return None;
    }
    let g = open_for_query(root).ok()?;

    /// Decode an OK response; on a daemon error response, return `None` so the
    /// caller falls back to the next transport rather than surfacing it.
    fn decode<T: DeserializeOwned>(resp: Response) -> Option<Result<T>> {
        if resp.ok {
            let v = resp.result.unwrap_or(serde_json::Value::Null);
            Some(serde_json::from_value(v).map_err(Into::into))
        } else {
            None
        }
    }

    // Global daemon (serves every project; addressed by resolved root).
    if let Some(mut c) = Client::try_connect(&greplm_core::proto::global_socket_path()) {
        if let Ok(resp) = c.request_routed(g.root(), &req) {
            if let Some(r) = decode(resp) {
                return Some(r);
            }
        }
    }
    // Per-project daemon.
    if let Some(mut c) = Client::try_connect(&g.socket_path()) {
        if let Ok(resp) = c.request(&req) {
            if let Some(r) = decode(resp) {
                return Some(r);
            }
        }
    }
    None
}

fn open_for_index(root: &RootArg) -> Result<Greplm> {
    let root = match &root.root {
        Some(r) => r.clone(),
        None => cwd()?,
    };
    Ok(Greplm::open(root)?)
}

fn open_for_query(root: &RootArg) -> Result<Greplm> {
    let start = match &root.root {
        Some(r) => r.clone(),
        None => cwd()?,
    };
    Ok(Greplm::discover(start)?)
}

/// Record a query's token savings: the unique result files (grep+read baseline)
/// versus the size of the compact payload greplm returned. Best-effort.
fn record_savings(
    root: &RootArg,
    kind: &str,
    files: BTreeSet<String>,
    results: u64,
    payload: &impl serde::Serialize,
) {
    let returned_chars = serde_json::to_string(payload).map(|s| s.len()).unwrap_or(0) as u64;
    if let Ok(g) = open_for_query(root) {
        g.record_savings(kind, &files, returned_chars, results);
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("GREPLM_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    if let Err(e) = run() {
        eprintln!("greplm: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init(args) => cmd_init(args),
        Command::Index(args) => cmd_index(args),
        Command::Search(args) => cmd_search(args),
        Command::Symbols(args) => cmd_symbols(args),
        Command::Refs(args) => cmd_refs(args),
        Command::Xref(args) => cmd_xref(args),
        Command::Callers(args) => cmd_callers(args),
        Command::Callees(args) => cmd_callees(args),
        Command::Impact(args) => cmd_impact(args),
        Command::Def(args) => cmd_def(args),
        Command::RefsAt(args) => cmd_refs_at(args),
        Command::Ast(args) => cmd_ast(args),
        Command::Pack(args) => cmd_pack(args),
        Command::Blame(args) => cmd_blame(args),
        Command::History(args) => cmd_history(args),
        Command::Changed(args) => cmd_changed(args),
        Command::Outline(args) => cmd_outline(args),
        Command::Snippet(args) => cmd_snippet(args),
        Command::Summary(args) => cmd_summary(args),
        Command::Status(args) => cmd_status(args),
        Command::Watch(args) => cmd_watch(args),
        Command::Serve(args) => cmd_serve(args),
        #[cfg(feature = "semantic")]
        Command::SemanticIndex(args) => cmd_semantic_index(args),
        #[cfg(feature = "semantic")]
        Command::SemanticSearch(args) => cmd_semantic_search(args),
        Command::Savings(args) => cmd_savings(args),
        Command::Agent(cmd) => cmd_agent(cmd),
        Command::Clean(args) => cmd_clean(args),
    }
}

fn cmd_agent(cmd: AgentCommand) -> Result<()> {
    match cmd {
        AgentCommand::Add(args) => {
            let root = match args.root {
                Some(r) => r,
                None => cwd()?,
            };
            agent::add(args.tool.as_deref(), &root, args.global, args.force)
        }
        AgentCommand::List(args) => {
            let root = match args.root {
                Some(r) => r,
                None => cwd()?,
            };
            agent::list(&root, args.global)
        }
    }
}

#[cfg(feature = "semantic")]
fn make_embedder(model: &Option<PathBuf>) -> Result<Box<dyn greplm_core::semantic::Embedder>> {
    let dir = model
        .clone()
        .or_else(|| std::env::var_os("GREPLM_SEMANTIC_MODEL").map(PathBuf::from));
    match dir {
        Some(d) => {
            use greplm_core::semantic::Embedder;
            let e = greplm_core::semantic::Model2VecEmbedder::from_dir(&d)?;
            eprintln!(
                "using model2vec embedder at {} (dim {})",
                d.display(),
                e.dim()
            );
            Ok(Box::new(e))
        }
        None => Ok(Box::new(greplm_core::semantic::HashEmbedder::default())),
    }
}

#[cfg(feature = "semantic")]
fn cmd_semantic_index(args: SemanticIndexArgs) -> Result<()> {
    let g = open_for_index(&args.root)?;
    let embedder = make_embedder(&args.model)?;
    let start = std::time::Instant::now();
    let n = greplm_core::semantic::build(&g, embedder.as_ref())?;
    println!("embedded {} chunks in {:.2?}", n, start.elapsed());
    Ok(())
}

#[cfg(feature = "semantic")]
fn cmd_semantic_search(args: SemanticSearchArgs) -> Result<()> {
    let g = open_for_query(&args.root)?;
    let embedder = make_embedder(&args.model)?;
    let hits = greplm_core::semantic::search(&g, embedder.as_ref(), &args.query, args.limit)?;
    {
        // Returned cost = the chunk (line range) the agent would read, mirroring
        // semble's snippet accounting; baseline = the matched files in full.
        let root = g.root();
        let returned_chars: u64 = hits
            .iter()
            .map(|h| line_range_chars(&root.join(&h.path), h.line_start, h.line_end))
            .sum();
        let files: BTreeSet<String> = hits.iter().map(|h| h.path.clone()).collect();
        g.record_savings("semantic", &files, returned_chars, hits.len() as u64);
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for h in &hits {
            writeln!(
                out,
                "{:.3}  {:<10} {:<24} {}:{}-{}",
                h.score, h.kind, h.name, h.path, h.line_start, h.line_end
            )?;
        }
    }
    Ok(())
}

fn cmd_init(args: RootArg) -> Result<()> {
    let g = open_for_index(&args)?;
    let base = g.root().join(".greplm");
    let existed = base.is_dir();
    g.ensure_initialized()?;
    if existed {
        println!("already initialized at {}", base.display());
    } else {
        println!("initialized greplm at {}", base.display());
        println!(
            "edit {} then run `greplm index`",
            base.join("config.toml").display()
        );
    }
    Ok(())
}

fn cmd_index(args: IndexArgs) -> Result<()> {
    // Translate one-off flags into env overrides, which `Config::load` applies on
    // top of the persisted config. This keeps a single override path (env) and
    // avoids mutating `config.toml`.
    if args.index_binary {
        std::env::set_var("GREPLM_INDEX_BINARY", "1");
    }
    if args.index_empty {
        std::env::set_var("GREPLM_INDEX_EMPTY", "1");
    }
    if let Some(n) = args.max_file_size {
        std::env::set_var("GREPLM_MAX_FILE_SIZE", n.to_string());
    }

    let g = open_for_index(&args.root)?;
    let start = std::time::Instant::now();
    let stats = g.index(args.force)?;
    let elapsed = start.elapsed();

    // Render the per-reason skip breakdown as e.g. "binary=3, too_large=1".
    let skip_breakdown = || -> String {
        stats
            .skipped_by_reason
            .iter()
            .map(|(reason, n)| format!("{}={}", reason.as_str(), n))
            .collect::<Vec<_>>()
            .join(", ")
    };

    if args.json {
        let v = serde_json::json!({
            "files_indexed": stats.files_indexed,
            "files_skipped": stats.files_skipped,
            "skipped_by_reason": stats.skipped_by_reason,
            "skipped_sample": stats.skipped_sample,
            "files_removed": stats.files_removed,
            "symbols": stats.symbols,
            "segments": stats.segments,
            "elapsed_ms": elapsed.as_millis(),
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
    } else {
        println!(
            "indexed {} files ({} symbols), {} removed, {} segment(s) in {:.2?}",
            stats.files_indexed, stats.symbols, stats.files_removed, stats.segments, elapsed
        );
        if stats.files_skipped > 0 {
            println!("skipped {} files ({})", stats.files_skipped, skip_breakdown());
            if args.explain_skips {
                for s in &stats.skipped_sample {
                    println!("  {} ({})", s.rel, s.reason.as_str());
                }
                if stats.skipped_sample.len() < stats.files_skipped {
                    println!(
                        "  ... and {} more",
                        stats.files_skipped - stats.skipped_sample.len()
                    );
                }
            } else {
                println!("  (run with --explain-skips to list them)");
            }
        }
    }
    Ok(())
}

fn cmd_search(args: SearchArgs) -> Result<()> {
    let query = SearchQuery {
        pattern: args.query,
        regex: args.regex,
        case_insensitive: args.ignore_case,
        whole_word: args.word,
        lang: args.lang,
        path: args.path,
        limit: args.limit,
        offset: args.offset,
        max_per_file: args.max_per_file,
        exhaustive: args.exhaustive,
    };
    let hits: Vec<SearchHit> = match via_daemon(&args.root, Request::Search(query.clone())) {
        Some(r) => r?,
        None => {
            // In-process (no daemon): self-heal a missing index, then search
            // with a transparent grep fallback if the index is unavailable.
            let g = open_for_query(&args.root)?;
            let _ = g.ensure_indexed();
            g.search_or_grep(&query)?
        }
    };
    let files: BTreeSet<String> = hits.iter().map(|h| h.path.clone()).collect();
    record_savings(&args.root, "search", files, hits.len() as u64, &hits);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for h in &hits {
            writeln!(out, "{}:{}:{}: {}", h.path, h.line, h.column, h.text)?;
        }
        if hits.is_empty() {
            eprintln!("no matches");
        }
    }
    Ok(())
}

fn cmd_symbols(args: SymbolsArgs) -> Result<()> {
    let query = SymbolQuery {
        name: args.name,
        kind: args.kind,
        exact: args.exact,
        limit: args.limit,
        offset: args.offset,
    };
    let hits: Vec<SymbolHit> = match via_daemon(&args.root, Request::Symbols(query.clone())) {
        Some(r) => r?,
        None => open_for_query(&args.root)?.searcher()?.symbols(&query)?,
    };
    let files: BTreeSet<String> = hits.iter().map(|h| h.path.clone()).collect();
    record_savings(&args.root, "symbols", files, hits.len() as u64, &hits);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for h in &hits {
            writeln!(
                out,
                "{:<10} {:<24} {}:{}-{}",
                h.kind, h.name, h.path, h.line_start, h.line_end
            )?;
        }
        if hits.is_empty() {
            eprintln!("no symbols");
        }
    }
    Ok(())
}

fn cmd_outline(args: OutlineArgs) -> Result<()> {
    let req = Request::Outline {
        file: args.file.clone(),
    };
    let hits: Vec<SymbolHit> = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?
            .searcher()?
            .outline(&args.file)?,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for h in &hits {
            writeln!(out, "{:>5}  {:<10} {}", h.line_start, h.kind, h.name)?;
        }
        if hits.is_empty() {
            eprintln!("no symbols in {}", args.file);
        }
    }
    Ok(())
}

fn cmd_refs(args: RefsArgs) -> Result<()> {
    let req = Request::Refs {
        name: args.name.clone(),
        limit: args.limit,
        offset: args.offset,
    };
    let hits: Vec<SearchHit> = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?.searcher()?.references(
            &args.name,
            args.limit,
            args.offset,
        )?,
    };
    let files: BTreeSet<String> = hits.iter().map(|h| h.path.clone()).collect();
    record_savings(&args.root, "refs", files, hits.len() as u64, &hits);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for h in &hits {
            writeln!(out, "{}:{}:{}: {}", h.path, h.line, h.column, h.text)?;
        }
        if hits.is_empty() {
            eprintln!("no references");
        }
    }
    Ok(())
}

fn cmd_xref(args: RefsArgs) -> Result<()> {
    let req = Request::RefsResolved {
        name: args.name.clone(),
        limit: args.limit,
        offset: args.offset,
    };
    let hits: Vec<RefHit> = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?.searcher()?.references_resolved(
            &args.name,
            args.limit,
            args.offset,
        ),
    };
    let files: BTreeSet<String> = hits.iter().map(|h| h.path.clone()).collect();
    record_savings(&args.root, "xref", files, hits.len() as u64, &hits);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for h in &hits {
            let ctx = h
                .container
                .as_deref()
                .map(|c| format!("  [{c}]"))
                .unwrap_or_default();
            writeln!(
                out,
                "{:<11} {}:{}:{}{}",
                h.kind, h.path, h.line, h.column, ctx
            )?;
        }
        if hits.is_empty() {
            eprintln!("no resolved references");
        }
    }
    Ok(())
}

fn cmd_callers(args: CallArgs) -> Result<()> {
    let req = Request::Callers {
        name: args.name.clone(),
        limit: args.limit,
        offset: args.offset,
    };
    let hits: Vec<CallSite> = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => {
            open_for_query(&args.root)?
                .searcher()?
                .callers(&args.name, args.limit, args.offset)
        }
    };
    let files: BTreeSet<String> = hits.iter().map(|h| h.path.clone()).collect();
    record_savings(&args.root, "callers", files, hits.len() as u64, &hits);
    print_callsites(&hits, args.json)
}

fn cmd_callees(args: CallArgs) -> Result<()> {
    let req = Request::Callees {
        name: args.name.clone(),
        limit: args.limit,
        offset: args.offset,
    };
    let hits: Vec<CallSite> = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => {
            open_for_query(&args.root)?
                .searcher()?
                .callees(&args.name, args.limit, args.offset)
        }
    };
    let files: BTreeSet<String> = hits.iter().map(|h| h.path.clone()).collect();
    record_savings(&args.root, "callees", files, hits.len() as u64, &hits);
    print_callsites(&hits, args.json)
}

fn print_callsites(hits: &[CallSite], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(hits)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for h in hits {
            let caller = h.caller.as_deref().unwrap_or("<file>");
            writeln!(
                out,
                "{} -> {}  {}:{}:{}",
                caller, h.callee, h.path, h.line, h.column
            )?;
        }
        if hits.is_empty() {
            eprintln!("no call sites");
        }
    }
    Ok(())
}

fn cmd_impact(args: ImpactArgs) -> Result<()> {
    let req = Request::BlastRadius {
        name: args.name.clone(),
        depth: args.depth,
        limit: args.limit,
    };
    let nodes: Vec<ImpactNode> = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?
            .searcher()?
            .blast_radius(&args.name, args.depth, args.limit),
    };
    let files: BTreeSet<String> = nodes.iter().map(|n| n.path.clone()).collect();
    record_savings(&args.root, "impact", files, nodes.len() as u64, &nodes);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for n in &nodes {
            writeln!(
                out,
                "d{}  {:<10} {:<24} {}:{}-{}",
                n.distance, n.kind, n.name, n.path, n.line_start, n.line_end
            )?;
        }
        if nodes.is_empty() {
            eprintln!("no impact found (is the project indexed?)");
        }
    }
    Ok(())
}

fn cmd_def(args: DefArgs) -> Result<()> {
    let req = Request::Definition {
        file: args.file.clone(),
        line: args.line,
        col: args.col,
    };
    let hits: Vec<DefHit> = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?
            .searcher()?
            .definition(&args.file, args.line, args.col)?,
    };
    let files: BTreeSet<String> = hits.iter().map(|h| h.path.clone()).collect();
    record_savings(&args.root, "def", files, hits.len() as u64, &hits);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for h in &hits {
            let mark = if h.resolved { "*" } else { " " };
            writeln!(
                out,
                "{} {:<10} {:<24} {}:{}-{}",
                mark, h.kind, h.name, h.path, h.line_start, h.line_end
            )?;
        }
        if hits.is_empty() {
            eprintln!("no definition found");
        }
    }
    Ok(())
}

fn cmd_refs_at(args: DefArgs) -> Result<()> {
    let req = Request::ReferencesAt {
        file: args.file.clone(),
        line: args.line,
        col: args.col,
    };
    let hits: Vec<RefHit> = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?
            .searcher()?
            .references_of(&args.file, args.line, args.col)?,
    };
    let files: BTreeSet<String> = hits.iter().map(|h| h.path.clone()).collect();
    record_savings(&args.root, "refs-at", files, hits.len() as u64, &hits);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for h in &hits {
            let ctx = h
                .container
                .as_deref()
                .map(|c| format!("  [{c}]"))
                .unwrap_or_default();
            writeln!(
                out,
                "{:<11} {}:{}:{}{}",
                h.kind, h.path, h.line, h.column, ctx
            )?;
        }
        if hits.is_empty() {
            eprintln!("no resolved references");
        }
    }
    Ok(())
}

fn cmd_ast(args: AstArgs) -> Result<()> {
    let req = Request::Structural {
        pattern: args.pattern.clone(),
        lang: args.lang.clone(),
        limit: args.limit,
        offset: args.offset,
    };
    let hits: Vec<StructHit> = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?.searcher()?.structural_search(
            &args.pattern,
            &args.lang,
            args.limit,
            args.offset,
        )?,
    };
    let files: BTreeSet<String> = hits.iter().map(|h| h.path.clone()).collect();
    record_savings(&args.root, "ast", files, hits.len() as u64, &hits);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for h in &hits {
            let caps: String = h
                .captures
                .iter()
                .map(|c| format!(" {}={}", c.name, c.text))
                .collect();
            writeln!(
                out,
                "{}:{}-{}: {}{}",
                h.path, h.line_start, h.line_end, h.text, caps
            )?;
        }
        if hits.is_empty() {
            eprintln!("no structural matches");
        }
    }
    Ok(())
}

fn cmd_pack(args: PackArgs) -> Result<()> {
    let req = Request::ContextPack {
        task: args.task.clone(),
        budget: args.budget,
    };
    let pack: ContextPack = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?
            .searcher()?
            .context_pack(&args.task, args.budget),
    };
    // Record savings: baseline is the files we'd otherwise read whole.
    let files: BTreeSet<String> = pack.items.iter().map(|i| i.path.clone()).collect();
    record_savings(&args.root, "pack", files, pack.items.len() as u64, &pack);
    if args.json {
        println!("{}", serde_json::to_string_pretty(&pack)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        writeln!(
            out,
            "# context pack for: {}\n# {} items, ~{}/{} tokens{}\n",
            pack.task,
            pack.items.len(),
            pack.used_tokens,
            pack.budget_tokens,
            if pack.truncated { " (truncated)" } else { "" }
        )?;
        for item in &pack.items {
            writeln!(
                out,
                "## {} {} ({})  {}:{}-{}  [{:.1}]",
                item.kind,
                item.name,
                item.reason,
                item.path,
                item.line_start,
                item.line_end,
                item.score,
            )?;
            for line in &item.snippet {
                writeln!(out, "{:>6} | {}", line.line, line.text)?;
            }
            writeln!(out)?;
        }
        if pack.items.is_empty() {
            eprintln!("no relevant context found (is the project indexed?)");
        }
    }
    Ok(())
}

fn cmd_blame(args: BlameArgs) -> Result<()> {
    let req = Request::Blame {
        file: args.file.clone(),
        line: args.line,
    };
    let b: BlameLine = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?
            .searcher()?
            .blame(&args.file, args.line)?,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&b)?);
    } else {
        println!(
            "{} {} {}:{}\n{}",
            b.commit, b.author, b.path, b.line, b.summary
        );
    }
    Ok(())
}

fn cmd_history(args: HistoryArgs) -> Result<()> {
    let req = Request::History {
        name: args.name.clone(),
        limit: args.limit,
    };
    let h: SymbolHistory = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?
            .searcher()?
            .symbol_history(&args.name, args.limit)?,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&h)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        writeln!(
            out,
            "history of {} ({}:{}-{})",
            h.name, h.path, h.line_start, h.line_end
        )?;
        for c in &h.commits {
            writeln!(out, "{}  {:<18} {}", c.commit, c.author, c.summary)?;
        }
        if h.commits.is_empty() {
            eprintln!("no history (not a git repo, or no commits touch this symbol)");
        }
    }
    Ok(())
}

fn cmd_changed(args: ChangedArgs) -> Result<()> {
    let req = Request::ChangedSince {
        rev: args.rev.clone(),
    };
    let changed: Vec<ChangedSymbols> = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?
            .searcher()?
            .changed_since(&args.rev)?,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&changed)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for c in &changed {
            writeln!(out, "{} {}", c.status, c.path)?;
            if !c.symbols.is_empty() {
                writeln!(out, "    {}", c.symbols.join(", "))?;
            }
        }
        if changed.is_empty() {
            eprintln!("no changes since {}", args.rev);
        }
    }
    Ok(())
}

fn cmd_snippet(args: SnippetArgs) -> Result<()> {
    let end = args.end.unwrap_or(args.start);
    let req = Request::Snippet {
        file: args.file.clone(),
        start: args.start,
        end,
        context: args.context,
    };
    let snip: Snippet = match via_daemon(&args.root, req) {
        Some(r) => r?,
        None => open_for_query(&args.root)?.searcher()?.read_snippet(
            &args.file,
            args.start,
            end,
            args.context,
        )?,
    };
    let returned_chars: u64 = snip.lines.iter().map(|l| l.text.len() as u64 + 1).sum();
    if let Ok(g) = open_for_query(&args.root) {
        let files: BTreeSet<String> = [snip.path.clone()].into_iter().collect();
        g.record_savings("snippet", &files, returned_chars, 1);
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&snip)?);
    } else {
        let mut out = std::io::BufWriter::new(std::io::stdout());
        for line in &snip.lines {
            writeln!(out, "{:>6} | {}", line.line, line.text)?;
        }
    }
    Ok(())
}

fn cmd_summary(args: RootArg) -> Result<()> {
    let summary: RepoSummary = match via_daemon(&args, Request::Summary) {
        Some(r) => r?,
        None => open_for_query(&args)?.searcher()?.summary(),
    };
    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}

fn cmd_status(args: RootArg) -> Result<()> {
    let status: Status = match via_daemon(&args, Request::Status) {
        Some(r) => r?,
        None => open_for_query(&args)?.status()?,
    };
    println!("{}", serde_json::to_string_pretty(&status)?);
    Ok(())
}

fn cmd_serve(args: ServeArgs) -> Result<()> {
    if args.global {
        let socket = greplm_core::proto::global_socket_path();
        eprintln!(
            "greplm global daemon: serving all projects on {} (lazy, idle-evicted)",
            socket.display()
        );
        greplm_core::daemon::serve_global(&socket)?;
        return Ok(());
    }
    let g = open_for_index(&args.root)?;
    g.ensure_initialized()?;
    let socket = g.socket_path();
    eprintln!(
        "greplm daemon: indexing {} then serving on {}",
        g.root().display(),
        socket.display()
    );
    let greplm = std::sync::Arc::new(g);
    greplm_core::daemon::serve(greplm, &socket)?;
    Ok(())
}

fn cmd_watch(args: WatchArgs) -> Result<()> {
    let g = open_for_index(&args.root)?;
    // Build once before watching.
    let stats = g.index(false)?;
    eprintln!(
        "initial index: {} files, {} symbols; watching {}",
        stats.files_indexed,
        stats.symbols,
        g.root().display()
    );
    g.watch(Duration::from_millis(args.debounce_ms), |s| {
        eprintln!(
            "reindexed: +{} files, -{} removed, {} segment(s)",
            s.files_indexed, s.files_removed, s.segments
        );
    })?;
    Ok(())
}

/// Character count of lines `[start, end]` (1-based, inclusive) of a file,
/// counting the newline per line. Used to size semantic chunks for savings.
#[cfg(feature = "semantic")]
fn line_range_chars(path: &std::path::Path, start: u32, end: u32) -> u64 {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return 0,
    };
    let lines: Vec<&str> = data.lines().collect();
    let from = start.max(1) as usize;
    let to = (end.max(start) as usize).min(lines.len());
    lines
        .get(from.saturating_sub(1)..to)
        .map(|s| s.iter().map(|l| l.len() as u64 + 1).sum())
        .unwrap_or(0)
}

fn human_tokens(n: u64) -> String {
    let n = n as f64;
    if n >= 1_000_000.0 {
        format!("{:.1}M", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else {
        format!("{n:.0}")
    }
}

fn savings_bar(ratio: f64) -> String {
    const WIDTH: usize = 16;
    let filled = (ratio * WIDTH as f64).round().clamp(0.0, WIDTH as f64) as usize;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(WIDTH - filled))
}

fn cmd_savings(args: SavingsArgs) -> Result<()> {
    let g = open_for_query(&args.root)?;
    let report = g.savings_report();
    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("  greplm Token Savings");
    println!("  {}", "=".repeat(64));
    println!("  {:<14}{:>7}   Savings", "Period", "Calls");
    println!("  {}", "-".repeat(64));
    for p in &report.periods {
        println!(
            "  {:<14}{:>7}   {}  ~{} tokens ({:.0}%)",
            p.label,
            p.calls,
            savings_bar(p.ratio()),
            human_tokens(p.tokens_saved()),
            p.ratio() * 100.0,
        );
    }
    if args.verbose && !report.by_kind.is_empty() {
        println!("\n  By call type (all time)");
        println!("  {}", "-".repeat(64));
        for k in &report.by_kind {
            println!(
                "  {:<14}{:>7}   {}  ~{} tokens ({:.0}%)",
                k.label,
                k.calls,
                savings_bar(k.ratio()),
                human_tokens(k.tokens_saved()),
                k.ratio() * 100.0,
            );
        }
    }
    if report.periods.last().map(|p| p.calls).unwrap_or(0) == 0 {
        eprintln!("\nno queries recorded yet — run some searches first");
    }
    Ok(())
}

fn cmd_clean(args: RootArg) -> Result<()> {
    let g = open_for_query(&args)?;
    g.clean()?;
    println!("removed {}", g.root().join(".greplm").display());
    Ok(())
}
