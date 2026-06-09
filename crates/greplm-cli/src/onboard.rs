//! Post-install / first-run terminal guidance.

use std::io::{IsTerminal, Write};
use std::path::Path;

const WIDTH: usize = 58;

/// Lightweight ANSI styling — only when the stream is a TTY and `NO_COLOR` is unset.
struct Term {
    color: bool,
}

impl Term {
    fn stdout() -> Self {
        Self::new(std::io::stdout().is_terminal())
    }

    fn stderr() -> Self {
        Self::new(std::io::stderr().is_terminal())
    }

    fn new(is_tty: bool) -> Self {
        let no_color = std::env::var("NO_COLOR").is_ok();
        Self {
            color: is_tty && !no_color,
        }
    }

    fn wrap(&self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }

    fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }

    fn dim(&self, s: &str) -> String {
        self.wrap("2", s)
    }

    fn cyan(&self, s: &str) -> String {
        self.wrap("36", s)
    }

    fn green(&self, s: &str) -> String {
        self.wrap("32", s)
    }

    fn yellow(&self, s: &str) -> String {
        self.wrap("33", s)
    }
}

fn pad_line(inner: &str) -> String {
    let visible = strip_ansi(inner);
    let pad = WIDTH.saturating_sub(visible.len());
    format!("  │ {inner}{:pad$} │", "", pad = pad)
}

fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some('[') {
                for ch in chars.by_ref() {
                    if ch.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

fn print_box(title: &str, subtitle: Option<&str>, t: &Term) {
    let top = format!("  ╭{}╮", "─".repeat(WIDTH + 2));
    let bot = format!("  ╰{}╯", "─".repeat(WIDTH + 2));
    println!();
    println!("{top}");
    println!("{}", pad_line(""));
    println!("{}", pad_line(&format!("  {}", t.bold(&t.cyan(title)))));
    if let Some(sub) = subtitle {
        println!("{}", pad_line(&format!("  {}", t.dim(sub))));
    }
    println!("{}", pad_line(""));
    println!("{bot}");
}

fn print_step(num: &str, title: &str, cmd: &str, hint: &str, t: &Term) {
    println!();
    println!(
        "  {}  {}",
        t.yellow(num),
        t.bold(title)
    );
    println!(
        "     {}",
        t.green(&format!("$ {cmd}"))
    );
    if !hint.is_empty() {
        println!("     {}", t.dim(hint));
    }
}

fn print_kv(key: &str, value: &str, t: &Term) {
    println!(
        "  {:<18}{}",
        t.dim(&format!("{key}:")),
        t.cyan(value)
    );
}

/// Compact success line after `greplm setup` finishes indexing / daemon work.
pub fn print_setup_summary(built: bool, root: &Path) {
    let t = Term::stdout();
    let action = if built { "Indexed" } else { "Verified index for" };
    println!();
    println!(
        "  {}  {} {}",
        t.green("✓"),
        t.bold(action),
        t.cyan(&root.display().to_string())
    );
    println!(
        "  {}  {}",
        t.green("✓"),
        t.dim("Warm daemon ready — queries route automatically")
    );
}

/// Friendly next-steps banner after `setup` or `greplm welcome`.
pub fn print_next_steps(root: &Path) {
    let t = Term::stdout();
    print_box(
        "greplm",
        Some("code search & intelligence for the agent loop"),
        &t,
    );

    println!();
    println!("  {}", t.bold("Get started in 3 steps"));

    print_step(
        "①",
        "Connect your AI editor",
        "greplm mcp config",
        "paste JSON → .cursor/mcp.json · Claude · VS Code",
        &t,
    );
    print_step(
        "②",
        "Teach your editor to use greplm",
        "greplm agent add",
        "auto-detects Cursor, Claude, Copilot, Gemini, …",
        &t,
    );
    print_step(
        "③",
        "Search this project",
        "greplm search \"your query\"",
        "or greplm pack \"your task\" --budget 8000",
        &t,
    );

    println!();
    let rule = "─".repeat(WIDTH + 6);
    println!("  {}", t.dim(&rule));
    println!();
    print_kv("Health check", "greplm doctor", &t);
    print_kv("Show this again", "greplm welcome", &t);
    print_kv("Project", &root.display().to_string(), &t);
    println!();
}

/// Hints printed above MCP JSON on stderr (`greplm mcp config`).
pub fn print_mcp_hints() {
    let t = Term::stderr();
    let mut out = std::io::stderr();
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  {}",
        t.bold("Copy the JSON below into your editor's MCP settings")
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  {}  {}",
        t.yellow("Cursor"),
        t.dim(".cursor/mcp.json  ·  Cursor Settings → MCP")
    );
    let _ = writeln!(
        out,
        "  {}  {}",
        t.yellow("Claude"),
        t.dim("~/Library/Application Support/Claude/claude_desktop_config.json")
    );
    let _ = writeln!(
        out,
        "  {}  {}",
        t.yellow("VS Code"),
        t.dim(".vscode/mcp.json")
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  {}  {}",
        t.dim("Also run:"),
        t.green("greplm agent add")
    );
    let _ = writeln!(out);
}
