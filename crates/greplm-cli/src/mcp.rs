//! `greplm mcp` — emit ready-to-paste MCP client configuration.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::onboard;

/// Names to look for when resolving the `greplm-mcp` binary.
fn mcp_binary_names() -> &'static [&'static str] {
    if cfg!(windows) {
        &["greplm-mcp.exe", "greplm-mcp"]
    } else {
        &["greplm-mcp"]
    }
}

/// Resolve the `greplm-mcp` binary: sibling of this executable, then `$PATH`.
pub fn resolve_mcp_binary() -> Result<PathBuf> {
    let greplm = std::env::current_exe().context("cannot resolve greplm binary path")?;
    if let Some(dir) = greplm.parent() {
        for name in mcp_binary_names() {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(std::fs::canonicalize(&candidate).unwrap_or(candidate));
            }
        }
    }
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in mcp_binary_names() {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Ok(std::fs::canonicalize(&candidate).unwrap_or(candidate));
                }
            }
        }
    }
    bail!(
        "greplm-mcp not found next to {} or on PATH; reinstall greplm (it ships both binaries)",
        greplm.display()
    )
}

#[derive(Serialize)]
struct McpServerEntry {
    command: String,
    args: Vec<String>,
}

#[derive(Serialize)]
struct McpConfig {
    #[serde(rename = "mcpServers")]
    mcp_servers: std::collections::BTreeMap<String, McpServerEntry>,
}

/// Build the MCP client JSON for `root`.
pub fn config_json(root: &Path, pretty: bool) -> Result<String> {
    let mcp = resolve_mcp_binary()?;
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut servers = std::collections::BTreeMap::new();
    servers.insert(
        "greplm".to_string(),
        McpServerEntry {
            command: mcp.display().to_string(),
            args: vec![root.display().to_string()],
        },
    );
    let cfg = McpConfig {
        mcp_servers: servers,
    };
    if pretty {
        Ok(serde_json::to_string_pretty(&cfg)?)
    } else {
        Ok(serde_json::to_string(&cfg)?)
    }
}

/// Print MCP JSON to stdout and, unless `quiet`, paste hints to stderr.
pub fn print_config(root: &Path, pretty: bool, quiet: bool) -> Result<()> {
    let json = config_json(root, pretty)?;
    if !quiet {
        onboard::print_mcp_hints();
    }
    println!("{json}");
    Ok(())
}
