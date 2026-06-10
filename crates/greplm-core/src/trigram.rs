//! Trigram extraction and query decomposition.
//!
//! A trigram is a 3-byte sequence. The index maps each trigram to the set of
//! documents that contain it. A query is satisfiable only in documents that
//! contain *every* trigram of the query literal, so we can intersect posting
//! lists to get a small candidate set before verifying with the real matcher.

/// A trigram, stored big-endian so byte order matches numeric order (required
/// for the FST term dictionary, whose keys must be lexicographically sorted).
pub type Trigram = [u8; 3];

/// One AND-group of trigrams: a document must contain every member.
pub type TrigramGroup = Vec<Trigram>;

/// A disjunction of AND-groups (disjunctive normal form): a document passes
/// when at least one group is fully present.
pub type TrigramDnf = Vec<TrigramGroup>;

pub(crate) fn key_of(w: &[u8]) -> u32 {
    (u32::from(w[0]) << 16) | (u32::from(w[1]) << 8) | u32::from(w[2])
}

pub(crate) fn tri_of(key: u32) -> Trigram {
    [(key >> 16) as u8, (key >> 8) as u8, key as u8]
}

std::thread_local! {
    /// A 2^24-bit membership set (2 MiB) used to deduplicate trigram keys
    /// *during* the scan. Reused across calls on the same thread; after each
    /// use the set bits are cleared by replaying the distinct-key list, so
    /// reset costs O(distinct) instead of a 2 MiB memset per file.
    static SEEN: std::cell::RefCell<Box<[u64]>> =
        std::cell::RefCell::new(vec![0u64; 1 << 18].into_boxed_slice());
}

/// Extract the distinct trigrams present in `data`, sorted ascending.
///
/// Each 3-byte window is encoded as a `u32` key and deduplicated on the fly
/// against a thread-local bitset, so the scan is O(n) and the subsequent sort
/// runs over *distinct* keys only (typically 10-50x fewer than windows for
/// source code). This replaces sorting every window (O(n log n) with a
/// transient allocation of 4 bytes per input byte). The big-endian encoding
/// means the sorted order is exactly the lexicographic order the FST term
/// dictionary requires.
pub fn extract(data: &[u8]) -> Vec<Trigram> {
    if data.len() < 3 {
        return Vec::new();
    }
    SEEN.with(|seen| {
        let mut seen = seen.borrow_mut();
        let mut keys: Vec<u32> = Vec::new();
        for w in data.windows(3) {
            let k = key_of(w);
            let word = (k >> 6) as usize;
            let bit = 1u64 << (k & 63);
            if seen[word] & bit == 0 {
                seen[word] |= bit;
                keys.push(k);
            }
        }
        // Every set bit corresponds to exactly one pushed key, so zeroing each
        // key's word clears the whole set (idempotent for keys sharing a word).
        for &k in &keys {
            seen[(k >> 6) as usize] = 0;
        }
        keys.sort_unstable();
        keys.into_iter().map(tri_of).collect()
    })
}

/// Extract the trigrams of a literal needle, sorted and deduplicated. Returns an
/// empty vec when the needle is shorter than 3 bytes (meaning: trigram filtering
/// can't help and the caller must scan all candidates).
pub fn literal_trigrams(needle: &[u8]) -> Vec<Trigram> {
    extract(needle)
}

/// A boolean query over trigrams: a conjunction of DNFs. A document is a
/// candidate when it satisfies *every* DNF, where a DNF is satisfied when at
/// least one of its AND-groups is fully present in the document.
///
/// This one shape expresses everything the planner produces:
///
/// * an exact literal is one DNF with a single AND-group (all its trigrams);
/// * a case-insensitive literal contributes one DNF per needle window, each a
///   disjunction of single-trigram groups (the window's fold variants);
/// * a regex contributes a DNF for its required prefix literals and another
///   for its required suffix literals.
///
/// A DNF that cannot filter (it is empty, or contains an empty group, which
/// would make it trivially true) is ignored. An empty query means "scan
/// everything".
#[derive(Debug, Default, Clone)]
pub struct TrigramQuery {
    pub dnfs: Vec<TrigramDnf>,
}

impl TrigramQuery {
    /// True when no usable trigram constraints exist and all documents are
    /// candidates.
    pub fn is_unconstrained(&self) -> bool {
        !self.dnfs.iter().any(dnf_filters)
    }

    pub fn from_literal(needle: &[u8]) -> TrigramQuery {
        let tris = literal_trigrams(needle);
        if tris.is_empty() {
            TrigramQuery::default()
        } else {
            TrigramQuery {
                dnfs: vec![vec![tris]],
            }
        }
    }

    /// Build a case-insensitive literal query. Each 3-byte window of the needle
    /// becomes one DNF listing every byte sequence the window can begin with in
    /// a match, so the trigram index can still prune candidates without false
    /// negatives.
    ///
    /// The matcher folds case Unicode-aware, so a window's variants are not
    /// just its ASCII case permutations: `s`/`S` also matches U+017F (LATIN
    /// SMALL LETTER LONG S) and `k`/`K` also matches U+212A (KELVIN SIGN),
    /// whose UTF-8 encodings are multi-byte. For each window we enumerate every
    /// combination of per-character fold forms and take the first three bytes
    /// of each — exactly the set of trigrams a match of that window can start
    /// with. Windows containing non-ASCII needle bytes are skipped (their fold
    /// forms aren't enumerable this way), which only widens the candidate set.
    pub fn from_literal_ci(needle: &[u8]) -> TrigramQuery {
        if needle.len() < 3 {
            return TrigramQuery::default();
        }
        let mut dnfs: Vec<TrigramDnf> = Vec::new();
        for w in needle.windows(3) {
            if let Some(clause) = ci_window_trigrams([w[0], w[1], w[2]]) {
                dnfs.push(clause.into_iter().map(|t| vec![t]).collect());
            }
        }
        if dnfs.iter().any(dnf_filters) {
            TrigramQuery { dnfs }
        } else {
            TrigramQuery::default()
        }
    }
}

/// True when a DNF actually constrains matching: it has at least one group and
/// no empty group (an empty group is trivially satisfied, disabling the DNF).
pub fn dnf_filters(dnf: &TrigramDnf) -> bool {
    !dnf.is_empty() && dnf.iter().all(|g| !g.is_empty())
}

/// A fold form: up to 3 UTF-8 bytes plus its length. Stack-only so query
/// planning stays allocation-free per character.
type FoldForm = ([u8; 3], usize);

/// The byte sequences a single needle byte can match under the matcher's
/// case-insensitive (Unicode simple fold) semantics, or `None` when they are
/// not enumerable (non-ASCII bytes, whose folded forms shift window
/// alignment unpredictably). Returns the number of forms written to `out`.
fn fold_forms(b: u8, out: &mut [FoldForm; 3]) -> Option<usize> {
    if b >= 0x80 {
        return None;
    }
    if !b.is_ascii_alphabetic() {
        out[0] = ([b, 0, 0], 1);
        return Some(1);
    }
    out[0] = ([b.to_ascii_lowercase(), 0, 0], 1);
    out[1] = ([b.to_ascii_uppercase(), 0, 0], 1);
    match b.to_ascii_lowercase() {
        // U+017F LATIN SMALL LETTER LONG S folds to 's'.
        b's' => {
            out[2] = ([0xC5, 0xBF, 0], 2);
            Some(3)
        }
        // U+212A KELVIN SIGN folds to 'k'.
        b'k' => {
            out[2] = ([0xE2, 0x84, 0xAA], 3);
            Some(3)
        }
        _ => Some(2),
    }
}

/// All trigrams a case-insensitive match of window `w` can begin with: for
/// every combination of per-character fold forms, the first three bytes of the
/// concatenation (each form is >= 1 byte, so three forms always cover a
/// trigram). At most 3^3 = 27 combinations; deduplicated and sorted.
fn ci_window_trigrams(w: Trigram) -> Option<Vec<Trigram>> {
    let mut forms = [[([0u8; 3], 0usize); 3]; 3];
    let mut counts = [0usize; 3];
    for i in 0..3 {
        counts[i] = fold_forms(w[i], &mut forms[i])?;
    }
    let mut out: Vec<Trigram> = Vec::with_capacity(counts[0] * counts[1] * counts[2]);
    let mut buf = [0u8; 9];
    for a in &forms[0][..counts[0]] {
        for b in &forms[1][..counts[1]] {
            for c in &forms[2][..counts[2]] {
                buf[..a.1].copy_from_slice(&a.0[..a.1]);
                buf[a.1..a.1 + b.1].copy_from_slice(&b.0[..b.1]);
                buf[a.1 + b.1..a.1 + b.1 + c.1].copy_from_slice(&c.0[..c.1]);
                out.push([buf[0], buf[1], buf[2]]);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    Some(out)
}

/// Build a trigram query from a regular expression by extracting required
/// literal substrings: the prefixes any match must start with *and* the
/// suffixes any match must end with, each contributing an independent DNF.
/// If neither side yields usable literals we fall back to an unconstrained
/// query (scan all candidates).
pub fn regex_trigrams(pattern: &str, case_insensitive: bool) -> TrigramQuery {
    use regex_syntax::hir::literal::{ExtractKind, Extractor};
    use regex_syntax::ParserBuilder;

    let hir = match ParserBuilder::new()
        .case_insensitive(case_insensitive)
        .build()
        .parse(pattern)
    {
        Ok(h) => h,
        Err(_) => return TrigramQuery::default(),
    };

    /// Turn an extracted literal sequence into a DNF (one AND-group per
    /// literal). Returns `None` when any literal is too short to filter,
    /// since requiring the remaining groups could drop real matches.
    fn dnf_of(seq: &regex_syntax::hir::literal::Seq) -> Option<TrigramDnf> {
        let lits = seq.literals()?;
        let mut dnf: TrigramDnf = Vec::with_capacity(lits.len());
        for lit in lits {
            let tris = literal_trigrams(lit.as_bytes());
            if tris.is_empty() {
                return None;
            }
            dnf.push(tris);
        }
        if dnf.is_empty() {
            None
        } else {
            Some(dnf)
        }
    }

    let prefix = dnf_of(&Extractor::new().extract(&hir));
    let suffix = {
        let mut ex = Extractor::new();
        ex.kind(ExtractKind::Suffix);
        dnf_of(&ex.extract(&hir))
    };

    let mut dnfs: Vec<TrigramDnf> = Vec::new();
    if let Some(p) = prefix {
        dnfs.push(p);
    }
    if let Some(s) = suffix {
        // A fully literal pattern yields identical prefix and suffix sets;
        // evaluating the same DNF twice would just double posting work.
        if dnfs.first() != Some(&s) {
            dnfs.push(s);
        }
    }
    if dnfs.is_empty() {
        TrigramQuery::default()
    } else {
        TrigramQuery { dnfs }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_basic() {
        let set = extract(b"abcd");
        assert!(set.contains(b"abc"));
        assert!(set.contains(b"bcd"));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn extract_is_sorted_and_deduped() {
        let set = extract(b"abcabcabc");
        let mut sorted = set.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(set, sorted, "extract must return sorted, distinct trigrams");
    }

    #[test]
    fn short_input_has_no_trigrams() {
        assert!(extract(b"ab").is_empty());
        assert!(literal_trigrams(b"ab").is_empty());
    }

    #[test]
    fn literal_query_is_constrained() {
        let q = TrigramQuery::from_literal(b"function");
        assert!(!q.is_unconstrained());
        // Short literals cannot use the trigram filter.
        assert!(TrigramQuery::from_literal(b"fn").is_unconstrained());
    }

    #[test]
    fn regex_extracts_required_literal() {
        let q = regex_trigrams("error_handler", false);
        assert!(!q.is_unconstrained());
        // A literal pattern must not evaluate the same DNF twice.
        assert_eq!(q.dnfs.len(), 1);
    }

    #[test]
    fn regex_extracts_suffix_literals() {
        // The prefix literal ("fn ") is too short to filter, but the required
        // suffix "_handler" is selective; the query must be constrained by it.
        let q = regex_trigrams(r"fn \w+_handler", false);
        assert!(
            !q.is_unconstrained(),
            "suffix literal should constrain the query: {q:?}"
        );
    }

    #[test]
    fn case_insensitive_literal_is_constrained() {
        let q = TrigramQuery::from_literal_ci(b"Foo");
        assert!(!q.is_unconstrained());
        // One DNF per 3-byte window; "Foo" has one window of 3 letters =>
        // 2^3 single-trigram groups.
        assert_eq!(q.dnfs.len(), 1);
        let tris: Vec<Trigram> = q.dnfs[0].iter().map(|g| g[0]).collect();
        assert!(tris.contains(b"foo"));
        assert!(tris.contains(b"FOO"));
        assert!(tris.contains(b"Foo"));
        assert_eq!(tris.len(), 8);
        // Short needles cannot be filtered.
        assert!(TrigramQuery::from_literal_ci(b"fo").is_unconstrained());
    }

    #[test]
    fn ci_skips_windows_with_non_ascii_bytes() {
        // "café" => windows "caf", "af\xC3", "f\xC3\xA9". Only the all-ASCII
        // "caf" window is enumerable; the others span the multibyte 'é' whose
        // uppercase form ('É') has different bytes, so requiring them would drop
        // real matches.
        let q = TrigramQuery::from_literal_ci("café".as_bytes());
        assert_eq!(q.dnfs.len(), 1);
        let tris: Vec<Trigram> = q.dnfs[0].iter().map(|g| g[0]).collect();
        assert!(tris.contains(b"caf"));
        assert!(tris.contains(b"CAF"));
    }

    #[test]
    fn ci_kelvin_and_long_s_windows_stay_constrained() {
        // Windows containing 's'/'k' used to be dropped entirely (the fold
        // class includes a non-ASCII character), degrading common needles to
        // full scans. They are now kept by enumerating the multi-byte fold
        // forms, so every all-ASCII needle stays constrained.
        for needle in [&b"class"[..], b"list", b"make", b"kayak"] {
            let q = TrigramQuery::from_literal_ci(needle);
            assert!(
                !q.is_unconstrained(),
                "{needle:?} should be constrained: {q:?}"
            );
        }
        // The 's' window clause must include the long-s byte prefix so a
        // haystack containing U+017F still passes the filter.
        let q = TrigramQuery::from_literal_ci(b"las");
        let tris: Vec<Trigram> = q.dnfs[0].iter().map(|g| g[0]).collect();
        assert!(tris.contains(b"las"));
        assert!(tris.contains(b"LAS"));
        assert!(
            tris.contains(&[b'l', b'a', 0xC5]),
            "expected long-s prefix variant, got {tris:?}"
        );
    }

    /// Documents *why* the fold forms include 's'/'k' specials: the matcher's
    /// Unicode-aware case folding makes /k/i match U+212A and /s/i match
    /// U+017F, while a non-special letter like /a/i does not match U+00E5
    /// ('å'). If this ever changes upstream, `fold_forms` must be revisited.
    #[test]
    fn regex_ci_folds_kelvin_and_long_s() {
        let ci = |pat: &str, hay: &str| {
            regex::bytes::RegexBuilder::new(&regex::escape(pat))
                .case_insensitive(true)
                .build()
                .unwrap()
                .is_match(hay.as_bytes())
        };
        assert!(ci("k", "\u{212A}"), "/k/i should match KELVIN SIGN");
        assert!(
            ci("s", "\u{017F}"),
            "/s/i should match LATIN SMALL LETTER LONG S"
        );
        assert!(!ci("a", "\u{00E5}"), "/a/i should not match 'å'");
    }

    /// End-to-end soundness: whenever the case-insensitive matcher accepts a
    /// haystack, the trigram filter must also keep it (no false negatives).
    #[test]
    fn ci_filter_never_drops_a_match() {
        // (needle, haystack) pairs the Unicode-aware matcher accepts.
        let matching: &[(&str, &str)] = &[
            ("café", "a CAFÉ here"),        // non-ASCII fold
            ("café", "tiny café shop"),     // exact bytes
            ("class", "MyClass {}"),        // plain 's'
            ("class", "cla\u{017F}s X"),    // long s in the haystack
            ("foobar", "FOOBAR()"),         // plain ASCII
            ("make", "MAKEFILE"),           // plain 'k'
            ("make", "ma\u{212A}e it"),     // KELVIN SIGN in the haystack
            ("kayak", "KAYAK"),             // multiple 'k's
            ("kayak", "kaya\u{212A} trip"), // trailing Kelvin
            ("string", "STRING s"),
        ];
        for (needle, hay) in matching {
            let re = regex::bytes::RegexBuilder::new(&regex::escape(needle))
                .case_insensitive(true)
                .build()
                .unwrap();
            assert!(
                re.is_match(hay.as_bytes()),
                "test setup: {needle:?} must match {hay:?}"
            );

            let q = TrigramQuery::from_literal_ci(needle.as_bytes());
            assert!(
                filter_keeps(&q, hay.as_bytes()),
                "filter wrongly dropped {hay:?} for needle {needle:?}"
            );
        }
    }

    /// Soundness for the exact-case and regex planners too.
    #[test]
    fn literal_and_regex_filters_keep_their_matches() {
        let hay = b"pub fn segment_writer_flush(x: u32) {}";
        let lit = TrigramQuery::from_literal(b"segment_writer");
        assert!(filter_keeps(&lit, hay));

        let re = regex_trigrams(r"fn \w+_flush", false);
        assert!(filter_keeps(&re, hay));
    }

    /// Mirror of `Segment::candidates` evaluation for an in-memory document:
    /// the doc passes when, for every filtering DNF, at least one group's
    /// trigrams are all present.
    fn filter_keeps(q: &TrigramQuery, haystack: &[u8]) -> bool {
        let doc = extract(haystack);
        let has = |t: &Trigram| doc.binary_search(t).is_ok();
        q.dnfs
            .iter()
            .filter(|d| dnf_filters(d))
            .all(|dnf| dnf.iter().any(|group| group.iter().all(&has)))
    }
}
