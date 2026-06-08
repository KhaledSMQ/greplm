//! `greplm agent` — install the bundled agent definition for a coding tool.
//!
//! The agent markdown lives in the repo's `agents/` directory and is embedded
//! into the binary at build time, so installation works fully offline (no curl
//! round-trip) and ships with `cargo install` / release binaries.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

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
    /// The embedded agent markdown.
    content: &'static str,
}

const TOOLS: &[AgentTool] = &[
    AgentTool {
        key: "claude",
        label: "Claude Code",
        dir: ".claude/agents",
        file: "greplm-search.md",
        content: include_str!("../../../agents/claude.md"),
    },
    AgentTool {
        key: "cursor",
        label: "Cursor",
        dir: ".cursor/agents",
        file: "greplm-search.md",
        content: include_str!("../../../agents/cursor.md"),
    },
    AgentTool {
        key: "gemini",
        label: "Gemini CLI",
        dir: ".gemini/agents",
        file: "greplm-search.md",
        content: include_str!("../../../agents/gemini.md"),
    },
    AgentTool {
        key: "copilot",
        label: "GitHub Copilot",
        dir: ".github/agents",
        file: "greplm-search.agent.md",
        content: include_str!("../../../agents/copilot.md"),
    },
    AgentTool {
        key: "opencode",
        label: "opencode",
        dir: ".opencode/agent",
        file: "greplm-search.md",
        content: include_str!("../../../agents/opencode.md"),
    },
    AgentTool {
        key: "kiro",
        label: "Kiro",
        dir: ".kiro/agents",
        file: "greplm-search.md",
        content: include_str!("../../../agents/kiro.md"),
    },
    AgentTool {
        key: "pi",
        label: "Pi",
        dir: ".pi/agents",
        file: "greplm-search.md",
        content: include_str!("../../../agents/pi.md"),
    },
    AgentTool {
        key: "reasonix",
        label: "Reasonix",
        dir: ".reasonix/agents",
        file: "greplm-search.md",
        content: include_str!("../../../agents/reasonix.md"),
    },
];

impl AgentTool {
    /// Absolute destination path for this tool under the given scope root.
    fn dest(&self, scope_root: &Path) -> PathBuf {
        scope_root.join(self.dir).join(self.file)
    }
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

/// Tools whose convention directory already exists under `root` (best-effort
/// auto-detection for `agent add` with no tool argument).
fn detect_tools(root: &Path) -> Vec<&'static AgentTool> {
    TOOLS
        .iter()
        .filter(|t| {
            // The first path component (e.g. `.cursor`, `.github`) signals the tool.
            let marker = Path::new(t.dir).components().next();
            match marker {
                Some(c) => root.join(c.as_os_str()).is_dir(),
                None => false,
            }
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

/// `greplm agent add [tool]`.
pub fn add(tool: Option<&str>, project_root: &Path, global: bool, force: bool) -> Result<()> {
    let scope = scope_root(global, project_root)?;

    let targets: Vec<&AgentTool> = match tool {
        Some(key) => vec![find_tool(key)?],
        None => {
            let detected = detect_tools(project_root);
            if detected.is_empty() {
                bail!(
                    "no known tool directory found in {}. Specify one explicitly, e.g. \
                     `greplm agent add cursor` (see `greplm agent list`).",
                    project_root.display()
                );
            }
            detected
        }
    };

    let mut wrote_any = false;
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
    }

    if wrote_any {
        println!("restart your tool (or start a new session) so it picks up the agent.");
    }
    Ok(())
}

/// `greplm agent list`.
pub fn list(project_root: &Path, global: bool) -> Result<()> {
    let scope = scope_root(global, project_root)?;
    println!("{:<10} {:<16} DESTINATION", "TOOL", "NAME");
    for t in TOOLS {
        println!("{:<10} {:<16} {}", t.key, t.label, t.dest(&scope).display());
    }
    Ok(())
}
