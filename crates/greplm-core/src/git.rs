//! Git time-travel intelligence.
//!
//! Lightweight, on-demand history queries backed by the `git` CLI: line blame,
//! the commit history of a symbol's line range, and what changed since a
//! revision. Nothing here is stored in the index — keeping indexing fast — but
//! [`head`] records the current commit so callers can detect a branch switch.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// One blamed line: the commit and author that last touched it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlameLine {
    pub path: String,
    pub line: u32,
    pub commit: String,
    pub author: String,
    /// Author time, unix seconds.
    pub author_time: u64,
    pub summary: String,
    pub content: String,
}

/// One commit in a history listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub commit: String,
    pub author: String,
    pub author_time: u64,
    pub summary: String,
}

/// A path changed relative to a revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    /// Single-letter git status (M, A, D, R, ...).
    pub status: String,
}

/// Run `git` in `root` and return stdout on success.
fn git(root: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| Error::other(format!("failed to run git: {e}")))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(Error::other(format!(
            "git {} failed: {}",
            args.join(" "),
            err.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// True if `root` is inside a git work tree.
pub fn is_repo(root: &Path) -> bool {
    git(root, &["rev-parse", "--is-inside-work-tree"])
        .map(|s| s.trim() == "true")
        .unwrap_or(false)
}

/// The current commit sha and branch name, if `root` is a repo.
pub fn head(root: &Path) -> Option<(String, String)> {
    let sha = git(root, &["rev-parse", "HEAD"]).ok()?.trim().to_string();
    let branch = git(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    Some((sha, branch))
}

/// Blame a single 1-based line of `rel_path`.
pub fn blame(root: &Path, rel_path: &str, line: u32) -> Result<BlameLine> {
    let range = format!("{line},{line}");
    let out = git(
        root,
        &["blame", "-L", &range, "--porcelain", "--", rel_path],
    )?;
    parse_blame(&out, rel_path, line)
        .ok_or_else(|| Error::other(format!("could not blame {rel_path}:{line}")))
}

fn parse_blame(out: &str, rel_path: &str, line: u32) -> Option<BlameLine> {
    let mut commit = String::new();
    let mut author = String::new();
    let mut author_time = 0u64;
    let mut summary = String::new();
    let mut content = String::new();
    for (i, l) in out.lines().enumerate() {
        if i == 0 {
            commit = l.split_whitespace().next().unwrap_or("").to_string();
        } else if let Some(rest) = l.strip_prefix("author ") {
            author = rest.to_string();
        } else if let Some(rest) = l.strip_prefix("author-time ") {
            author_time = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = l.strip_prefix("summary ") {
            summary = rest.to_string();
        } else if let Some(rest) = l.strip_prefix('\t') {
            content = rest.to_string();
        }
    }
    if commit.is_empty() {
        return None;
    }
    Some(BlameLine {
        path: rel_path.to_string(),
        line,
        commit: short_sha(&commit),
        author,
        author_time,
        summary,
        content,
    })
}

/// Commits that touched lines `[start, end]` of `rel_path`, newest first.
pub fn line_history(
    root: &Path,
    rel_path: &str,
    start: u32,
    end: u32,
    limit: usize,
) -> Result<Vec<Commit>> {
    let lspec = format!("{start},{end}:{rel_path}");
    let out = git(
        root,
        &[
            "log",
            "-L",
            &lspec,
            "--no-patch",
            &format!("--max-count={limit}"),
            "--format=%H%x09%an%x09%at%x09%s",
        ],
    )?;
    Ok(parse_commits(&out))
}

/// Commits that touched `rel_path`, newest first.
pub fn file_history(root: &Path, rel_path: &str, limit: usize) -> Result<Vec<Commit>> {
    let out = git(
        root,
        &[
            "log",
            &format!("--max-count={limit}"),
            "--format=%H%x09%an%x09%at%x09%s",
            "--",
            rel_path,
        ],
    )?;
    Ok(parse_commits(&out))
}

fn parse_commits(out: &str) -> Vec<Commit> {
    let mut commits = Vec::new();
    for l in out.lines() {
        let parts: Vec<&str> = l.splitn(4, '\t').collect();
        if parts.len() == 4 {
            commits.push(Commit {
                commit: short_sha(parts[0]),
                author: parts[1].to_string(),
                author_time: parts[2].trim().parse().unwrap_or(0),
                summary: parts[3].to_string(),
            });
        }
    }
    commits
}

/// Files changed relative to `rev` (e.g. a branch, tag, or `HEAD~5`).
pub fn changed_since(root: &Path, rev: &str) -> Result<Vec<ChangedFile>> {
    let out = git(root, &["diff", "--name-status", rev, "--"])?;
    let mut files = Vec::new();
    for l in out.lines() {
        let mut it = l.split('\t');
        let status = it.next().unwrap_or("").to_string();
        // For renames git emits "R100\told\tnew"; take the final path.
        let path = it.next_back().unwrap_or("").to_string();
        if !path.is_empty() {
            files.push(ChangedFile {
                path,
                status: status.chars().next().map(String::from).unwrap_or_default(),
            });
        }
    }
    Ok(files)
}

fn short_sha(sha: &str) -> String {
    sha.chars().take(12).collect()
}
