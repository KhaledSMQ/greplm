//! `greplm agent` — install the bundled agent definition for a coding tool.
//!
//! The agent markdown lives in this crate's `agents/` directory and is embedded
//! into the binary at build time, so installation works fully offline (no curl
//! round-trip) and ships with `cargo install` / release binaries.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Shared, greplm-first guidance for a tool's *main* agent loop. The bundled
/// subagent file only runs when the primary agent chooses to delegate to it; to
/// actually steer the default tool selection (away from grep) we also write to
/// the tool's always-on memory/rules file.
struct RuleSpec {
    /// Memory file location relative to the *project* root.
    project_dir: &'static str,
    /// Memory file location relative to the user *home* (for `--global`).
    global_dir: &'static str,
    /// Memory filename.
    file: &'static str,
    /// Front matter written only when the file is *created* (e.g. Cursor `.mdc`
    /// needs `alwaysApply: true`). On existing files we only manage our own
    /// delimited block, so the user's front matter and surrounding content stay
    /// untouched.
    frontmatter: Option<&'static str>,
}

/// A supported coding tool and where its agent definition belongs.
struct AgentTool {
    /// Key used on the command line (e.g. `cursor`).
    key: &'static str,
    /// Human-readable label.
    label: &'static str,
    /// Destination directory, relative to the scope root (project or home).
    dir: &'static str,
    /// Destination filename.
    file: &'static str,
    /// The embedded agent markdown (subagent definition).
    content: &'static str,
    /// Where to write always-on, greplm-first guidance for the main loop.
    rules: RuleSpec,
}

/// The greplm-first guidance body shared across every tool's memory file.
const RULES_BODY: &str = include_str!("../agents/rules.md");

/// Markers delimiting greplm's block inside a *shared* memory file so it can be
/// detected (idempotent installs) and refreshed (`--force`).
const RULES_BEGIN: &str = "<!-- greplm:begin -->";
const RULES_END: &str = "<!-- greplm:end -->";

/// Front matter so Cursor treats the owned rule file as always-applied.
const CURSOR_FRONTMATTER: &str =
    "---\ndescription: Prefer greplm for code search, navigation, and code intelligence\nalwaysApply: true\n---\n\n";

const TOOLS: &[AgentTool] = &[
    AgentTool {
        key: "claude",
        label: "Claude Code",
        dir: ".claude/agents",
        file: "greplm-search.md",
        content: include_str!("../agents/claude.md"),
        rules: RuleSpec {
            project_dir: "",
            global_dir: ".claude",
            file: "CLAUDE.md",
            frontmatter: None,
        },
    },
    AgentTool {
        key: "cursor",
        label: "Cursor",
        dir: ".cursor/agents",
        file: "greplm-search.md",
        content: include_str!("../agents/cursor.md"),
        rules: RuleSpec {
            project_dir: ".cursor/rules",
            global_dir: ".cursor/rules",
            file: "greplm.mdc",
            frontmatter: Some(CURSOR_FRONTMATTER),
        },
    },
    AgentTool {
        key: "gemini",
        label: "Gemini CLI",
        dir: ".gemini/agents",
        file: "greplm-search.md",
        content: include_str!("../agents/gemini.md"),
        rules: RuleSpec {
            project_dir: "",
            global_dir: ".gemini",
            file: "GEMINI.md",
            frontmatter: None,
        },
    },
    AgentTool {
        key: "copilot",
        label: "GitHub Copilot",
        dir: ".github/agents",
        file: "greplm-search.agent.md",
        content: include_str!("../agents/copilot.md"),
        rules: RuleSpec {
            project_dir: ".github",
            global_dir: ".github",
            file: "copilot-instructions.md",
            frontmatter: None,
        },
    },
    AgentTool {
        key: "opencode",
        label: "opencode",
        dir: ".opencode/agent",
        file: "greplm-search.md",
        content: include_str!("../agents/opencode.md"),
        rules: RuleSpec {
            project_dir: "",
            global_dir: ".config/opencode",
            file: "AGENTS.md",
            frontmatter: None,
        },
    },
    AgentTool {
        key: "kiro",
        label: "Kiro",
        dir: ".kiro/agents",
        file: "greplm-search.md",
        content: include_str!("../agents/kiro.md"),
        rules: RuleSpec {
            project_dir: ".kiro/steering",
            global_dir: ".kiro/steering",
            file: "greplm.md",
            frontmatter: None,
        },
    },
    AgentTool {
        key: "pi",
        label: "Pi",
        dir: ".pi/agents",
        file: "greplm-search.md",
        content: include_str!("../agents/pi.md"),
        rules: RuleSpec {
            project_dir: "",
            global_dir: ".pi",
            file: "AGENTS.md",
            frontmatter: None,
        },
    },
    AgentTool {
        key: "reasonix",
        label: "Reasonix",
        dir: ".reasonix/agents",
        file: "greplm-search.md",
        content: include_str!("../agents/reasonix.md"),
        rules: RuleSpec {
            project_dir: "",
            global_dir: ".reasonix",
            file: "AGENTS.md",
            frontmatter: None,
        },
    },
];

impl AgentTool {
    /// Absolute destination path for this tool under the given scope root.
    fn dest(&self, scope_root: &Path) -> PathBuf {
        scope_root.join(self.dir).join(self.file)
    }

    /// Absolute destination path for this tool's main-loop memory/rules file.
    fn rules_dest(&self, scope_root: &Path, global: bool) -> PathBuf {
        let dir = if global {
            self.rules.global_dir
        } else {
            self.rules.project_dir
        };
        scope_root.join(dir).join(self.rules.file)
    }
}

/// Result of writing a tool's main-loop memory/rules file.
enum RuleOutcome {
    /// Owned file written, or a fresh block appended to a shared file.
    Wrote(PathBuf),
    /// Existing greplm block refreshed in place (`--force`).
    Updated(PathBuf),
    /// Nothing changed (already present, no `--force`).
    Skipped(PathBuf),
}

/// Replace the existing `greplm:begin..end` block in `haystack` with `block`.
fn replace_rules_block(haystack: &str, block: &str) -> String {
    let Some(start) = haystack.find(RULES_BEGIN) else {
        return haystack.to_string();
    };
    let Some(end_rel) = haystack[start..].find(RULES_END) else {
        return haystack.to_string();
    };
    let end = start + end_rel + RULES_END.len();
    let mut out = String::with_capacity(haystack.len() + block.len());
    out.push_str(&haystack[..start]);
    out.push_str(block.trim_end());
    out.push_str(&haystack[end..]);
    out
}

/// Install (or refresh) the greplm-first guidance for a tool's main agent loop.
fn install_rules(
    tool: &AgentTool,
    scope_root: &Path,
    global: bool,
    force: bool,
) -> Result<RuleOutcome> {
    let dest = tool.rules_dest(scope_root, global);
    write_rules_file(&dest, tool.rules.frontmatter, force)
}

/// Write greplm's delimited guidance block into `dest`, preserving any existing
/// user content. `frontmatter` is only used when creating the file from scratch.
///
/// - File absent → create it (front matter, if any, then the block).
/// - Block already present → skip, or refresh in place under `--force`.
/// - Other content present → append the block after a blank line.
fn write_rules_file(dest: &Path, frontmatter: Option<&str>, force: bool) -> Result<RuleOutcome> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let block = format!("{RULES_BEGIN}\n{}\n{RULES_END}\n", RULES_BODY.trim_end());
    let existing = std::fs::read_to_string(dest).unwrap_or_default();

    if existing.contains(RULES_BEGIN) {
        if !force {
            return Ok(RuleOutcome::Skipped(dest.to_path_buf()));
        }
        let updated = replace_rules_block(&existing, &block);
        std::fs::write(dest, updated).with_context(|| format!("writing {}", dest.display()))?;
        return Ok(RuleOutcome::Updated(dest.to_path_buf()));
    }

    let mut content = String::new();
    if existing.is_empty() {
        // Fresh file: front matter (if any) sits above our block.
        if let Some(fm) = frontmatter {
            content.push_str(fm);
        }
    } else {
        // Preserve the user's content; separate our block with a blank line.
        content.push_str(&existing);
        if !content.ends_with('\n') {
            content.push('\n');
        }
        content.push('\n');
    }
    content.push_str(&block);
    std::fs::write(dest, content).with_context(|| format!("writing {}", dest.display()))?;
    Ok(RuleOutcome::Wrote(dest.to_path_buf()))
}

fn find_tool(key: &str) -> Result<&'static AgentTool> {
    let needle = key.to_ascii_lowercase();
    TOOLS.iter().find(|t| t.key == needle).ok_or_else(|| {
        let names: Vec<&str> = TOOLS.iter().map(|t| t.key).collect();
        anyhow!("unknown tool '{key}'. Supported: {}", names.join(", "))
    })
}

/// Resolve the scope root: the user's home directory for `--global`, otherwise
/// the project root.
fn scope_root(global: bool, project_root: &Path) -> Result<PathBuf> {
    if !global {
        return Ok(project_root.to_path_buf());
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot determine home directory for --global (set HOME)")?;
    Ok(home)
}

/// Whether a tool's project memory file uniquely identifies it (i.e. no other
/// tool writes to the same path). Shared files like `AGENTS.md` (opencode / pi /
/// reasonix) are *not* unique and must not drive auto-detection.
fn rules_path_is_unique(tool: &AgentTool) -> bool {
    let key = (tool.rules.project_dir, tool.rules.file);
    TOOLS
        .iter()
        .filter(|o| (o.rules.project_dir, o.rules.file) == key)
        .count()
        == 1
}

/// Tools we can reasonably auto-install for under `root` (best-effort detection
/// for `agent add` with no tool argument). A tool is detected when either its
/// convention directory (e.g. `.cursor`, `.github`) exists, or its *unambiguous*
/// memory file (e.g. `CLAUDE.md`, `GEMINI.md`) is already present — the latter
/// catches editors configured via a root memory file before their dot-dir
/// exists.
fn detect_tools(root: &Path) -> Vec<&'static AgentTool> {
    TOOLS
        .iter()
        .filter(|t| {
            // The first path component (e.g. `.cursor`, `.github`) signals the tool.
            let dir_marker = Path::new(t.dir)
                .components()
                .next()
                .map(|c| root.join(c.as_os_str()).is_dir())
                .unwrap_or(false);
            let rules_marker = rules_path_is_unique(t)
                && root.join(t.rules.project_dir).join(t.rules.file).is_file();
            dir_marker || rules_marker
        })
        .collect()
}

/// Write one tool's agent file, honoring `force`. Returns the destination path
/// written, or `None` if it already existed and `force` was not set.
fn install_one(tool: &AgentTool, scope_root: &Path, force: bool) -> Result<Option<PathBuf>> {
    let dest = tool.dest(scope_root);
    if dest.exists() && !force {
        return Ok(None);
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&dest, tool.content).with_context(|| format!("writing {}", dest.display()))?;
    Ok(Some(dest))
}

/// Cross-tool memory file used as a universal fallback when no specific editor
/// is detected. `AGENTS.md` is read by Cursor, opencode, Codex, and others.
const UNIVERSAL_RULES_FILE: &str = "AGENTS.md";

/// Report a [`RuleOutcome`] to stdout and note whether it changed anything.
fn report_rule(outcome: RuleOutcome, wrote_any: &mut bool) {
    match outcome {
        RuleOutcome::Wrote(dest) => {
            println!("  ↳ main-loop guidance -> {}", dest.display());
            *wrote_any = true;
        }
        RuleOutcome::Updated(dest) => {
            println!("  ↳ refreshed main-loop guidance -> {}", dest.display());
            *wrote_any = true;
        }
        RuleOutcome::Skipped(dest) => {
            println!(
                "  ↳ main-loop guidance already present at {} (use --force to refresh)",
                dest.display()
            );
        }
    }
}

/// `greplm agent add [tool]`.
pub fn add(tool: Option<&str>, project_root: &Path, global: bool, force: bool) -> Result<()> {
    let scope = scope_root(global, project_root)?;

    // Explicit tool, or whatever we can auto-detect.
    let targets: Vec<&AgentTool> = match tool {
        Some(key) => vec![find_tool(key)?],
        None => detect_tools(project_root),
    };

    let mut wrote_any = false;

    // Nothing detected (auto mode): still configure the universal AGENTS.md so
    // the agent loop prefers greplm. No subagent — we don't know which tool.
    if targets.is_empty() {
        let dest = scope.join(UNIVERSAL_RULES_FILE);
        report_rule(write_rules_file(&dest, None, force)?, &mut wrote_any);
        println!(
            "no specific editor detected — configured {}. For a tool-specific \
             subagent, run e.g. `greplm agent add cursor` (see `greplm agent list`).",
            UNIVERSAL_RULES_FILE
        );
        if wrote_any {
            println!("restart your tool (or start a new session) so it picks up the changes.");
        }
        return Ok(());
    }

    for t in targets {
        match install_one(t, &scope, force)? {
            Some(dest) => {
                println!("installed {} agent -> {}", t.label, dest.display());
                wrote_any = true;
            }
            None => {
                println!(
                    "{} agent already exists at {} (use --force to overwrite)",
                    t.label,
                    t.dest(&scope).display()
                );
            }
        }

        // The subagent above only runs on delegation; this steers the main loop
        // to prefer greplm over grep by default.
        report_rule(install_rules(t, &scope, global, force)?, &mut wrote_any);
    }

    if wrote_any {
        println!("restart your tool (or start a new session) so it picks up the changes.");
    }
    Ok(())
}

/// `greplm agent list`.
pub fn list(project_root: &Path, global: bool) -> Result<()> {
    let scope = scope_root(global, project_root)?;
    println!("{:<10} {:<16} {:<8} DESTINATION", "TOOL", "NAME", "KIND");
    let mut seen_rules: Vec<PathBuf> = Vec::new();
    for t in TOOLS {
        println!(
            "{:<10} {:<16} {:<8} {}",
            t.key,
            t.label,
            "subagent",
            t.dest(&scope).display()
        );
        // Several tools share one memory file (e.g. AGENTS.md); show it once.
        let rules = t.rules_dest(&scope, global);
        if !seen_rules.contains(&rules) {
            println!("{:<10} {:<16} {:<8} {}", "", "", "rules", rules.display());
            seen_rules.push(rules);
        }
    }
    println!(
        "{:<10} {:<16} {:<8} {}",
        "(none)",
        "universal",
        "rules",
        scope.join(UNIVERSAL_RULES_FILE).display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Throwaway directory, unique per call (no external test deps).
    fn tmp_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "greplm-agent-{tag}-{}-{nanos}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn marker_count(s: &str) -> usize {
        s.matches(RULES_BEGIN).count()
    }

    #[test]
    fn every_tool_has_a_bundled_subagent_and_rules_dest() {
        let root = Path::new("/tmp/x");
        for t in TOOLS {
            assert!(!t.content.trim().is_empty(), "{} subagent empty", t.key);
            assert!(t.dest(root).starts_with(root));
            assert!(t.rules_dest(root, false).starts_with(root));
        }
    }

    #[test]
    fn creates_file_with_frontmatter_when_absent() {
        let dir = tmp_dir("create");
        let dest = dir.join("greplm.mdc");
        let out = write_rules_file(&dest, Some(CURSOR_FRONTMATTER), false).unwrap();
        assert!(matches!(out, RuleOutcome::Wrote(_)));
        let body = std::fs::read_to_string(&dest).unwrap();
        assert!(
            body.starts_with("---\n"),
            "front matter should lead the file"
        );
        assert_eq!(marker_count(&body), 1);
        assert!(body.contains(RULES_END));
    }

    #[test]
    fn second_install_is_idempotent_skip() {
        let dir = tmp_dir("idem");
        let dest = dir.join("CLAUDE.md");
        write_rules_file(&dest, None, false).unwrap();
        let before = std::fs::read_to_string(&dest).unwrap();
        let out = write_rules_file(&dest, None, false).unwrap();
        assert!(matches!(out, RuleOutcome::Skipped(_)));
        let after = std::fs::read_to_string(&dest).unwrap();
        assert_eq!(before, after, "skip must not modify the file");
        assert_eq!(marker_count(&after), 1);
    }

    #[test]
    fn force_refreshes_block_in_place_and_preserves_user_edits() {
        let dir = tmp_dir("force");
        let dest = dir.join("greplm.mdc");
        write_rules_file(&dest, Some(CURSOR_FRONTMATTER), false).unwrap();
        // Simulate the user appending their own content below our block.
        let mut content = std::fs::read_to_string(&dest).unwrap();
        content.push_str("\n## My rule\nkeep me\n");
        std::fs::write(&dest, &content).unwrap();

        let out = write_rules_file(&dest, Some(CURSOR_FRONTMATTER), true).unwrap();
        assert!(matches!(out, RuleOutcome::Updated(_)));
        let after = std::fs::read_to_string(&dest).unwrap();
        assert_eq!(marker_count(&after), 1, "no duplicate block after --force");
        assert!(
            after.contains("keep me"),
            "user content must survive --force"
        );
        assert!(after.starts_with("---\n"), "front matter stays on top");
    }

    #[test]
    fn appends_block_to_existing_shared_file_without_clobbering() {
        let dir = tmp_dir("append");
        let dest = dir.join("AGENTS.md");
        std::fs::write(&dest, "# Team agents\n\nBe nice.\n").unwrap();
        let out = write_rules_file(&dest, None, false).unwrap();
        assert!(matches!(out, RuleOutcome::Wrote(_)));
        let after = std::fs::read_to_string(&dest).unwrap();
        assert!(after.contains("Be nice."), "existing content preserved");
        assert_eq!(marker_count(&after), 1);
        // Existing files never get front matter injected.
        assert!(after.starts_with("# Team agents"));
    }

    #[test]
    fn replace_rules_block_keeps_surrounding_text() {
        let block = format!("{RULES_BEGIN}\nNEW\n{RULES_END}");
        let original = format!("top\n\n{RULES_BEGIN}\nOLD\n{RULES_END}\n\nbottom\n");
        let out = replace_rules_block(&original, &block);
        assert!(out.starts_with("top\n"));
        assert!(out.contains("bottom"));
        assert!(out.contains("NEW") && !out.contains("OLD"));
        assert_eq!(marker_count(&out), 1);
    }

    #[test]
    fn agents_md_is_ambiguous_claude_md_is_unique() {
        let claude = find_tool("claude").unwrap();
        let opencode = find_tool("opencode").unwrap();
        assert!(rules_path_is_unique(claude), "CLAUDE.md maps to one tool");
        assert!(
            !rules_path_is_unique(opencode),
            "AGENTS.md is shared by opencode/pi/reasonix"
        );
    }

    #[test]
    fn detects_tool_via_dot_dir() {
        let dir = tmp_dir("detect-dir");
        std::fs::create_dir_all(dir.join(".cursor")).unwrap();
        let found: Vec<_> = detect_tools(&dir).iter().map(|t| t.key).collect();
        assert!(found.contains(&"cursor"));
    }

    #[test]
    fn detects_tool_via_unique_memory_file_without_dot_dir() {
        let dir = tmp_dir("detect-claude");
        std::fs::write(dir.join("CLAUDE.md"), "# hi\n").unwrap();
        let found: Vec<_> = detect_tools(&dir).iter().map(|t| t.key).collect();
        assert_eq!(found, vec!["claude"], "CLAUDE.md alone detects claude");
    }

    #[test]
    fn ambiguous_agents_md_alone_detects_nothing() {
        let dir = tmp_dir("detect-agents");
        std::fs::write(dir.join("AGENTS.md"), "# agents\n").unwrap();
        assert!(
            detect_tools(&dir).is_empty(),
            "a bare AGENTS.md must not scaffold opencode/pi/reasonix"
        );
    }
}
