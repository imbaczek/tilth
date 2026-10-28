//! Approximate suggestions for complete zero-hit narrow searches.
//!
//! These are deliberately not `Match` records: they must never become
//! definitions, expansion targets, or inputs to relationship searches.
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::listing::Page;

const MAX_WORK: usize = 1_000_000_000;
const MAX_SUGGESTIONS: usize = 100_000;

#[derive(Debug, Clone, Copy)]
pub(crate) struct Options {
    pub threshold: f64,
    pub max_files: usize,
    pub max_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            threshold: 0.80,
            max_files: 20,
            max_bytes: 10_000_000,
        }
    }
}

impl Options {
    pub fn from_args(args: &serde_json::Value) -> Result<Self, String> {
        let mut options = Self::default();
        if let Some(value) = args.get("similarity_threshold") {
            options.threshold = value
                .as_f64()
                .filter(|v| v.is_finite() && (0.0..=1.0).contains(v))
                .ok_or("similarity_threshold must be a number between 0 and 1")?;
        }
        if let Some(value) = args.get("similarity_max_files") {
            options.max_files = value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|n| *n > 0)
                .ok_or("similarity_max_files must be a positive integer")?;
        }
        if let Some(value) = args.get("similarity_max_bytes") {
            options.max_bytes = value
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or("similarity_max_bytes must be a positive integer")?;
        }
        Ok(options)
    }
}

#[derive(Debug)]
struct Suggestion {
    path: PathBuf,
    line: usize,
    candidate: String,
    score: f64,
}

#[derive(Default)]
struct Inventory {
    seen: HashSet<PathBuf>,
    files: Vec<PathBuf>,
    bytes: u64,
    incomplete: bool,
}

/// Collect the entire physical file set before performing approximate matching.
/// A failed walk or exhausted limit makes the fallback ineligible.
fn inventory(scopes: &[PathBuf], glob: Option<&str>, options: Options) -> Option<Vec<PathBuf>> {
    let state = Mutex::new(Inventory::default());
    for scope in scopes {
        let walker = super::walker(scope, glob).ok()?;
        walker.run(|| {
            Box::new(|entry| {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.incomplete {
                    return ignore::WalkState::Quit;
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => {
                        state.incomplete = true;
                        return ignore::WalkState::Quit;
                    }
                };
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    return ignore::WalkState::Continue;
                }
                let path = entry.path();
                let identity = match path.canonicalize() {
                    Ok(p) => p,
                    Err(_) => {
                        state.incomplete = true;
                        return ignore::WalkState::Quit;
                    }
                };
                if !state.seen.insert(identity.clone()) {
                    return ignore::WalkState::Continue;
                }
                let size = match std::fs::metadata(path) {
                    Ok(m) => m.len(),
                    Err(_) => {
                        state.incomplete = true;
                        return ignore::WalkState::Quit;
                    }
                };
                state.bytes = state.bytes.saturating_add(size);
                state.files.push(identity);
                if state.files.len() > options.max_files
                    || state.bytes > options.max_bytes
                    || size > crate::types::MAX_SOURCE_FILE_SIZE
                {
                    state.incomplete = true;
                    return ignore::WalkState::Quit;
                }
                ignore::WalkState::Continue
            })
        });
        if state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .incomplete
        {
            return None;
        }
    }
    let mut state = state
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.files.sort();
    Some(state.files)
}

fn normalized_osa(query: &str, candidate: &str) -> f64 {
    let length = query.chars().count().max(candidate.chars().count());
    if length == 0 {
        return 1.0;
    }
    1.0 - strsim::osa_distance(query, candidate) as f64 / length as f64
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Find the closest substring using OSA dynamic programming with free prefix.
/// Work is bounded across the entire request; aborting is disclosed to callers.
fn best_substring(
    query: &[char],
    line: &str,
    threshold: f64,
    work: &mut usize,
) -> Result<Option<(String, f64)>, ()> {
    let chars: Vec<char> = line.chars().collect();
    let m = query.len();
    if m == 0 || chars.is_empty() {
        return Ok(None);
    }
    // A candidate with score >= threshold cannot be longer than m/threshold.
    let max_length = if threshold == 0.0 {
        chars.len()
    } else {
        ((m as f64 / threshold + 1e-9).floor() as usize).min(chars.len())
    };
    let max_edits = (((1.0 - threshold) * m.max(max_length) as f64) + 1e-9).floor() as usize;
    let stride = max_edits.saturating_add(1);
    let cells = m.saturating_add(1).saturating_mul(stride);
    *work = work.saturating_add(cells.saturating_mul(chars.len()));
    // Three rows of start offsets are held simultaneously.
    if *work > MAX_WORK || cells > 1_000_000 {
        return Err(());
    }
    let unreachable = usize::MAX;
    let mut previous = vec![unreachable; cells];
    let mut previous2 = vec![unreachable; cells];
    let mut current = vec![unreachable; cells];
    for j in 0..=m.min(max_edits) {
        previous[j * stride + j] = 0;
    }
    let mut best: Option<(usize, usize, f64)> = None;
    for (i, &c) in chars.iter().enumerate() {
        current.fill(unreachable);
        current[0] = i + 1; // free prefix before the candidate
        for j in 1..=m {
            let cost = usize::from(query[j - 1] != c);
            for d in 0..=max_edits {
                let mut start = unreachable;
                if d > 0 {
                    start = start.min(previous[j * stride + d - 1]);
                    start = start.min(current[(j - 1) * stride + d - 1]);
                }
                if d >= cost {
                    start = start.min(previous[(j - 1) * stride + d - cost]);
                }
                if d > 0 && i > 0 && j > 1 && c == query[j - 2] && chars[i - 1] == query[j - 1] {
                    start = start.min(previous2[(j - 2) * stride + d - 1]);
                }
                current[j * stride + d] = start;
            }
        }
        for d in 0..=max_edits {
            let start = current[m * stride + d];
            if start == unreachable || start > i {
                continue;
            }
            let length = i + 1 - start;
            if length > max_length {
                continue;
            }
            let score = 1.0 - d as f64 / m.max(length) as f64;
            if score >= threshold && best.as_ref().is_none_or(|(_, _, old)| score > *old) {
                best = Some((start, i + 1, score));
            }
        }
        std::mem::swap(&mut previous2, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }
    Ok(best.map(|(start, end, score)| (chars[start..end].iter().collect(), score)))
}

/// Return a warning plus a stable page, or `None` if no suggestion is useful.
/// A work cap is reported rather than treating a partial scan as complete.
pub(crate) fn suggest(
    query: &str,
    kind: &str,
    scopes: &[PathBuf],
    glob: Option<&str>,
    options: Options,
    page: Page,
    mcp: bool,
) -> Option<String> {
    if query.is_empty() || !matches!(kind, "symbol" | "content") {
        return None;
    }
    let files = inventory(scopes, glob, options)?;
    let mut results = Vec::new();
    let query_chars: Vec<char> = query.chars().collect();
    let mut scores = HashMap::new();
    let mut work = 0usize;
    let mut observed_bytes = 0u64;
    for path in files {
        let size = std::fs::metadata(&path).ok()?.len();
        observed_bytes = observed_bytes.saturating_add(size);
        if observed_bytes > options.max_bytes || size > crate::types::MAX_SOURCE_FILE_SIZE {
            return None;
        }
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(crate::lang::detection::is_minified_by_name)
        {
            continue;
        }
        let bytes = std::fs::read(&path).ok()?;
        if bytes.len() as u64 != size {
            return None;
        }
        if bytes.len() as u64 >= crate::lang::detection::MINIFIED_CHECK_THRESHOLD
            && crate::lang::detection::is_minified_by_content(&bytes)
        {
            continue;
        }
        if crate::lang::detection::is_binary(&bytes) {
            continue;
        }
        let source = String::from_utf8_lossy(&bytes);
        for (line, text) in source.lines().enumerate() {
            let best = if kind == "symbol" {
                text.split(|c: char| !word_char(c))
                    .filter(|s| !s.is_empty())
                    .filter_map(|candidate| {
                        let length = candidate.chars().count().max(query_chars.len());
                        work = work.saturating_add(length.saturating_mul(length));
                        if work > MAX_WORK {
                            return None;
                        }
                        let score = *scores
                            .entry(candidate.to_owned())
                            .or_insert_with(|| normalized_osa(query, candidate));
                        (score >= options.threshold).then(|| (candidate.to_owned(), score))
                    })
                    .max_by(|a, b| a.1.total_cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
            } else {
                match best_substring(&query_chars, text, options.threshold, &mut work) {
                    Ok(candidate) => candidate.filter(|(_, score)| *score >= options.threshold),
                    Err(()) => return Some("> Similarity search skipped: work limit exceeded; narrow the scope or query.\n".into()),
                }
            };
            if work > MAX_WORK {
                return Some("> Similarity search skipped: work limit exceeded; narrow the scope or query.\n".into());
            }
            if let Some((candidate, score)) = best {
                if results.len() == MAX_SUGGESTIONS {
                    return Some("> Similarity search skipped: suggestion limit exceeded; narrow the scope.\n".into());
                }
                results.push(Suggestion {
                    path: path.clone(),
                    line: line + 1,
                    candidate,
                    score,
                });
            }
        }
    }
    if results.is_empty() {
        return None;
    }
    results.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a.candidate.cmp(&b.candidate))
    });
    let rows: Vec<_> = results
        .iter()
        .map(|m| {
            format!(
                "{}:{}  similarity {:.3}  candidate {:?}",
                m.path.display(),
                m.line,
                m.score,
                m.candidate
            )
        })
        .collect();
    let header = format!("# Similar suggestions for {query:?}\n> WARNING: No exact matches. These are approximate suggestions, not verified matches; do not follow, grok, or trace them.");
    Some(page.render(&header, &rows, "suggestions", mcp))
}

/// Append suggestions only when the complete exact result for that query is zero.
/// Raw search APIs and projections stay exact. A second, uncapped exact check
/// prevents an empty requested page from being mistaken for a zero-hit search.
pub(crate) fn append(
    base: String,
    queries: &[&str],
    kind: &str,
    scopes: &[PathBuf],
    context: Option<&Path>,
    glob: Option<&str>,
    options: Options,
    page: Page,
    budget: Option<u64>,
    mcp: bool,
) -> String {
    if !matches!(kind, "symbol" | "content" | "concept" | "fallthrough")
        || inventory(scopes, glob, options).is_none()
    {
        return base;
    }
    let mut output = base;
    let mut appended = false;
    for query in queries {
        if kind == "content" && super::parse_pattern(query).1 {
            continue;
        }
        let probe = super::paging::collect(
            query,
            kind,
            scopes,
            context,
            glob,
            Page {
                offset: 0,
                limit: 1,
            },
        );
        let complete_zero = match probe {
            Ok(result) => {
                result.total_found == 0
                    && result.scope_errors.is_empty()
                    && matches!(result.count_estimate, crate::types::CountEstimate::Exact(_))
            }
            Err(crate::error::TilthError::NotFound { .. })
                if matches!(kind, "concept" | "fallthrough") =>
            {
                true
            }
            Err(_) => false,
        };
        if !complete_zero {
            continue;
        }
        let effective_kind = if kind == "symbol"
            || (matches!(kind, "concept" | "fallthrough") && !query.contains(' '))
        {
            "symbol"
        } else {
            "content"
        };
        if let Some(suggestions) = suggest(query, effective_kind, scopes, glob, options, page, mcp)
        {
            output.push_str("\n\n---\n\n");
            output.push_str(&suggestions);
            appended = true;
        }
    }
    if appended {
        crate::listing::finish(&output, budget)
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osa_and_literal_substrings_handle_transposition_and_length_changes() {
        assert_eq!(strsim::osa_distance("CA", "ABC"), 3);
        assert!(normalized_osa("serach", "search") >= 0.8);
        assert!(normalized_osa("żółw", "żóĹw") < 1.0);
        for (query, line, expected) in [
            ("abcd", "axbcd", "axbcd"),
            (
                "little brown box",
                "prefix little brown fox suffix",
                "little brown fox",
            ),
            ("hello wold", "prefix hello world suffix", "hello world"),
            ("żółwie", "prefix żółiwie suffix", "żółiwie"),
        ] {
            let (candidate, score) =
                best_substring(&query.chars().collect::<Vec<_>>(), line, 0.8, &mut 0)
                    .unwrap()
                    .unwrap();
            assert!(score >= 0.8, "{query} {candidate} {score}");
            assert_eq!(candidate, expected);
        }
    }

    #[test]
    fn literal_matcher_agrees_with_exhaustive_osa_windows() {
        let words = |n: usize| -> Vec<String> {
            (0..(1 << n))
                .map(|bits| {
                    (0..n)
                        .map(|i| if bits & (1 << i) == 0 { 'a' } else { 'b' })
                        .collect()
                })
                .collect()
        };
        for query in words(4) {
            for line in words(6) {
                let mut best = 0.0f64;
                for start in 0..line.len() {
                    for end in start + 1..=line.len() {
                        best = best.max(normalized_osa(&query, &line[start..end]));
                    }
                }
                for threshold in [0.0, 0.5, 0.7, 0.8, 0.85, 1.0] {
                    let actual = best_substring(
                        &query.chars().collect::<Vec<_>>(),
                        &line,
                        threshold,
                        &mut 0,
                    )
                    .unwrap();
                    if best >= threshold {
                        assert!(
                            (actual.unwrap().1 - best).abs() < 1e-9,
                            "{query} {line} threshold={threshold} expected={best}"
                        );
                    } else {
                        assert!(
                            actual.is_none(),
                            "{query} {line} threshold={threshold} expected={best}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn inventory_limits_are_inclusive_and_deduplicate_overlapping_scopes() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..20 {
            let path = dir.path().join(format!("{n}.txt"));
            let file = std::fs::File::create(path).unwrap();
            file.set_len(500_000).unwrap();
        }
        let roots = [dir.path().to_path_buf(), dir.path().join("0.txt")];
        assert_eq!(
            inventory(&roots, None, Options::default()).unwrap().len(),
            20
        );
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("0.txt"))
            .unwrap()
            .set_len(500_001)
            .unwrap();
        assert!(inventory(&roots, None, Options::default()).is_none());
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("0.txt"))
            .unwrap()
            .set_len(500_000)
            .unwrap();
        std::fs::write(dir.path().join("extra.txt"), "x").unwrap();
        assert!(inventory(&roots, None, Options::default()).is_none());
        let relaxed = Options {
            max_files: 21,
            max_bytes: 10_000_001,
            ..Options::default()
        };
        assert_eq!(inventory(&roots, None, relaxed).unwrap().len(), 21);
    }

    #[test]
    fn options_validate_each_limit_and_threshold() {
        for args in [
            serde_json::json!({"similarity_threshold": -0.1}),
            serde_json::json!({"similarity_threshold": 1.1}),
            serde_json::json!({"similarity_threshold": "NaN"}),
            serde_json::json!({"similarity_max_files":0}),
            serde_json::json!({"similarity_max_bytes":0}),
        ] {
            assert!(Options::from_args(&args).is_err(), "{args}");
        }
        assert!(Options::from_args(&serde_json::json!({"similarity_threshold":0.0,"similarity_max_files":1,"similarity_max_bytes":1})).is_ok());
    }

    #[test]
    fn work_limit_is_reported_without_partial_results() {
        let mut work = MAX_WORK;
        assert!(best_substring(&['a'], "a", 0.8, &mut work).is_err());
    }
}
