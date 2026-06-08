//! Operational subcommands that keep a greplm install healthy:
//! `doctor` (diagnose + auto-fix), `update` (self-update), and `setup`
//! (index + install the always-on daemon as a background service).
//!
//! Network/service actions shell out to tools the user already has
//! (`curl`, `launchctl`, `systemctl`) so the binary stays dependency-light.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use greplm_core::client::Client;
use greplm_core::proto::{global_socket_path, Request};
use greplm_core::Greplm;

const REPO: &str = "KhaledSMQ/greplm";
const INSTALL_URL: &str = "https://raw.githubusercontent.com/KhaledSMQ/greplm/main/install.sh";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

// ---- status helpers --------------------------------------------------------

/// Which daemon, if any, is reachable to serve a project.
enum DaemonKind {
    Global,
    PerProject,
    None,
}

fn daemon_kind(g: &Greplm) -> DaemonKind {
    if let Some(mut c) = Client::try_connect(&global_socket_path()) {
        if c.request(&Request::Ping).is_ok() {
            return DaemonKind::Global;
        }
    }
    if let Some(mut c) = Client::try_connect(&g.socket_path()) {
        if c.request(&Request::Ping).is_ok() {
            return DaemonKind::PerProject;
        }
    }
    DaemonKind::None
}

fn ok(msg: &str) {
    println!("  \u{2713} {msg}");
}
fn warn(msg: &str) {
    println!("  \u{26a0} {msg}");
}
fn fail(msg: &str) {
    println!("  \u{2717} {msg}");
}
fn note(msg: &str) {
    println!("  \u{00b7} {msg}");
}

// ---- doctor ----------------------------------------------------------------

/// Diagnose common problems and, with `fix`, repair the safe ones (build/refresh
/// a missing or stale index, install the daemon service).
pub fn doctor(root: &Path, fix: bool) -> Result<()> {
    println!(
        "greplm doctor  (v{CURRENT_VERSION}){}\n",
        if fix { "  [--fix]" } else { "" }
    );
    let mut problems = 0usize;
    let mut fixed = 0usize;

    // 1. Up to date?
    match latest_version() {
        Ok(latest) if is_newer(&latest, CURRENT_VERSION) => {
            warn(&format!(
                "a newer version {latest} is available (you have v{CURRENT_VERSION})"
            ));
            println!("      fix: greplm update");
            problems += 1;
        }
        Ok(_) => ok("greplm is up to date"),
        Err(_) => note("skipped update check (offline, or curl unavailable)"),
    }

    // 2. Project + index.
    let g = Greplm::discover(root)?;
    println!("  project: {}", g.root().display());
    match g.status() {
        Ok(s) if s.indexed => {
            ok(&format!(
                "index present ({} files, {} symbols)",
                s.doc_count, s.symbol_count
            ));
            match g.is_dirty() {
                Ok(true) if fix => {
                    g.index(false)?;
                    ok("index was stale \u{2014} refreshed");
                    fixed += 1;
                }
                Ok(true) => {
                    warn("index is stale (files changed since last index)");
                    println!("      fix: greplm index   (or rerun doctor with --fix)");
                    problems += 1;
                }
                Ok(false) => ok("index is fresh"),
                Err(e) => note(&format!("could not check freshness: {e}")),
            }
        }
        Ok(_) if fix => {
            g.index(false)?;
            ok("no index \u{2014} built one");
            fixed += 1;
        }
        Ok(_) => {
            warn("no index for this project");
            println!("      fix: greplm index   (or rerun doctor with --fix)");
            problems += 1;
        }
        Err(e) if fix => {
            g.index(true)?;
            ok(&format!("index was unreadable ({e}) \u{2014} rebuilt"));
            fixed += 1;
        }
        Err(e) => {
            fail(&format!("index unreadable: {e}"));
            println!("      fix: greplm index --force   (or rerun doctor with --fix)");
            problems += 1;
        }
    }

    // 3. Warm daemon.
    match daemon_kind(&g) {
        DaemonKind::Global => ok("global daemon running (warm; sub-ms queries for every project)"),
        DaemonKind::PerProject => {
            ok("per-project daemon running");
            note("tip: a global daemon (`greplm setup`) covers all projects from one process");
        }
        DaemonKind::None if fix => match install_global_service() {
            Ok(()) => {
                ok("installed always-on global daemon service");
                fixed += 1;
            }
            Err(e) => {
                warn(&format!("no warm daemon, and auto-install failed: {e}"));
                println!("      fix: greplm serve --global   (run it yourself)");
                problems += 1;
            }
        },
        DaemonKind::None => {
            warn("no warm daemon running \u{2014} queries cold-open the index (~25ms each)");
            println!("      fix: greplm setup   (builds the index + installs an always-on daemon)");
            problems += 1;
        }
    }

    println!();
    if problems == 0 {
        println!(
            "\u{2713} healthy{}",
            if fixed > 0 {
                format!(" ({fixed} fixed)")
            } else {
                String::new()
            }
        );
    } else {
        println!(
            "{problems} issue(s) remaining{}.",
            if fixed > 0 {
                format!(", {fixed} fixed")
            } else {
                String::new()
            }
        );
    }
    Ok(())
}

// ---- setup -----------------------------------------------------------------

/// First-run convenience: build the index and install the always-on global
/// daemon so agents get warm, fresh results with nothing else to configure.
pub fn setup(root: &Path, install_service: bool) -> Result<()> {
    let g = Greplm::open(root)?;
    g.ensure_initialized()?;
    let built = g.ensure_indexed()?;
    println!(
        "{} index for {}",
        if built { "built" } else { "verified" },
        g.root().display()
    );
    if install_service {
        install_global_service()?;
    } else {
        note("skipped daemon service (--no-daemon-service); run `greplm serve --global` to start one");
    }
    println!("done \u{2014} queries and the MCP server will auto-route to the warm daemon.");
    Ok(())
}

// ---- update ----------------------------------------------------------------

/// Self-update via the official install script. With `check_only`, just report.
pub fn update(check_only: bool) -> Result<()> {
    let latest = latest_version()
        .context("could not check the latest version (needs network access and curl)")?;
    println!("current: v{CURRENT_VERSION}    latest: {latest}");
    if !is_newer(&latest, CURRENT_VERSION) {
        println!("already up to date.");
        return Ok(());
    }
    if check_only {
        println!("a newer version is available; run `greplm update` to install it.");
        return Ok(());
    }
    println!("updating via {INSTALL_URL} ...");
    let status = Command::new("sh")
        .arg("-c")
        .arg(format!("curl -fsSL {INSTALL_URL} | sh"))
        .status()
        .context("running the installer")?;
    if !status.success() {
        bail!("update failed (installer exited non-zero)");
    }
    println!("updated to {latest}. Restart the daemon to run the new binary:");
    println!("  pkill -f 'greplm serve'   # a launchd/systemd service relaunches automatically");
    Ok(())
}

/// Latest release tag from the GitHub API (via `curl`).
fn latest_version() -> Result<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let out = Command::new("curl")
        .args(["-fsSL", "-H", "User-Agent: greplm", &url])
        .output()
        .context("invoking curl")?;
    if !out.status.success() {
        bail!("GitHub API request failed");
    }
    let body = String::from_utf8_lossy(&out.stdout);
    body.split("\"tag_name\"")
        .nth(1)
        .and_then(|s| s.split('"').nth(1))
        .map(|s| s.to_string())
        .context("no tag_name in GitHub response")
}

/// Compare dotted versions (ignoring a leading `v` and any pre-release suffix).
fn is_newer(latest: &str, current: &str) -> bool {
    ver_tuple(latest) > ver_tuple(current)
}

fn ver_tuple(s: &str) -> (u64, u64, u64) {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split(['.', '-', '+']);
    let next =
        |it: &mut std::str::Split<[char; 3]>| it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    (next(&mut it), next(&mut it), next(&mut it))
}

// ---- service install -------------------------------------------------------

fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("cannot resolve the greplm binary path")
}

/// Install the global daemon as a per-user background service that starts at
/// login and restarts on crash. macOS (launchd) and Linux (systemd) only.
pub fn install_global_service() -> Result<()> {
    let exe = current_exe()?;
    #[cfg(target_os = "macos")]
    {
        return install_launchd(&exe);
    }
    #[cfg(target_os = "linux")]
    {
        return install_systemd(&exe);
    }
    #[allow(unreachable_code)]
    {
        let _ = exe;
        bail!(
            "automatic service install is supported on macOS and Linux only; \
             run `greplm serve --global` (e.g. from your init system) instead"
        )
    }
}

#[cfg(target_os = "macos")]
fn install_launchd(exe: &Path) -> Result<()> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let agents = format!("{home}/Library/LaunchAgents");
    let log = format!("{home}/Library/Logs/greplm-global.log");
    std::fs::create_dir_all(&agents).ok();
    if let Some(p) = Path::new(&log).parent() {
        std::fs::create_dir_all(p).ok();
    }
    let label = "com.greplm.global";
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>serve</string><string>--global</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
  <key>EnvironmentVariables</key><dict><key>GREPLM_LOG</key><string>info</string></dict>
</dict>
</plist>
"#,
        exe = exe.display()
    );
    let path = format!("{agents}/{label}.plist");
    std::fs::write(&path, plist).with_context(|| format!("writing {path}"))?;
    let _ = Command::new("launchctl").args(["unload", &path]).status();
    let st = Command::new("launchctl")
        .args(["load", &path])
        .status()
        .context("launchctl load")?;
    if !st.success() {
        bail!("launchctl load failed for {path}");
    }
    println!("installed launchd service {label}");
    println!("  plist: {path}");
    println!("  logs : {log}");
    println!("  stop : launchctl unload \"{path}\" && rm \"{path}\"");
    Ok(())
}

#[cfg(target_os = "linux")]
fn install_systemd(exe: &Path) -> Result<()> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let dir = format!("{home}/.config/systemd/user");
    std::fs::create_dir_all(&dir).ok();
    let unit = format!(
        "[Unit]\nDescription=greplm global warm-index daemon (all projects)\nAfter=default.target\n\n\
         [Service]\nType=simple\nExecStart={exe} serve --global\nRestart=always\nRestartSec=10\n\
         Environment=GREPLM_LOG=info\n\n[Install]\nWantedBy=default.target\n",
        exe = exe.display()
    );
    let path = format!("{dir}/greplm-global.service");
    std::fs::write(&path, unit).with_context(|| format!("writing {path}"))?;
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let st = Command::new("systemctl")
        .args(["--user", "enable", "--now", "greplm-global.service"])
        .status()
        .context("systemctl enable")?;
    if !st.success() {
        bail!("systemctl enable --now greplm-global.service failed");
    }
    println!("installed systemd user service greplm-global.service");
    println!("  unit: {path}");
    println!("  stop: systemctl --user disable --now greplm-global.service");
    Ok(())
}
