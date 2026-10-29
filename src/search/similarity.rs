//! Approximate suggestions for complete zero-hit searches.
//!
//! These are deliberately not `Match` records: they must never become
//! definitions, expansion targets, or inputs to relationship searches.
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::listing::Page;

const MAX_WORK: usize = 1_000_000_000;
const MAX_SUGGESTIONS: usize = 100_000;
const MAX_FILENAME_ENTRIES: usize = 100_000;

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
            max_files: 500,
            max_bytes: 20_000_000,
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
    line: Option<usize>,
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

/// Collect the entire physical file set before performing approximate content matching.
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
                let Ok(entry) = entry else {
                    state.incomplete = true;
                    return ignore::WalkState::Quit;
                };
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    return ignore::WalkState::Continue;
                }
                let path = entry.path();
                let Ok(identity) = path.canonicalize() else {
                    state.incomplete = true;
                    return ignore::WalkState::Quit;
                };
                if !state.seen.insert(identity.clone()) {
                    return ignore::WalkState::Continue;
                }
                let size = if let Ok(m) = std::fs::metadata(path) {
                    m.len()
                } else {
                    state.incomplete = true;
                    return ignore::WalkState::Quit;
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

// Counts describe in-memory strings or bounded search work; f64 precision is sufficient for ranking.
#[allow(clippy::cast_precision_loss)]
fn normalized_osa(query: &str, candidate: &str) -> f64 {
    let length = query.chars().count().max(candidate.chars().count());
    if length == 0 {
        return 1.0;
    }
    1.0 - strsim::osa_distance(query, candidate) as f64 / length as f64
}

#[derive(Default)]
struct FilenameScan {
    visited: usize,
    work: usize,
    hits: HashMap<PathBuf, Suggestion>,
    incomplete: bool,
    exact_name: bool,
}

/// Compare names from the ignore-aware walk without opening or parsing files.
/// Physical aliases retain their best observed spelling; ties are deterministic.
// Counts describe in-memory strings or bounded search work; f64 precision is sufficient for ranking.
#[allow(clippy::cast_precision_loss)]
fn filename_suggestions(
    query: &str,
    scopes: &[PathBuf],
    glob: Option<&str>,
    options: Options,
) -> (Vec<Suggestion>, bool, bool) {
    if query.is_empty() || query.contains(['\n', '\r']) {
        return (Vec::new(), false, false);
    }
    let normalized_query: PathBuf = Path::new(query)
        .components()
        .filter(|component| !matches!(component, std::path::Component::CurDir))
        .collect();
    let query_path = normalized_query.as_path();
    let state = Mutex::new(FilenameScan::default());
    for scope in scopes {
        let Ok(walker) = super::walker(scope, glob) else {
            return (Vec::new(), true, false);
        };
        let root = if scope.is_file() {
            scope.parent().unwrap_or(Path::new("."))
        } else {
            scope.as_path()
        };
        // Compare absolute requests relative to the selected scope; a shared
        // absolute prefix must not inflate filename similarity.
        let scoped_query = if query_path.is_absolute() {
            let absolute_root = if root.is_absolute() {
                root.to_path_buf()
            } else {
                match std::env::current_dir() {
                    Ok(cwd) => cwd.join(root),
                    Err(_) => return (Vec::new(), true, false),
                }
            };
            match query_path.strip_prefix(&absolute_root) {
                Ok(relative) => relative,
                Err(_) => continue,
            }
        } else {
            query_path
        };
        let path_shaped = scoped_query.components().count() > 1;
        let with_extension = scoped_query.extension().is_some();
        let query_lower = scoped_query.to_string_lossy().to_lowercase();
        let query_len = query_lower.chars().count();
        walker.run(|| {
            Box::new(|entry| {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.incomplete || state.exact_name {
                    return ignore::WalkState::Quit;
                }
                state.visited = state.visited.saturating_add(1);
                if state.visited > MAX_FILENAME_ENTRIES {
                    state.incomplete = true;
                    return ignore::WalkState::Quit;
                }
                let Ok(entry) = entry else {
                    state.incomplete = true;
                    return ignore::WalkState::Quit;
                };
                if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                    return ignore::WalkState::Continue;
                }
                let path = entry.path();
                let candidate_path = if path_shaped {
                    match path.strip_prefix(root) {
                        Ok(relative) => relative.to_path_buf(),
                        Err(_) => return ignore::WalkState::Continue,
                    }
                } else {
                    match path.file_name() {
                        Some(name) => PathBuf::from(name),
                        None => return ignore::WalkState::Continue,
                    }
                };
                let candidate_path = if with_extension {
                    candidate_path
                } else {
                    candidate_path.with_extension("")
                };
                let Some(candidate) = candidate_path.to_str() else {
                    return ignore::WalkState::Continue;
                };
                let candidate_lower = candidate.to_lowercase();
                if candidate_lower == query_lower {
                    state.exact_name = true;
                    return ignore::WalkState::Quit;
                }
                let candidate_len = candidate_lower.chars().count();
                let longest = query_len.max(candidate_len);
                if longest == 0
                    || query_len.min(candidate_len) as f64 / (longest as f64) < options.threshold
                {
                    return ignore::WalkState::Continue;
                }
                state.work = state
                    .work
                    .saturating_add(query_len.saturating_mul(candidate_len));
                if state.work > MAX_WORK {
                    state.incomplete = true;
                    return ignore::WalkState::Quit;
                }
                let score = normalized_osa(&query_lower, &candidate_lower);
                if score < options.threshold {
                    return ignore::WalkState::Continue;
                }
                let Ok(identity) = path.canonicalize() else {
                    state.incomplete = true;
                    return ignore::WalkState::Quit;
                };
                let hit = Suggestion {
                    path: path.to_path_buf(),
                    line: None,
                    candidate: candidate.to_owned(),
                    score,
                };
                state
                    .hits
                    .entry(identity)
                    .and_modify(|current| {
                        if hit.score > current.score
                            || (hit.score.total_cmp(&current.score).is_eq()
                                && hit.path < current.path)
                        {
                            current.path.clone_from(&hit.path);
                            current.candidate.clone_from(&hit.candidate);
                            current.score = hit.score;
                        }
                    })
                    .or_insert(hit);
                ignore::WalkState::Continue
            })
        });
        let done = {
            let state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.incomplete || state.exact_name
        };
        if done {
            break;
        }
    }
    let state = state
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state.incomplete || state.exact_name {
        (Vec::new(), state.incomplete, state.exact_name)
    } else {
        (state.hits.into_values().collect(), false, false)
    }
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

/// Find the closest substring using OSA dynamic programming with free prefix.
/// Work is bounded across the entire request; aborting is disclosed to callers.
// Counts describe in-memory strings or bounded search work; f64 precision is sufficient for ranking.
#[allow(clippy::cast_precision_loss)]
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

/// Return a stable page of suggestions or a scan notice, or `None` if no suggestion is useful.
/// A work cap is reported rather than treating a partial scan as complete.
fn suggest(
    query: &str,
    kind: &str,
    files: Option<&[PathBuf]>,
    options: Options,
    mut results: Vec<Suggestion>,
    filename_incomplete: bool,
    page: Page,
    mcp: bool,
) -> Option<String> {
    if query.is_empty() || !matches!(kind, "symbol" | "content") {
        return None;
    }
    let filename_count = results.len();
    // A filename suggestion is metadata only: do not open it to collect text
    // suggestions or expand its source as part of this fallback.
    let filename_ids: Result<HashSet<_>, _> =
        results.iter().map(|hit| hit.path.canonicalize()).collect();
    let mut content_problem = None;
    let filename_ids = if let Ok(ids) = filename_ids {
        ids
    } else {
        content_problem = Some("Content similarity skipped: a filename changed during the search.");
        HashSet::new()
    };
    let query_chars: Vec<char> = query.chars().collect();
    let mut scores = HashMap::new();
    let mut work = 0usize;
    let mut observed_bytes = 0u64;
    'content: for path in files.unwrap_or_default() {
        if content_problem.is_some() {
            break;
        }
        let size = if let Ok(metadata) = std::fs::metadata(path) {
            metadata.len()
        } else {
            content_problem = Some("Content similarity skipped: a file could not be inspected.");
            break;
        };
        observed_bytes = observed_bytes.saturating_add(size);
        if observed_bytes > options.max_bytes || size > crate::types::MAX_SOURCE_FILE_SIZE {
            content_problem =
                Some("Content similarity skipped: the scope changed during the search.");
            break;
        }
        if filename_ids.contains(path) {
            continue;
        }
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(crate::lang::detection::is_minified_by_name)
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            content_problem = Some("Content similarity skipped: a file could not be read.");
            break;
        };
        if bytes.len() as u64 != size {
            content_problem =
                Some("Content similarity skipped: the scope changed during the search.");
            break;
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
            } else if let Ok(candidate) =
                best_substring(&query_chars, text, options.threshold, &mut work)
            {
                candidate.filter(|(_, score)| *score >= options.threshold)
            } else {
                content_problem = Some(
                    "Content similarity skipped: work limit exceeded; narrow the scope or query.",
                );
                break 'content;
            };
            if work > MAX_WORK {
                content_problem = Some(
                    "Content similarity skipped: work limit exceeded; narrow the scope or query.",
                );
                break 'content;
            }
            if let Some((candidate, score)) = best {
                if results.len() == MAX_SUGGESTIONS {
                    content_problem = Some(
                        "Content similarity skipped: suggestion limit exceeded; narrow the scope.",
                    );
                    break 'content;
                }
                results.push(Suggestion {
                    path: path.clone(),
                    line: Some(line + 1),
                    candidate,
                    score,
                });
            }
        }
    }
    if content_problem.is_some() {
        results.truncate(filename_count);
    }
    let mut notices = Vec::new();
    if filename_incomplete {
        notices.push("Filename similarity skipped: metadata walk or work limit exceeded, or a path could not be inspected.");
    }
    if let Some(problem) = content_problem {
        notices.push(problem);
    }
    if results.is_empty() && notices.is_empty() {
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
        .map(|hit| match hit.line {
            Some(line) => format!(
                "{}:{}  similarity {:.3}  candidate {:?}",
                hit.path.display(),
                line,
                hit.score,
                hit.candidate
            ),
            None => format!(
                "{}  [filename] similarity {:.3}  candidate {:?}",
                hit.path.display(),
                hit.score,
                hit.candidate
            ),
        })
        .collect();
    let mut header = format!("# Similar suggestions for {query:?}");
    for notice in notices {
        header.push_str("\n> ");
        header.push_str(notice);
    }
    if rows.is_empty() {
        Some(header)
    } else {
        Some(page.render(&header, &rows, "suggestions", mcp))
    }
}

/// Remove a footer only when the caller knows the base formatter emitted it.
fn strip_generated_token_footer(output: &mut String) {
    let Some((body, suffix)) = output.rsplit_once("\n\n(") else {
        return;
    };
    let expected = format!(
        "{} tokens)",
        super::format_token_count(crate::types::estimate_tokens(body.len() as u64))
    );
    if suffix == expected {
        output.truncate(body.len());
    }
}

/// Budget the complete response before measuring it. Omit the footer if it
/// cannot fit, and enforce the limit even for very small budgets.
fn finish_with_token_footer(output: &str, budget: Option<u64>) -> String {
    let limit = budget.unwrap_or(crate::budget::DEFAULT_BUDGET);
    let footer = |body: &str| {
        format!(
            "\n\n({} tokens)",
            super::format_token_count(crate::types::estimate_tokens(body.len() as u64))
        )
    };
    let initial_footer = footer(output);
    if crate::types::estimate_tokens((output.len() + initial_footer.len()) as u64) <= limit {
        return format!("{output}{initial_footer}");
    }
    let reserve = crate::types::estimate_tokens(initial_footer.len() as u64).saturating_add(1);
    let body =
        crate::listing::finish_with_reported_budget(output, limit.saturating_sub(reserve), limit);
    let final_footer = footer(&body);
    if crate::types::estimate_tokens((body.len() + final_footer.len()) as u64) <= limit {
        return format!("{body}{final_footer}");
    }
    let max_bytes = usize::try_from(limit.saturating_mul(4)).unwrap_or(usize::MAX);
    let mut end = max_bytes.min(body.len());
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    body[..end].to_string()
}

/// Summarize literal substring hits independently of approximate suggestions.
/// Candidate frequencies count lines, not occurrences, and large result sets use
/// a deterministic capped sample while retaining the complete search totals.
// Counts describe in-memory strings or bounded search work; f64 precision is sufficient for ranking.
#[allow(clippy::cast_precision_loss)]
fn substring_suggestions(
    query: &str,
    scopes: &[PathBuf],
    context: Option<&Path>,
    glob: Option<&str>,
    page: Page,
    mcp: bool,
) -> Option<String> {
    const SAMPLE_LIMIT: usize = 10_000;
    if query.is_empty()
        || !query
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let result = super::paging::collect(
        query,
        "content",
        scopes,
        context,
        glob,
        Page {
            offset: 0,
            limit: SAMPLE_LIMIT,
        },
    )
    .ok()?;
    if result.total_found == 0
        || !result.scope_errors.is_empty()
        || !matches!(result.count_estimate, crate::types::CountEstimate::Exact(_))
    {
        return None;
    }
    let mut frequencies: HashMap<&str, usize> = HashMap::new();
    for hit in &result.matches {
        let words: HashSet<_> = hit
            .text
            .split(|c: char| !c.is_alphanumeric() && c != '_' && c != '-')
            .filter(|word| word.len() > query.len() && word.contains(query))
            .collect();
        for word in words {
            *frequencies.entry(word).or_default() += 1;
        }
    }
    let max_frequency = frequencies.values().copied().max().unwrap_or(1);
    let denominator = (max_frequency as f64 + 1.0).ln();
    let mut candidates: Vec<_> = frequencies
        .into_iter()
        .map(|(word, count)| {
            // For a containing string, edit distance is exactly the extra characters.
            let similarity = query.chars().count() as f64 / word.chars().count() as f64;
            let frequency = (count as f64 + 1.0).ln() / denominator;
            let prefix = if word.starts_with(query) { 1.0 } else { 0.0 };
            (
                word,
                count,
                0.6 * similarity + 0.3 * frequency + 0.1 * prefix,
            )
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.2.total_cmp(&a.2)
            .then_with(|| b.1.cmp(&a.1))
            .then_with(|| a.0.cmp(b.0))
    });
    candidates.truncate(3);
    let sampled = result.matches.len() < result.total_found;
    let sample_note = if sampled {
        format!("; candidates sampled from {} lines", result.matches.len())
    } else {
        String::new()
    };
    let tests = result.facet_totals.tests;
    let files = result
        .file_ledger
        .as_ref()
        .map_or(0, std::collections::HashMap::len);
    let mut header = format!("# Substring matches for {query:?}: {} matching lines in {files} files ({tests} test-associated, {} other){sample_note}", result.total_found, result.total_found.saturating_sub(tests));
    if let Some(ledger) = &result.file_ledger {
        // Ledger totals cover the complete search, not just the candidate sample.
        let root = if result.scope.is_file() {
            result.scope.parent().unwrap_or(&result.scope)
        } else {
            &result.scope
        };
        let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut locations: Vec<_> = ledger
            .iter()
            .map(|(path, totals)| {
                let relative = path.strip_prefix(&root).unwrap_or(path);
                (relative.to_path_buf(), totals.hits)
            })
            .collect();
        let mut directories: HashMap<PathBuf, usize> = HashMap::new();
        for (path, count) in &locations {
            let dir = path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."));
            *directories.entry(dir.to_path_buf()).or_default() += count;
        }
        let mut directories: Vec<_> = directories.into_iter().collect();
        let sort = |entries: &mut Vec<(PathBuf, usize)>| {
            entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            entries.truncate(3);
        };
        sort(&mut locations);
        sort(&mut directories);
        for (label, entries) in [("Files", locations), ("Directories (direct)", directories)] {
            if !entries.is_empty() {
                let summary = entries
                    .into_iter()
                    .map(|(path, count)| format!("{} ({count})", path.display()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let _ = write!(header, "\n> {label}: {summary}");
            }
        }
    }
    let rows: Vec<_> = candidates
        .into_iter()
        .map(|(name, count, _)| {
            format!(
                "{name}  {count} matching lines{}",
                if sampled { " in sample" } else { "" }
            )
        })
        .collect();
    if rows.is_empty() {
        Some(header)
    } else {
        Some(page.render(&header, &rows, "candidates", mcp))
    }
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
    base_has_token_footer: bool,
) -> String {
    if !matches!(kind, "symbol" | "content" | "concept" | "fallthrough") {
        return base;
    }
    // The byte and file limits apply to content scanning, not filename metadata.
    let content_files = inventory(scopes, glob, options);
    let mut output = base;
    let mut appended = false;
    for query in queries {
        if kind == "content" && super::parse_pattern(query).1 {
            continue;
        }
        let mut filename_hits = Vec::new();
        let mut filename_incomplete = false;
        let mut exact_name = false;
        if content_files.is_none() {
            (filename_hits, filename_incomplete, exact_name) =
                filename_suggestions(query, scopes, glob, options);
            if kind != "symbol"
                && (exact_name || (filename_hits.is_empty() && !filename_incomplete))
            {
                continue;
            }
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
        if content_files.is_some() {
            (filename_hits, filename_incomplete, exact_name) =
                filename_suggestions(query, scopes, glob, options);
        }
        if exact_name && kind != "symbol" {
            continue;
        }
        let effective_kind = if kind == "symbol"
            || (matches!(kind, "concept" | "fallthrough") && !query.contains(' '))
        {
            "symbol"
        } else {
            "content"
        };
        let substring = if kind == "symbol" {
            substring_suggestions(query, scopes, context, glob, page, mcp)
        } else {
            None
        };
        let similar = if exact_name {
            None
        } else {
            suggest(
                query,
                effective_kind,
                content_files.as_deref(),
                options,
                filename_hits,
                filename_incomplete,
                page,
                mcp,
            )
        };
        for suggestions in [substring, similar].into_iter().flatten() {
            if !appended && base_has_token_footer {
                strip_generated_token_footer(&mut output);
            }
            output.push_str("\n\n---\n\n");
            output.push_str(&suggestions);
            appended = true;
        }
    }
    if appended {
        finish_with_token_footer(&output, budget)
    } else {
        output
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similarity_footer_respects_small_budgets_and_utf8() {
        let output = format!(
            "# Search: résumé\n\n{}\n> Next page: offset=1",
            "é".repeat(200)
        );
        for limit in [0, 1, 5, 15, 20, 30, 40, 60, 120] {
            let result = finish_with_token_footer(&output, Some(limit));
            assert!(
                crate::types::estimate_tokens(result.len() as u64) <= limit,
                "budget {limit}: {result}"
            );
            if crate::types::estimate_tokens(output.len() as u64) > limit {
                assert!(!result.contains("> Next page:"), "budget {limit}: {result}");
            }
            if let Some((_, diagnostic)) = result.rsplit_once("budget: ") {
                let digits: String = diagnostic
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                if !digits.is_empty() {
                    assert_eq!(digits.parse::<u64>().unwrap(), limit);
                }
            }
        }
        let limit = crate::types::estimate_tokens(output.len() as u64);
        let result = finish_with_token_footer(&output, Some(limit));
        assert!(result.ends_with(" tokens)"), "{result}");
        assert!(!result.contains("> Next page:"));
        assert!(crate::types::estimate_tokens(result.len() as u64) <= limit);
    }

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
        for n in 0..500 {
            let path = dir.path().join(format!("{n}.txt"));
            let file = std::fs::File::create(path).unwrap();
            file.set_len(40_000).unwrap();
        }
        let roots = [dir.path().to_path_buf(), dir.path().join("0.txt")];
        assert_eq!(
            inventory(&roots, None, Options::default()).unwrap().len(),
            500
        );
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("0.txt"))
            .unwrap()
            .set_len(40_001)
            .unwrap();
        assert!(inventory(&roots, None, Options::default()).is_none());
        std::fs::OpenOptions::new()
            .write(true)
            .open(dir.path().join("0.txt"))
            .unwrap()
            .set_len(40_000)
            .unwrap();
        std::fs::write(dir.path().join("extra.txt"), "").unwrap();
        assert!(inventory(&roots, None, Options::default()).is_none());
        let relaxed = Options {
            max_files: 501,
            max_bytes: 20_000_000,
            ..Options::default()
        };
        assert_eq!(inventory(&roots, None, relaxed).unwrap().len(), 501);
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

#[cfg(test)]
mod filename_tests {
    use super::*;

    #[test]
    fn filename_metadata_respects_filters_and_compares_stems_and_paths() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        std::fs::write(nested.join("catalog_digest.rs"), "private marker").unwrap();
        std::fs::write(nested.join("catalog_digest.py"), "other marker").unwrap();
        let roots = [dir.path().to_path_buf()];
        let strict_content_limits = Options {
            max_files: 1,
            max_bytes: 1,
            ..Options::default()
        };
        let (stems, incomplete, exact_name) =
            filename_suggestions("catalog_digests", &roots, None, strict_content_limits);
        assert!(!incomplete && !exact_name);
        assert_eq!(stems.len(), 2);
        let (dot, incomplete, exact_name) =
            filename_suggestions("./catalog_digests", &roots, None, strict_content_limits);
        assert!(!incomplete && !exact_name && dot.len() == 2);
        let absolute_query = nested.join("catalog_digests.rs");
        let (absolute, incomplete, exact_name) = filename_suggestions(
            absolute_query.to_str().unwrap(),
            &roots,
            None,
            strict_content_limits,
        );
        assert!(
            !incomplete
                && !exact_name
                && absolute
                    .iter()
                    .any(|hit| hit.path.ends_with("catalog_digest.rs"))
        );
        let (filtered, incomplete, exact_name) = filename_suggestions(
            "catalog_digests",
            &roots,
            Some("*.rs"),
            strict_content_limits,
        );
        assert!(!incomplete && !exact_name);
        assert_eq!(filtered.len(), 1);
        assert!(filtered[0].path.ends_with("catalog_digest.rs"));
        let (path_hits, incomplete, exact_name) = filename_suggestions(
            "nested/catalog_digests.rs",
            &roots,
            None,
            strict_content_limits,
        );
        assert!(!incomplete && !exact_name);
        assert!(path_hits
            .iter()
            .any(|hit| hit.path.ends_with("catalog_digest.rs")));
        let (wrong_dir, incomplete, exact_name) = filename_suggestions(
            "entirely_different_and_unrelated_directory/catalog_digests.rs",
            &roots,
            None,
            strict_content_limits,
        );
        assert!(!incomplete && !exact_name && wrong_dir.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn aliases_are_compared_before_physical_deduplication() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("unrelated.bin");
        std::fs::write(&target, "secret").unwrap();
        std::os::unix::fs::symlink(&target, dir.path().join("catalog_digest.rs")).unwrap();
        let roots = [dir.path().to_path_buf(), dir.path().to_path_buf()];
        let (hits, incomplete, exact_name) =
            filename_suggestions("catalog_digests", &roots, None, Options::default());
        assert!(!incomplete && !exact_name);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].path.ends_with("catalog_digest.rs"));
    }
}
