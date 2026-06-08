//! Trigram extraction and query decomposition.
//!
//! A trigram is a 3-byte sequence. The index maps each trigram to the set of
//! documents that contain it. A query is satisfiable only in documents that
//! contain *every* trigram of the query literal, so we can intersect posting
//! lists to get a small candidate set before verifying with the real matcher.

use std::collections::BTreeSet;

/// A trigram, stored big-endian so byte order matches numeric order (required
/// for the FST term dictionary, whose keys must be lexicographically sorted).
pub type Trigram = [u8; 3];

/// Extract the set of distinct trigrams present in `data`.
pub fn extract(data: &[u8]) -> BTreeSet<Trigram> {
    let mut set = BTreeSet::new();
    if data.len() < 3 {
        return set;
    }
    // Each 3-byte window is one trigram; the BTreeSet dedups duplicates and keeps
    // keys sorted, which the FST term dictionary requires.
    for w in data.windows(3) {
        set.insert([w[0], w[1], w[2]]);
    }
    set
}

/// Extract the trigrams of a literal needle, sorted and deduplicated. Returns an
/// empty vec when the needle is shorter than 3 bytes (meaning: trigram filtering
/// can't help and the caller must scan all candidates).
pub fn literal_trigrams(needle: &[u8]) -> Vec<Trigram> {
    extract(needle).into_iter().collect()
}

/// A boolean query over trigrams supporting two complementary shapes:
///
/// * `or_groups` is disjunctive normal form (DNF): each inner group is an AND of
///   trigrams and the outer set is an OR of groups. Used for exact literals and
///   regex required-literal alternations.
/// * `and_clauses` is conjunctive normal form (CNF): each inner clause is an OR
///   of trigrams and a document must satisfy *every* clause. Used for
///   case-insensitive literals, where each needle position contributes the OR of
///   its case variants.
///
/// A document is a candidate if it satisfies the DNF part (or the DNF part is
/// empty) *and* every CNF clause. An empty query means "scan everything".
#[derive(Debug, Default, Clone)]
pub struct TrigramQuery {
    pub or_groups: Vec<Vec<Trigram>>,
    pub and_clauses: Vec<Vec<Trigram>>,
}

impl TrigramQuery {
    /// True when no usable trigram constraints exist and all documents are
    /// candidates. A group/clause that is empty means that part can't filter.
    pub fn is_unconstrained(&self) -> bool {
        let dnf_off = self.or_groups.is_empty() || self.or_groups.iter().any(|g| g.is_empty());
        let cnf_off = self.and_clauses.is_empty() || self.and_clauses.iter().any(|c| c.is_empty());
        dnf_off && cnf_off
    }

    pub fn from_literal(needle: &[u8]) -> TrigramQuery {
        let tris = literal_trigrams(needle);
        if tris.is_empty() {
            TrigramQuery::default()
        } else {
            TrigramQuery {
                or_groups: vec![tris],
                and_clauses: Vec::new(),
            }
        }
    }

    /// Build a case-insensitive literal query. Each 3-byte window of the needle
    /// becomes a CNF clause listing every ASCII-case variant of that window, so
    /// the trigram index can still prune candidates without false negatives.
    ///
    /// A window is only usable as a clause when all three of its bytes are
    /// *ASCII-case-safe* (see [`ci_safe`]): otherwise the case-insensitive
    /// matcher (Unicode-aware) could match bytes we did not enumerate, and
    /// requiring the ASCII trigrams would drop real matches. Unsafe windows are
    /// skipped, which only widens the candidate set. If no usable window remains,
    /// the query is unconstrained and every document is scanned.
    pub fn from_literal_ci(needle: &[u8]) -> TrigramQuery {
        if needle.len() < 3 {
            return TrigramQuery::default();
        }
        let mut and_clauses: Vec<Vec<Trigram>> = Vec::new();
        for w in needle.windows(3) {
            if w.iter().all(|&b| ci_safe(b)) {
                and_clauses.push(case_variants([w[0], w[1], w[2]]));
            }
        }
        if and_clauses.is_empty() {
            return TrigramQuery::default();
        }
        TrigramQuery {
            or_groups: Vec::new(),
            and_clauses,
        }
    }
}

/// True when a byte's set of case-insensitive matches (under the matcher's
/// Unicode-aware case folding) is fully captured by enumerating its ASCII case
/// variants, so a trigram window containing it can soundly prune candidates.
///
/// Two classes of bytes are *not* safe:
///
/// * Bytes `>= 0x80` belong to multibyte UTF-8 sequences; their folded forms
///   differ in both bytes and length, so the ASCII variants of the raw bytes do
///   not cover what the matcher accepts.
/// * `s`/`S` and `k`/`K`: under Unicode simple case folding their fold class also
///   contains a non-ASCII character (U+017F LATIN SMALL LETTER LONG S folds to
///   `s`; U+212A KELVIN SIGN folds to `k`). A case-insensitive match could land
///   on text containing those characters, whose UTF-8 bytes never form this
///   ASCII trigram. (The `regex` crate, which backs the matcher, folds these by
///   default; see the `regex_ci_folds_kelvin_and_long_s` test.)
///
/// All other ASCII bytes are safe: ASCII letters fold only to `{lower, upper}`
/// and ASCII non-letters fold to themselves.
fn ci_safe(b: u8) -> bool {
    b < 0x80 && !matches!(b, b's' | b'S' | b'k' | b'K')
}

/// All ASCII-case permutations of a trigram (bytes that aren't ASCII letters are
/// fixed). At most 2^3 = 8 variants.
fn case_variants(w: Trigram) -> Vec<Trigram> {
    let mut variants: Vec<Trigram> = vec![[0; 3]];
    for (i, &b) in w.iter().enumerate() {
        if b.is_ascii_alphabetic() {
            let lo = b.to_ascii_lowercase();
            let up = b.to_ascii_uppercase();
            let mut next = Vec::with_capacity(variants.len() * 2);
            for v in &variants {
                let mut a = *v;
                a[i] = lo;
                let mut c = *v;
                c[i] = up;
                next.push(a);
                next.push(c);
            }
            variants = next;
        } else {
            for v in &mut variants {
                v[i] = b;
            }
        }
    }
    variants.sort_unstable();
    variants.dedup();
    variants
}

/// Build a trigram query from a regular expression by extracting required
/// literal substrings. If the regex has no usable required literals we fall back
/// to an unconstrained query (scan all candidates).
pub fn regex_trigrams(pattern: &str, case_insensitive: bool) -> TrigramQuery {
    use regex_syntax::hir::literal::Extractor;
    use regex_syntax::ParserBuilder;

    let hir = match ParserBuilder::new()
        .case_insensitive(case_insensitive)
        .build()
        .parse(pattern)
    {
        Ok(h) => h,
        Err(_) => return TrigramQuery::default(),
    };

    // Prefix literals that any match must start with. If the set is not exact or
    // is infinite, the extractor yields inexact literals which still anchor the
    // search usefully.
    let seq = Extractor::new().extract(&hir);
    let mut or_groups: Vec<Vec<Trigram>> = Vec::new();
    if let Some(lits) = seq.literals() {
        for lit in lits {
            let tris = literal_trigrams(lit.as_bytes());
            if tris.is_empty() {
                // A short or empty required literal disables filtering entirely.
                return TrigramQuery::default();
            }
            or_groups.push(tris);
        }
    }
    if or_groups.is_empty() {
        TrigramQuery::default()
    } else {
        TrigramQuery {
            or_groups,
            and_clauses: Vec::new(),
        }
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
    }

    #[test]
    fn case_insensitive_literal_is_constrained() {
        let q = TrigramQuery::from_literal_ci(b"Foo");
        assert!(!q.is_unconstrained());
        // One CNF clause per 3-byte window.
        assert_eq!(q.and_clauses.len(), 1);
        let clause = &q.and_clauses[0];
        // "Foo" has 3 letters => 2^3 case variants.
        assert!(clause.contains(b"foo"));
        assert!(clause.contains(b"FOO"));
        assert!(clause.contains(b"Foo"));
        assert_eq!(clause.len(), 8);
        // Short needles cannot be filtered.
        assert!(TrigramQuery::from_literal_ci(b"fo").is_unconstrained());
    }

    #[test]
    fn ci_skips_windows_with_non_ascii_bytes() {
        // "café" => windows "caf", "af\xC3", "f\xC3\xA9". Only the all-ASCII
        // "caf" window is sound; the others span the multibyte 'é' whose
        // uppercase form ('É') has different bytes, so requiring them would drop
        // real matches.
        let q = TrigramQuery::from_literal_ci("café".as_bytes());
        assert_eq!(q.and_clauses.len(), 1);
        let clause = &q.and_clauses[0];
        assert!(clause.contains(b"caf"));
        assert!(clause.contains(b"CAF"));
        // No clause may require a trigram containing a non-ASCII byte.
        for clause in &q.and_clauses {
            for tri in clause {
                assert!(
                    tri.iter().all(|&b| b < 0x80),
                    "clause kept non-ASCII {tri:?}"
                );
            }
        }
    }

    #[test]
    fn ci_skips_kelvin_and_long_s_windows() {
        // 's'/'k' fold to non-ASCII characters under Unicode, so any window
        // containing them is dropped.
        // "class" => "cla" (kept), "las"/"ass" (dropped: contain 's').
        let q = TrigramQuery::from_literal_ci(b"class");
        assert_eq!(q.and_clauses.len(), 1);
        assert!(q.and_clauses[0].contains(b"cla"));

        // Every window contains 's' or 'k' => unconstrained (full scan), but sound.
        assert!(TrigramQuery::from_literal_ci(b"list").is_unconstrained());
        assert!(TrigramQuery::from_literal_ci(b"make").is_unconstrained());
    }

    /// Documents *why* `ci_safe` rejects 's'/'k': the matcher's Unicode-aware
    /// case folding makes /k/i match U+212A and /s/i match U+017F, while a
    /// non-special letter like /a/i does not match U+00E5 ('å'). If this ever
    /// changes upstream, the `ci_safe` skip-set must be revisited.
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
    /// haystack, the trigram CNF filter must also keep it (no false negatives).
    #[test]
    fn ci_filter_never_drops_a_match() {
        // (needle, haystack) pairs the Unicode-aware matcher accepts.
        let matching: &[(&str, &str)] = &[
            ("café", "a CAFÉ here"),    // non-ASCII fold
            ("café", "tiny café shop"), // exact bytes
            ("class", "MyClass {}"),    // 's' windows dropped, 'cla' kept
            ("foobar", "FOOBAR()"),     // plain ASCII
            ("make", "MAKEFILE"),       // all windows dropped => unconstrained
            ("kayak", "KAYAK"),         // 'k' windows dropped
            ("string", "STRING s"),     // 's' windows dropped, others kept
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
                ci_filter_keeps(&q, hay.as_bytes()),
                "filter wrongly dropped {hay:?} for needle {needle:?}"
            );
        }
    }

    /// Mirror of `Segment::candidates` CNF evaluation for an in-memory document:
    /// the doc passes when every clause shares at least one trigram with it.
    fn ci_filter_keeps(q: &TrigramQuery, haystack: &[u8]) -> bool {
        if q.is_unconstrained() {
            return true;
        }
        let doc = extract(haystack);
        q.and_clauses
            .iter()
            .all(|clause| clause.iter().any(|t| doc.contains(t)))
    }
}
