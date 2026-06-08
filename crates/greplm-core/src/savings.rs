//! Token-savings accounting for agent queries.
//!
//! Every query an agent runs through greplm would otherwise have cost it a
//! `grep` (to find matches) followed by reading the matched files in full. We
//! record, per call, the size of that baseline (the unique files referenced in
//! the results) against the size of the compact payload greplm actually returns.
//! `greplm savings` then aggregates these into an estimate of tokens saved.
//!
//! Records are appended as JSON lines to `.greplm/savings.jsonl`. Accounting is
//! best-effort: any IO error is swallowed so it can never break a query. Set
//! `GREPLM_NO_SAVINGS=1` to disable recording entirely.

use std::collections::BTreeSet;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::paths::Paths;

/// Characters per token, matching the conservative estimate semble uses.
pub const CHARS_PER_TOKEN: u64 = 4;

const DAY: u64 = 86_400;

/// Trim the savings log once it grows past this size, so a long-running daemon
/// can't fill the disk one query-record at a time.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

/// When trimming, retain roughly this many of the most recent bytes (and thus
/// the recent rolling windows); only ancient all-time history is dropped.
const TRIM_KEEP_BYTES: usize = 4 * 1024 * 1024;

/// One recorded query.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    /// Unix timestamp (seconds).
    ts: u64,
    /// Query kind: "search", "symbols", "refs", "snippet", "semantic", ...
    kind: String,
    /// Number of results returned.
    results: u64,
    /// Full character count of the unique files referenced (grep+read baseline).
    baseline_chars: u64,
    /// Character count of the payload greplm returned to the agent.
    returned_chars: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn enabled() -> bool {
    !matches!(
        std::env::var("GREPLM_NO_SAVINGS").ok().as_deref(),
        Some("1") | Some("true")
    )
}

/// Record one query. `files` are the unique result paths (relative to the
/// project root); their on-disk sizes form the grep+read baseline. Best-effort:
/// errors are ignored.
pub fn record(
    paths: &Paths,
    kind: &str,
    files: &BTreeSet<String>,
    returned_chars: u64,
    results: u64,
) {
    if !enabled() || !paths.exists() {
        return;
    }
    let baseline_chars: u64 = files
        .iter()
        .filter_map(|f| std::fs::metadata(paths.root.join(f)).ok())
        .map(|m| m.len())
        .sum();
    let rec = Record {
        ts: now(),
        kind: kind.to_string(),
        results,
        baseline_chars,
        returned_chars,
    };
    let _ = append(paths, &rec);
}

fn append(paths: &Paths, rec: &Record) -> std::io::Result<()> {
    let path = paths.savings_file();
    maybe_trim(&path);
    let line = serde_json::to_string(rec).map_err(std::io::Error::other)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{line}")
}

/// Best-effort cap on the log file: once it exceeds [`MAX_LOG_BYTES`], rewrite
/// it keeping only the most recent ~[`TRIM_KEEP_BYTES`] worth of whole lines.
/// Any failure is ignored — accounting must never break a query.
fn maybe_trim(path: &std::path::Path) {
    let over = std::fs::metadata(path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false);
    if !over {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    // Keep the tail; align to a line boundary so we never emit a partial record.
    let start = text.len().saturating_sub(TRIM_KEEP_BYTES);
    let tail = match text[start..].find('\n') {
        Some(nl) if start > 0 => &text[start + nl + 1..],
        _ => &text[start..],
    };
    let _ = crate::fsutil::write_atomic(path, tail.as_bytes());
}

/// Savings rolled up over a single time window.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeriodSavings {
    pub label: String,
    pub calls: u64,
    pub baseline_chars: u64,
    pub returned_chars: u64,
}

impl PeriodSavings {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            calls: 0,
            baseline_chars: 0,
            returned_chars: 0,
        }
    }

    fn add(&mut self, rec: &Record) {
        self.calls += 1;
        self.baseline_chars += rec.baseline_chars;
        self.returned_chars += rec.returned_chars;
    }

    /// Estimated tokens saved versus grep+read (never negative).
    pub fn tokens_saved(&self) -> u64 {
        self.baseline_chars.saturating_sub(self.returned_chars) / CHARS_PER_TOKEN
    }

    /// Baseline tokens an agent would have spent reading matched files in full.
    pub fn baseline_tokens(&self) -> u64 {
        self.baseline_chars / CHARS_PER_TOKEN
    }

    /// Fraction of baseline tokens saved, in `[0, 1]`.
    pub fn ratio(&self) -> f64 {
        if self.baseline_chars == 0 {
            return 0.0;
        }
        (self.baseline_chars.saturating_sub(self.returned_chars)) as f64
            / self.baseline_chars as f64
    }
}

/// Aggregated savings report.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavingsReport {
    /// Rolling windows: last 24h, last 7 days, all time.
    pub periods: Vec<PeriodSavings>,
    /// All-time breakdown by query kind.
    pub by_kind: Vec<PeriodSavings>,
}

/// Read the log and aggregate it into rolling windows and a per-kind breakdown.
pub fn report(paths: &Paths) -> SavingsReport {
    let mut day = PeriodSavings::new("Last 24h");
    let mut week = PeriodSavings::new("Last 7 days");
    let mut all = PeriodSavings::new("All time");
    let mut kinds: std::collections::BTreeMap<String, PeriodSavings> = Default::default();

    let now = now();
    if let Ok(text) = std::fs::read_to_string(paths.savings_file()) {
        for line in text.lines() {
            let rec: Record = match serde_json::from_str(line) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if now.saturating_sub(rec.ts) <= DAY {
                day.add(&rec);
            }
            if now.saturating_sub(rec.ts) <= 7 * DAY {
                week.add(&rec);
            }
            all.add(&rec);
            kinds
                .entry(rec.kind.clone())
                .or_insert_with(|| PeriodSavings::new(&rec.kind))
                .add(&rec);
        }
    }

    let mut by_kind: Vec<PeriodSavings> = kinds.into_values().collect();
    by_kind.sort_by_key(|p| std::cmp::Reverse(p.tokens_saved()));
    SavingsReport {
        periods: vec![day, week, all],
        by_kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> TempRoot {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let mut p = std::env::temp_dir();
            p.push(format!("greplm-savings-{tag}-{nanos}"));
            std::fs::create_dir_all(p.join(crate::paths::DIR_NAME)).unwrap();
            TempRoot(p)
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // Single test because `GREPLM_NO_SAVINGS` is process-global; splitting would
    // let the enable/disable cases race under the parallel test runner.
    #[test]
    fn records_aggregates_and_respects_disable_flag() {
        let tmp = TempRoot::new("agg");
        let paths = Paths::new(&tmp.0);

        // A 4000-byte file is the grep+read baseline; a tiny returned payload.
        std::fs::write(paths.root.join("big.rs"), vec![b'x'; 4000]).unwrap();
        let files: BTreeSet<String> = ["big.rs".to_string()].into_iter().collect();
        record(&paths, "search", &files, 80, 1);

        let rep = report(&paths);
        let all = rep.periods.last().unwrap();
        assert_eq!(all.calls, 1);
        assert_eq!(all.baseline_chars, 4000);
        assert_eq!(all.returned_chars, 80);
        // (4000 - 80) / 4 = 980 tokens saved.
        assert_eq!(all.tokens_saved(), 980);
        assert!((all.ratio() - 0.98).abs() < 1e-6);
        assert_eq!(rep.by_kind.len(), 1);
        assert_eq!(rep.by_kind[0].label, "search");

        // The disable flag suppresses further recording.
        std::env::set_var("GREPLM_NO_SAVINGS", "1");
        record(&paths, "search", &files, 80, 1);
        std::env::remove_var("GREPLM_NO_SAVINGS");
        assert_eq!(report(&paths).periods.last().unwrap().calls, 1);
    }
}
