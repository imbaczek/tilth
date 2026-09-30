//! File-level dependency analysis: what a file imports and what imports it.
//! Used by `tilth_deps` for blast-radius checks before breaking changes.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::TilthError;
use crate::lang::detect_file_type;
use crate::lang::outline::get_outline_entries;
use crate::read::imports::{
    dependency_import_sources, is_external, js_module_sources_from_file, resolve_import_source,
};
use crate::search::callees::{extract_callee_names, resolve_callees};
use crate::search::callers::find_callers_in_files;
use crate::types::{FileType, OutlineKind};

/// Maximum number of exported symbols to search for in the reverse direction.
const MAX_EXPORTED_SYMBOLS: usize = 25;

/// Maximum number of dependents to show before truncation.
const MAX_DEPENDENTS: usize = 15;
const MAX_LOCAL_DEPENDENCIES: usize = 8;

/// Result of a full dependency analysis for a single file.
pub struct DepsResult {
    pub target: PathBuf,
    pub uses_local: Vec<LocalDep>,
    pub uses_external: Vec<String>,
    pub used_by: Vec<Dependent>,
    /// Total observed dependents, independent of display limits.
    pub total_dependents: usize,
    /// False when reverse symbol or call-site collection stopped at a limit.
    pub dependents_complete: bool,
    pub exported_count: usize,
    /// Actual number of symbols searched (may be < `exported_count` if capped).
    pub searched_count: usize,
}

/// A local file dependency with the symbols used from it.
pub struct LocalDep {
    pub path: PathBuf,
    pub symbols: Vec<String>,
}

/// A file that depends on the target, with symbol-level call detail.
pub struct Dependent {
    pub path: PathBuf,
    /// (`calling_function`, `called_symbol`, `line`) triples.
    pub symbols: Vec<(String, String, u32)>,
    pub is_test: bool,
}

/// Analyse the dependency graph for `path` within `scope`.
///
/// Phase 1: Extract exported symbols from the outline.
/// Phase 2: Forward dependencies — what this file uses.
/// Phase 3: Reverse dependencies — what uses this file.
#[cfg(test)]
pub fn analyze_deps(
    path: &Path,
    scope: &Path,
    bloom: &crate::index::bloom::BloomFilterCache,
) -> Result<DepsResult, TilthError> {
    analyze_deps_with_options(path, scope, bloom, false)
}

/// Full mode removes analysis limits as well as the formatter's preview limits.
pub fn analyze_deps_with_options(
    path: &Path,
    scope: &Path,
    bloom: &crate::index::bloom::BloomFilterCache,
    full: bool,
) -> Result<DepsResult, TilthError> {
    // Canonicalize for reliable path comparison (callers return absolute paths).
    let path = &path.canonicalize().map_err(|e| TilthError::IoError {
        path: path.to_path_buf(),
        source: e,
    })?;

    let content = fs::read_to_string(path).map_err(|e| TilthError::IoError {
        path: path.clone(),
        source: e,
    })?;

    let FileType::Code(lang) = detect_file_type(path) else {
        // Non-code file: return empty deps gracefully.
        return Ok(DepsResult {
            target: path.clone(),
            uses_local: Vec::new(),
            uses_external: Vec::new(),
            used_by: Vec::new(),
            total_dependents: 0,
            dependents_complete: true,
            exported_count: 0,
            searched_count: 0,
        });
    };

    // ── Phase 1: Extract exported symbols ────────────────────────────────────

    let entries = get_outline_entries(&content, lang);

    let mut all_names: Vec<String> = Vec::new();
    for entry in &entries {
        // Skip imports and re-export wrappers — they don't define symbols here.
        if matches!(entry.kind, OutlineKind::Import | OutlineKind::Export) {
            continue;
        }
        collect_symbol_names(entry, &mut all_names);
    }

    // Deduplicate
    all_names.sort();
    all_names.dedup();

    // Filter placeholder / noise names
    all_names.retain(|n| !is_placeholder_name(n));

    let exported_count = all_names.len();

    // Cap at MAX_EXPORTED_SYMBOLS, preferring longer (more specific) names
    let searched_count = if !full && all_names.len() > MAX_EXPORTED_SYMBOLS {
        all_names.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        all_names.truncate(MAX_EXPORTED_SYMBOLS);
        MAX_EXPORTED_SYMBOLS
    } else {
        all_names.len()
    };

    // ── Phase 2: Forward dependencies ────────────────────────────────────────

    // Local deps via callee resolution
    let callee_names = extract_callee_names(&content, lang, None);
    let resolved = resolve_callees(&callee_names, path, &content, bloom);

    // Group resolved callees by file
    let mut local_by_file: HashMap<PathBuf, Vec<String>> = HashMap::new();
    for callee in resolved {
        if callee.file != *path {
            local_by_file
                .entry(callee.file)
                .or_default()
                .push(callee.name);
        }
    }

    // Merge in import-resolved files (may not have resolved callees if symbols
    // weren't matched, but the import relationship itself is meaningful)
    let import_sources = dependency_import_sources(&content, lang);
    for source in &import_sources {
        if let Some(import_path) = resolve_import_source(path, source, lang) {
            local_by_file.entry(import_path).or_default();
        }
    }

    // Sort symbols within each dep, then build the list sorted by path
    let mut uses_local: Vec<LocalDep> = local_by_file
        .into_iter()
        .map(|(dep_path, mut syms)| {
            syms.sort();
            syms.dedup();
            LocalDep {
                path: dep_path,
                symbols: syms,
            }
        })
        .collect();
    uses_local.sort_by(|a, b| a.path.cmp(&b.path));

    // External and local counts come from the same complete source list.
    let mut external_set: HashSet<String> = HashSet::new();
    for source in &import_sources {
        if is_external(source, lang)
            && resolve_import_source(path, source, lang).is_none()
            && !is_stdlib(source, lang)
            && is_valid_module_path(source)
        {
            external_set.insert(source.clone());
        }
    }
    let mut uses_external: Vec<String> = external_set.into_iter().collect();
    uses_external.sort();

    // ── Phase 3: Reverse dependencies ────────────────────────────────────────

    let mut dependents_complete = true;
    let used_by = if searched_count > 0
        || matches!(
            lang,
            crate::types::Lang::TypeScript
                | crate::types::Lang::Tsx
                | crate::types::Lang::JavaScript
        ) {
        let symbols_set: HashSet<String> = all_names.iter().cloned().collect();
        // TypeScript dependencies are established by imports, including tsconfig
        // aliases. A name-only call match is not evidence of a dependency.
        let is_ts = matches!(
            lang,
            crate::types::Lang::TypeScript
                | crate::types::Lang::Tsx
                | crate::types::Lang::JavaScript
        );
        let importing_files = if is_ts {
            find_importers(path, scope)?
        } else {
            HashSet::new()
        };
        let (raw_matches, scan_complete) = if is_ts {
            (
                find_callers_in_files(&importing_files, scope, &symbols_set),
                true,
            )
        } else {
            crate::search::callers::find_callers_batch_with_size_limit(
                &symbols_set,
                scope,
                bloom,
                None,
                if full {
                    usize::MAX
                } else {
                    crate::search::callers::BATCH_EARLY_QUIT
                },
                if full {
                    u64::MAX
                } else {
                    super::bloom_walk::MAX_FILE_SIZE
                },
            )?
        };

        if !is_ts && !full {
            dependents_complete = searched_count == exported_count && scan_complete;
        }

        // Group by file path
        let mut by_file: HashMap<PathBuf, Vec<(String, String, u32)>> = HashMap::new();
        for (matched_symbol, caller_match) in raw_matches {
            // Exclude calls from within the target file itself (self-references)
            if caller_match.path == *path {
                continue;
            }
            by_file.entry(caller_match.path).or_default().push((
                caller_match.calling_function,
                matched_symbol,
                caller_match.line,
            ));
        }
        for importer in importing_files {
            if importer != *path {
                by_file.entry(importer).or_default();
            }
        }

        // Build Dependent list
        let target_dir = path.parent();
        let mut dependents: Vec<Dependent> = by_file
            .into_iter()
            .map(|(dep_path, mut pairs)| {
                pairs.sort();
                pairs.dedup();
                let is_test = is_test_file(&dep_path);
                Dependent {
                    path: dep_path,
                    symbols: pairs,
                    is_test,
                }
            })
            .collect();

        // Sort: same directory first, non-tests before tests, then alphabetical
        dependents.sort_by(|a, b| {
            let a_same_dir = target_dir.is_some_and(|d| a.path.parent() == Some(d));
            let b_same_dir = target_dir.is_some_and(|d| b.path.parent() == Some(d));
            b_same_dir
                .cmp(&a_same_dir)
                .then_with(|| a.is_test.cmp(&b.is_test))
                .then_with(|| a.path.cmp(&b.path))
        });

        dependents
    } else {
        Vec::new()
    };

    let total_dependents = used_by.len();

    Ok(DepsResult {
        target: path.clone(),
        uses_local,
        uses_external,
        used_by,
        total_dependents,
        dependents_complete,
        exported_count,
        searched_count,
    })
}

pub(crate) fn find_importers(target: &Path, scope: &Path) -> Result<HashSet<PathBuf>, TilthError> {
    find_importers_impl(target, scope, false)
}

/// Include modules exporting instances of imported classes, not only barrels.
/// Grok subsequently checks which exported binding matches the receiver.
pub(crate) fn find_transitive_importers(
    target: &Path,
    scope: &Path,
) -> Result<HashSet<PathBuf>, TilthError> {
    find_importers_impl(target, scope, true)
}

fn find_importers_impl(
    target: &Path,
    scope: &Path,
    follow_all: bool,
) -> Result<HashSet<PathBuf>, TilthError> {
    let edges = std::sync::Mutex::new(Vec::new());
    super::walker(scope, None)?.run(|| {
        let edges = &edges;
        Box::new(move |entry| {
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                return ignore::WalkState::Continue;
            }
            let importer = entry.path();
            let FileType::Code(
                lang @ (crate::types::Lang::TypeScript
                | crate::types::Lang::Tsx
                | crate::types::Lang::JavaScript),
            ) = detect_file_type(importer)
            else {
                return ignore::WalkState::Continue;
            };
            let file_edges: Vec<_> = js_module_sources_from_file(importer, lang)
                .into_iter()
                .filter_map(|(source, reexport)| {
                    resolve_import_source(importer, &source, lang)
                        .map(|imported| (importer.to_path_buf(), imported, reexport))
                })
                .collect();
            if !file_edges.is_empty() {
                edges
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(file_edges);
            }
            ignore::WalkState::Continue
        })
    });
    let edges = edges
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut exported_through = HashSet::from([target.to_path_buf()]);
    loop {
        let before = exported_through.len();
        for (importer, imported, reexport) in &edges {
            if (follow_all || *reexport) && exported_through.contains(imported) {
                exported_through.insert(importer.clone());
            }
        }
        if exported_through.len() == before {
            break;
        }
    }
    Ok(edges
        .into_iter()
        .filter(|(_, imported, _)| exported_through.contains(imported))
        .map(|(importer, _, _)| importer)
        .collect())
}

/// Format a `DepsResult` as a compact, readable string.
///
/// Budget truncation priority (when `budget` tokens is too tight):
/// 1. Truncate "Used by" entries (keep header count)
/// 2. Truncate "Uses (external)" to count only
/// 3. Truncate "Uses (local)" symbol lists to file paths only
/// 4. Never truncate the header line
#[cfg(test)]
pub fn format_deps(result: &DepsResult, scope: &Path, budget: Option<usize>) -> String {
    format_deps_with_options(result, scope, budget, false)
}

pub fn format_deps_with_options(
    result: &DepsResult,
    scope: &Path,
    budget: Option<usize>,
    full: bool,
) -> String {
    let dep_count = result.total_dependents;
    let local_limit = if full {
        result.uses_local.len()
    } else {
        MAX_LOCAL_DEPENDENCIES.min(result.uses_local.len())
    };
    let dependent_limit = if full {
        result.used_by.len()
    } else {
        MAX_DEPENDENTS.min(result.used_by.len())
    };
    let (prod_deps, test_deps): (Vec<_>, Vec<_>) = result.used_by[..dependent_limit]
        .iter()
        .partition(|d| !d.is_test);
    let dep_count_label = if result.dependents_complete {
        dep_count.to_string()
    } else {
        format!("at least {dep_count}")
    };

    // ── Build sections (full fidelity first) ─────────────────────────────────

    // Header
    let rel_target = result
        .target
        .strip_prefix(scope)
        .unwrap_or(&result.target)
        .display()
        .to_string();
    let header = format!(
        "# Deps: {} — {} local, {} external, {} dependent{}",
        rel_target,
        result.uses_local.len(),
        result.uses_external.len(),
        dep_count_label,
        if dep_count == 1 { "" } else { "s" },
    );

    let uses_local_section = format_uses_local(&result.uses_local[..local_limit], scope, true);
    let uses_external_section = format_uses_external(&result.uses_external);
    let used_by_section = format_used_by(&prod_deps, scope, "## Used by");
    let used_by_tests_section = format_used_by(&test_deps, scope, "## Used by (tests)");

    let mut barrel_note = if result.searched_count < result.exported_count {
        format!(
            "\n\n> ({} of {} exported symbols searched)",
            result.searched_count, result.exported_count
        )
    } else {
        String::new()
    };
    if !result.dependents_complete {
        barrel_note.push_str("\n\n> Reverse search was capped; dependent counts are lower bounds. Use full output to search all symbols and call sites.");
    }
    let hidden_local = result.uses_local.len() - local_limit;
    if hidden_local > 0 {
        let _ = write!(
            barrel_note,
            "\n\n... and {hidden_local} more local dependencies"
        );
    }
    let hidden_dependents = result.total_dependents.saturating_sub(dependent_limit);
    if hidden_dependents > 0 {
        let _ = write!(
            barrel_note,
            "\n\n... and {hidden_dependents} more dependents"
        );
    }

    // Full output
    let mut parts: Vec<String> = Vec::new();
    parts.push(header.clone());
    if !uses_local_section.is_empty() {
        parts.push(uses_local_section.clone());
    }
    if !uses_external_section.is_empty() {
        parts.push(uses_external_section.clone());
    }
    if !used_by_section.is_empty() {
        parts.push(used_by_section.clone());
    }
    if !used_by_tests_section.is_empty() {
        parts.push(used_by_tests_section.clone());
    }
    if !barrel_note.is_empty() {
        parts.push(barrel_note.clone());
    }

    let full = parts.join("\n\n");
    let full_tokens = crate::types::estimate_tokens(full.len() as u64) as usize;

    let output = match budget {
        None => full,
        Some(b) if full_tokens <= b => full,
        Some(b) => {
            // Apply truncation in priority order
            apply_budget_truncation(
                &header,
                &uses_local_section,
                &uses_external_section,
                &prod_deps,
                &test_deps,
                &barrel_note,
                scope,
                b,
            )
        }
    };

    let token_est = crate::types::estimate_tokens(output.len() as u64);
    format!("{output}\n\n[~{token_est} tokens]")
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Collect symbol names from an outline entry and its children.
fn collect_symbol_names(entry: &crate::types::OutlineEntry, out: &mut Vec<String>) {
    out.push(entry.name.clone());
    for child in &entry.children {
        // Include public methods of classes/structs/impls
        if !matches!(child.kind, OutlineKind::Import | OutlineKind::Export) {
            out.push(child.name.clone());
        }
    }
}

/// Returns true if the name is a noise/placeholder that should be excluded
/// from the reverse-dependency search.
fn is_placeholder_name(name: &str) -> bool {
    if name == "<anonymous>" {
        return true;
    }
    if name.starts_with('<') {
        return true;
    }
    if name.starts_with("impl ") {
        return true;
    }
    // Single-character names are too generic (e.g. `T`, `E`, `f`)
    if name.chars().count() == 1 {
        return true;
    }
    false
}

/// Returns true if the import source is a standard library module.
/// Agents can't navigate into stdlib — showing these is noise.
fn is_stdlib(source: &str, lang: crate::types::Lang) -> bool {
    crate::lang::spec::spec(lang).stdlib.matches(source)
}

/// Returns true if the string looks like a valid module/package path.
/// Filters out garbage from string literals that pass `is_import_line`.
fn is_valid_module_path(source: &str) -> bool {
    // Must not contain spaces (real module paths don't)
    if source.contains(' ') {
        return false;
    }
    // Must start with an alphanumeric, @, or dot
    source
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '@' || c == '.')
}

use crate::types::is_test_file;

/// Format the "Uses (local)" section.
fn format_uses_local(deps: &[LocalDep], scope: &Path, with_symbols: bool) -> String {
    if deps.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Uses (local)");
    for dep in deps {
        let rel = dep
            .path
            .strip_prefix(scope)
            .unwrap_or(&dep.path)
            .display()
            .to_string();
        if with_symbols && !dep.symbols.is_empty() {
            let _ = write!(out, "\n{:<30} {}", rel, dep.symbols.join(", "));
        } else {
            let _ = write!(out, "\n{rel}");
        }
    }
    out
}

/// Format the "Uses (external)" section.
fn format_uses_external(externals: &[String]) -> String {
    if externals.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Uses (external)");
    for ext in externals {
        let _ = write!(out, "\n{ext}");
    }
    out
}

/// Format a "Used by" section from a slice of dependents.
fn format_used_by(deps: &[&Dependent], scope: &Path, heading: &str) -> String {
    if deps.is_empty() {
        return String::new();
    }
    let mut out = String::from(heading);
    for dep in deps {
        let rel = dep
            .path
            .strip_prefix(scope)
            .unwrap_or(&dep.path)
            .display()
            .to_string();
        if dep.symbols.is_empty() {
            let _ = write!(out, "\n{rel} (imports file)");
            continue;
        }
        // Group by (caller, line) for readability — keep the earliest line per caller
        let mut by_caller: HashMap<&str, (u32, Vec<&str>)> = HashMap::new();
        for (caller, symbol, line) in &dep.symbols {
            let entry = by_caller
                .entry(caller.as_str())
                .or_insert((*line, Vec::new()));
            entry.0 = entry.0.min(*line);
            if !entry.1.contains(&symbol.as_str()) {
                entry.1.push(symbol.as_str());
            }
        }
        let mut callers: Vec<(&str, u32, Vec<&str>)> = by_caller
            .into_iter()
            .map(|(caller, (line, syms))| (caller, line, syms))
            .collect();
        callers.sort_unstable_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(b.0)));
        for (caller, line, syms) in callers {
            let loc = format!("{rel}:{line}");
            let joined = syms.join(", ");
            let _ = write!(out, "\n{loc:<30} {caller:<20} \u{2192} {joined}");
        }
    }
    out
}

/// Apply progressive budget truncation and reassemble the output.
#[allow(clippy::too_many_arguments)]
fn apply_budget_truncation(
    header: &str,
    uses_local_full: &str,
    uses_external_full: &str,
    prod_deps: &[&Dependent],
    test_deps: &[&Dependent],
    barrel_note: &str,
    scope: &Path,
    budget: usize,
) -> String {
    // Try progressively degraded versions
    #[allow(clippy::type_complexity)]
    let candidates: &[fn(
        &str,
        &str,
        &str,
        &[&Dependent],
        &[&Dependent],
        &str,
        &Path,
    ) -> String] = &[
        // Level 0: no tests
        |hdr, ul, ue, pd, td, bn, sc| {
            let note = if td.is_empty() {
                String::new()
            } else {
                format!("... {} test dependents omitted to fit budget", td.len())
            };
            assemble(&[
                hdr,
                ul,
                ue,
                &format_used_by(pd, sc, "## Used by"),
                &note,
                bn,
            ])
        },
        // Level 1: no used-by entries at all
        |hdr, ul, ue, pd, td, bn, _sc| {
            let count = pd.len() + td.len();
            let note = if count > 0 {
                format!("\n\n(... {count} more dependents)")
            } else {
                String::new()
            };
            assemble(&[hdr, ul, ue, &note, bn])
        },
        // Level 2: external as count only
        |hdr, ul, _ue, _pd, _td, bn, _sc| assemble(&[hdr, ul, bn]),
        // Level 3: local as paths only (no symbols)
        |hdr, ul, _ue, _pd, _td, _bn, _sc| {
            // Strip symbol lists: each line is "path_padded  symbols" — take only up to first space run
            let local_lines: Vec<&str> = ul
                .lines()
                .skip(1) // skip heading
                .map(|l| l.split_whitespace().next().unwrap_or(l))
                .collect();
            let paths_only = if local_lines.is_empty() {
                String::new()
            } else {
                format!("## Uses (local)\n{}", local_lines.join("\n"))
            };
            assemble(&[hdr, &paths_only])
        },
        // Level 4: header only
        |hdr, _ul, _ue, _pd, _td, _bn, _sc| hdr.to_string(),
    ];

    for candidate_fn in candidates {
        let candidate = candidate_fn(
            header,
            uses_local_full,
            uses_external_full,
            prod_deps,
            test_deps,
            barrel_note,
            scope,
        );
        let tokens = crate::types::estimate_tokens(candidate.len() as u64) as usize;
        if tokens <= budget {
            return candidate;
        }
    }

    // Absolute fallback: just the header
    header.to_string()
}

/// Join non-empty parts with double newlines.
fn assemble(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|s| !s.trim().is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_limited_reverse_count_is_a_lower_bound() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.rs");
        fs::write(&target, "pub fn target() {}\n").unwrap();
        fs::write(
            root.path().join("large.rs"),
            format!(
                "/* {} */\nfn caller() {{ target(); }}\n",
                "x".repeat(510_000)
            ),
        )
        .unwrap();
        let bloom = crate::index::bloom::BloomFilterCache::new();
        let preview = analyze_deps(&target, root.path(), &bloom).unwrap();
        assert_eq!(preview.total_dependents, 0);
        assert!(!preview.dependents_complete);
        assert!(format_deps(&preview, root.path(), None).contains("at least 0 dependents"));
        let full = analyze_deps_with_options(&target, root.path(), &bloom, true).unwrap();
        assert_eq!(full.total_dependents, 1);
        assert!(full.dependents_complete);
    }

    #[test]
    fn full_searches_late_symbols_in_large_caller_files() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.rs");
        let mut source = String::new();
        for i in 0..30 {
            writeln!(source, "pub fn func{i:02}() {{}}").unwrap();
        }
        fs::write(&target, source).unwrap();
        fs::write(root.path().join("small.rs"), "fn caller() { func00(); }\n").unwrap();
        fs::write(
            root.path().join("large.rs"),
            format!(
                "/* {} */\nfn caller() {{ func29(); }}\n",
                "x".repeat(510_000)
            ),
        )
        .unwrap();
        let bloom = crate::index::bloom::BloomFilterCache::new();
        let preview = analyze_deps(&target, root.path(), &bloom).unwrap();
        assert!(!preview.dependents_complete);
        assert_eq!(preview.total_dependents, 1);
        let full = analyze_deps_with_options(&target, root.path(), &bloom, true).unwrap();
        assert!(full.dependents_complete);
        assert_eq!(full.total_dependents, 2);
        assert!(full.used_by.iter().any(|d| d.path.ends_with("large.rs")));
    }

    #[test]
    fn full_dependencies_preserve_counts_and_expand_display() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let mut source = String::new();
        for i in 0..37 {
            fs::write(root.join(format!("dep{i}.ts")), "export const value = 1;\n").unwrap();
            write!(
                source,
                "import {{\n value as value{i}\n}} from './dep{i}';\n"
            )
            .unwrap();
        }
        source.push_str("export function target() {}\n");
        let path = root.join("target.ts");
        fs::write(&path, source).unwrap();
        for i in 0..20 {
            fs::write(
                root.join(format!("user{i}.ts")),
                "import { target } from './target';\ntarget();\n",
            )
            .unwrap();
        }
        let result =
            analyze_deps(&path, root, &crate::index::bloom::BloomFilterCache::new()).unwrap();
        assert_eq!(result.uses_local.len(), 37);
        assert_eq!(result.total_dependents, 20);
        assert_eq!(result.used_by.len(), 20);
        let limited = format_deps(&result, root, Some(20_000));
        assert!(limited.contains("37 local"));
        assert!(limited.contains("20 dependents"));
        assert!(limited.contains("29"));
        let full = format_deps_with_options(&result, root, Some(20_000), true);
        for i in 0..37 {
            assert!(full.contains(&format!("dep{i}.ts")));
        }
        for i in 0..20 {
            assert!(full.contains(&format!("user{i}.ts")));
        }
        let small = format_deps_with_options(&result, root, Some(80), true);
        assert!(small.len() < full.len());
    }

    #[test]
    fn import_then_export_barrel_reports_transitive_dependents() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let target = root.join("service.ts");
        fs::write(&target, "export class Service {}\n").unwrap();
        fs::write(
            root.join("index.ts"),
            "import { Service } from './service'; export { Service };\n",
        )
        .unwrap();
        fs::write(
            root.join("client.ts"),
            "import { Service } from './index';\nnew Service();\n",
        )
        .unwrap();
        let result =
            analyze_deps(&target, root, &crate::index::bloom::BloomFilterCache::new()).unwrap();
        assert_eq!(result.total_dependents, 2);
        assert!(result.used_by.iter().any(|d| d.path.ends_with("client.ts")));
    }

    #[test]
    fn tsconfig_alias_imports_count_as_dependents_without_name_collisions() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("tsconfig.json"),
            r#"{
            // The project uses a path alias.
            "compilerOptions": {"baseUrl": ".", "paths": {"@svc/*": ["src/*",],},},
        }"#,
        )
        .unwrap();
        let target = root.join("src/service.ts");
        fs::write(&target, "export class Service { create() {} }\n").unwrap();
        fs::write(root.join("src/a.ts"),
            "import { Service } from '@svc/service';\nconst service = new Service(); service.create(); service.create();\n").unwrap();
        fs::write(
            root.join("src/b.ts"),
            "import { Service } from '@svc/service';\nexport const service = Service;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/unrelated.ts"),
            "class Other { create() {} }\nnew Other().create();\n",
        )
        .unwrap();
        let bloom = crate::index::bloom::BloomFilterCache::new();
        let result = analyze_deps(&target, root, &bloom).unwrap();
        assert_eq!(result.total_dependents, 2);
        let output = format_deps(&result, root, None);
        assert!(output.contains("src/a.ts"), "{output}");
        assert!(output.contains("src/b.ts"), "{output}");
        assert!(!output.contains("src/unrelated.ts"), "{output}");
        assert!(!output.contains("create, create"), "{output}");
    }

    #[test]
    fn export_only_ts_file_still_reports_importers() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"paths":{"@svc/*":["src/*"]}}}"#,
        )
        .unwrap();
        let barrel = root.join("src/index.ts");
        fs::write(&barrel, "export { Service } from './service';\n").unwrap();
        fs::write(root.join("src/service.ts"), "export class Service {}\n").unwrap();
        fs::write(
            root.join("src/client.ts"),
            "import { Service } from '@svc/index';\nconst service = new Service();\n",
        )
        .unwrap();
        let bloom = crate::index::bloom::BloomFilterCache::new();
        let result = analyze_deps(&barrel, root, &bloom).unwrap();
        assert_eq!(result.total_dependents, 1);
        assert!(format_deps(&result, root, None).contains("src/client.ts"));
    }

    #[test]
    fn ts_importers_are_not_limited_by_global_name_match_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let target = root.join("service.ts");
        fs::write(&target, "export class Service { create() {} }\n").unwrap();
        for index in 0..60 {
            fs::write(
                root.join(format!("caller{index}.ts")),
                "import { Service } from './service';\nnew Service().create();\n",
            )
            .unwrap();
        }
        let bloom = crate::index::bloom::BloomFilterCache::new();
        let result = analyze_deps(&target, root, &bloom).unwrap();
        assert_eq!(result.total_dependents, 60);
    }

    #[test]
    fn ts_runtime_extension_import_counts_as_dependent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let target = root.join("service.ts");
        fs::write(&target, "export class Service {}\n").unwrap();
        fs::write(
            root.join("client.ts"),
            "import { Service } from './service.js';\nnew Service();\n",
        )
        .unwrap();
        let bloom = crate::index::bloom::BloomFilterCache::new();
        assert_eq!(
            analyze_deps(&target, root, &bloom)
                .unwrap()
                .total_dependents,
            1
        );
    }

    #[test]
    fn go_stdlib_fmt_is_stdlib() {
        assert!(is_stdlib("fmt", crate::types::Lang::Go));
    }

    #[test]
    fn commonjs_consumers_and_forwarders_are_dependents() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let target = root.join("lib.js");
        fs::write(&target, "function run() {}\nmodule.exports = { run };\n").unwrap();
        fs::write(
            root.join("direct.js"),
            "const { run } = require('./lib');\n",
        )
        .unwrap();
        fs::write(
            root.join("index.js"),
            "module.exports = require('./lib');\n",
        )
        .unwrap();
        fs::write(
            root.join("indirect.js"),
            "const { run } = require('./index');\n",
        )
        .unwrap();
        let result =
            analyze_deps(&target, root, &crate::index::bloom::BloomFilterCache::new()).unwrap();
        assert_eq!(result.total_dependents, 3);
    }

    #[test]
    fn go_stdlib_fmtlib_is_not_stdlib() {
        // "fmtlib" is not a Go stdlib package—previously matched via starts_with("fmt")
        assert!(!is_stdlib("fmtlib", crate::types::Lang::Go));
    }

    #[test]
    fn go_stdlib_fmtutil_is_not_stdlib() {
        assert!(!is_stdlib("fmtutil", crate::types::Lang::Go));
    }

    #[test]
    fn go_stdlib_multi_segment_paths_are_stdlib() {
        // Regression: multi-segment stdlib imports (single-line form
        // `import "net/http"`) must classify as stdlib via their root segment.
        // The exact-match allowlist briefly regressed these to "external".
        for path in [
            "net/http",
            "encoding/json",
            "path/filepath",
            "crypto/sha256",
            "text/template",
            "container/list",
            "database/sql",
        ] {
            assert!(
                is_stdlib(path, crate::types::Lang::Go),
                "{path} should be classified as Go stdlib"
            );
        }
    }

    #[test]
    fn go_local_multi_segment_path_is_not_stdlib() {
        // A local/third-party multi-segment package whose root isn't stdlib.
        assert!(!is_stdlib("mypackage/sub", crate::types::Lang::Go));
    }

    #[test]
    fn go_local_package_without_dot_is_not_stdlib() {
        // A local package like "mypackage" has no dot but is NOT stdlib—
        // the old !source.contains('.') rule wrongly classified it as stdlib.
        assert!(!is_stdlib("mypackage", crate::types::Lang::Go));
    }

    #[test]
    fn go_external_dotted_path_is_not_stdlib() {
        assert!(!is_stdlib(
            "github.com/gin-gonic/gin",
            crate::types::Lang::Go
        ));
    }

    #[test]
    fn go_stdlib_cmp_and_maps_are_stdlib() {
        // Go 1.21+ added `cmp` and `maps` to the standard library.
        assert!(is_stdlib("cmp", crate::types::Lang::Go));
        assert!(is_stdlib("maps", crate::types::Lang::Go));
    }
}
