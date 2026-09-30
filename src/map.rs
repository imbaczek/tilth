use std::collections::BTreeMap;
use std::fmt::Write;
use std::path::{Path, PathBuf};

use crate::cache::OutlineCache;
use crate::lang::detect_file_type;
use crate::read::outline;
use crate::search::base_walk_builder;
use crate::types::{estimate_tokens, FileType};

/// Generate a structural codebase map.
/// Code files show symbol names from outline cache.
/// Non-code files show name + token estimate.
#[must_use]
pub fn generate(scope: &Path, depth: usize, budget: Option<u64>, cache: &OutlineCache) -> String {
    let mut tree: BTreeMap<PathBuf, Vec<FileEntry>> = BTreeMap::new();

    let walker = base_walk_builder(scope).max_depth(Some(depth + 1)).build();

    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let path = entry.path();
        let rel = path.strip_prefix(scope).unwrap_or(path);

        // Skip if deeper than requested
        let file_depth = rel.components().count().saturating_sub(1);
        if file_depth > depth {
            continue;
        }

        let parent = rel.parent().unwrap_or(Path::new("")).to_path_buf();
        let name = rel
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();

        let meta = std::fs::metadata(path).ok();
        let byte_len = meta.as_ref().map_or(0, std::fs::Metadata::len);
        let tokens = estimate_tokens(byte_len);

        let file_type = detect_file_type(path);
        let symbols = match file_type {
            FileType::Code(_) => {
                let mtime = meta
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

                let outline_str = cache.get_or_compute_with_cap(path, mtime, true, || {
                    // Best-effort: an unreadable file contributes no outline symbols rather than
                    // aborting the map. Errors are intentionally swallowed here.
                    let content = std::fs::read_to_string(path).unwrap_or_default();
                    let buf = content.as_bytes();
                    outline::generate(path, file_type, &content, buf, true)
                });

                Some(extract_symbol_names(&outline_str))
            }
            _ => None,
        };

        tree.entry(parent.clone()).or_default().push(FileEntry {
            name,
            symbols,
            tokens,
        });

        // Ensure all ancestor directories exist in the tree so format_tree can find them.
        let mut ancestor = parent.parent();
        while let Some(a) = ancestor {
            tree.entry(a.to_path_buf()).or_default();
            if a == Path::new("") {
                break;
            }
            ancestor = a.parent();
        }
    }

    let mut out = format!("# Map: {} (depth {})\n", scope.display(), depth);
    let totals = compute_dir_totals(&tree);
    format_tree(&tree, &totals, Path::new(""), 0, &mut out);

    match budget {
        Some(b) => crate::budget::apply(&out, b),
        None => out,
    }
}

/// Sum tokens for every directory in the tree, including the implicit root
/// (`Path::new("")`). Each directory's total is the sum of its own files
/// plus the totals of all descendants. Computed by walking each directory's
/// direct files and folding their byte total into every ancestor.
fn compute_dir_totals(tree: &BTreeMap<PathBuf, Vec<FileEntry>>) -> BTreeMap<PathBuf, u64> {
    let mut totals: BTreeMap<PathBuf, u64> = BTreeMap::new();
    for (dir, files) in tree {
        let sum: u64 = files.iter().map(|f| f.tokens).sum();
        if sum == 0 {
            // Still need to seed the entry so format_tree can render the dir.
            totals.entry(dir.clone()).or_insert(0);
            continue;
        }
        let mut cur: Option<&Path> = Some(dir.as_path());
        while let Some(p) = cur {
            *totals.entry(p.to_path_buf()).or_insert(0) += sum;
            if p == Path::new("") {
                break;
            }
            cur = p.parent();
        }
    }
    totals
}

/// Compact human token count for directory rollups.
/// Uses the same scale as `tilth_list` output (`12.3k`, `1.2M`).
///
/// The k→M switchover triggers at `999_950` rather than `1_000_000` so values
/// that would round to `"1000.0k"` under `{:.1}` formatting roll cleanly into
/// the M tier (`"1.0M"`) instead of producing four-digit k labels.
fn fmt_tokens(n: u64) -> String {
    #[allow(clippy::cast_precision_loss)] // display-only; mantissa loss is fine for summaries
    let f = n as f64;
    if f >= 999_950.0 {
        format!("{:.1}M", f / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", f / 1_000.0)
    } else {
        n.to_string()
    }
}

struct FileEntry {
    name: String,
    symbols: Option<Vec<String>>,
    tokens: u64,
}

/// Extract symbol names from an outline string.
/// Outline lines look like: `[7-57]       fn classify`
/// We extract the last word(s) after the kind keyword.
fn extract_symbol_names(outline: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in outline.lines() {
        let trimmed = line.trim();
        let Some(payload) = outline_payload(trimmed) else {
            continue;
        };
        // Test outlines contain suite/test titles, not declaration signatures.
        // Preserve those labels rather than interpreting words inside them.
        if let Some(title) = payload
            .strip_prefix("suite: ")
            .or_else(|| payload.strip_prefix("test: "))
        {
            if !title.is_empty() {
                names.push(title.to_string());
            }
            continue;
        }
        // Skip import lines and empty lines
        if trimmed.starts_with('[') {
            // Find the symbol name after kind keywords
            if let Some(sig_start) = find_symbol_start(payload) {
                let sig = &payload[sig_start..];
                // Take just the name (up to first paren or space after name)
                let name = extract_name_from_sig(sig);
                if !name.is_empty() && name != "imports" {
                    names.push(name);
                }
            }
        }
    }
    names
}

fn outline_payload(line: &str) -> Option<&str> {
    let (range, payload) = line.strip_prefix('[')?.split_once(']')?;
    let mut bounds = range.split('-');
    let start = bounds.next()?.parse::<u32>().ok()?;
    if start == 0 {
        return None;
    }
    if let Some(end) = bounds.next() {
        if end.parse::<u32>().ok()? < start {
            return None;
        }
    }
    if bounds.next().is_some() {
        return None;
    }
    Some(payload.trim_start())
}

fn find_symbol_start(line: &str) -> Option<usize> {
    // Rust impl entries render as module entries named `impl Type`.
    // Preserve the implementing type rather than emitting the keyword `impl`.
    if line.starts_with("mod impl ") {
        return Some("mod impl ".len());
    }
    let kinds = [
        "fn ",
        "struct ",
        "enum ",
        "trait ",
        "impl ",
        "mod ",
        "class ",
        "interface ",
        "type ",
        "const ",
        "static ",
        "function ",
        "method ",
        "def ",
    ];
    for kind in &kinds {
        if line.starts_with(kind) {
            return Some(kind.len());
        }
    }
    None
}

fn extract_name_from_sig(sig: &str) -> String {
    // Take characters until we hit a non-identifier char
    sig.chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
        .collect()
}

fn format_tree(
    tree: &BTreeMap<PathBuf, Vec<FileEntry>>,
    totals: &BTreeMap<PathBuf, u64>,
    dir: &Path,
    indent: usize,
    out: &mut String,
) {
    // Collect subdirectories that have entries
    let mut subdirs: Vec<&PathBuf> = tree
        .keys()
        .filter(|k| k.parent() == Some(dir) && *k != dir)
        .collect();
    subdirs.sort();

    let prefix = "  ".repeat(indent);

    // Show files in this directory
    if let Some(files) = tree.get(dir) {
        for f in files {
            if let Some(ref symbols) = f.symbols {
                if symbols.is_empty() {
                    let _ = writeln!(out, "{prefix}{} (~{} tokens)", f.name, f.tokens);
                } else {
                    let syms = symbols.join(", ");
                    let truncated = if syms.len() > 80 {
                        format!("{}...", crate::types::truncate_str(&syms, 77))
                    } else {
                        syms
                    };
                    let _ = writeln!(out, "{prefix}{}: {truncated}", f.name);
                }
            } else {
                let _ = writeln!(out, "{prefix}{} (~{} tokens)", f.name, f.tokens);
            }
        }
    }

    // Recurse into subdirectories — annotate with cumulative token rollup so
    // agents can triage which subtrees are worth descending into.
    for subdir in subdirs {
        let dir_name = subdir.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let total = totals.get(subdir).copied().unwrap_or(0);
        let _ = writeln!(out, "{prefix}{dir_name}/ (~{} tokens)", fmt_tokens(total));
        format_tree(tree, totals, subdir, indent + 1, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_impl_module_labels_keep_their_type_names() {
        let outline = "[1] struct Reader\n[2-4] mod impl Reader\n[3] fn parse()\n[5-7] mod impl Load for Reader\n[8-9] mod utilities\n";
        assert_eq!(
            extract_symbol_names(outline),
            ["Reader", "Reader", "parse", "Load", "utilities"]
        );
    }

    #[test]
    fn declaration_labels_are_anchored_and_diagnostics_are_not_symbols() {
        let outline = "[1-9] fn parse(value: impl Trait)\n continuation fn not_a_symbol\n[10] struct Reader\n[11-] imports: class Imported\n[12] note: fn invented\n[generated] fn fake\n[13-14-15] fn malformed\n[16] fn café()\n";
        assert_eq!(extract_symbol_names(outline), ["parse", "Reader", "café"]);
        for invalid in [
            "[1-] fn fake",
            "[0] fn fake",
            "[0-0] fn fake",
            "[9-1] fn fake",
            "[1-2-3] fn fake",
        ] {
            assert!(extract_symbol_names(invalid).is_empty(), "{invalid}");
        }
    }

    #[test]
    fn test_titles_do_not_turn_declaration_words_into_symbols() {
        let outline =
            "[1] suite: describe(\"fn and class member\")\n  [2] test: it(\"method check\")\n";
        assert_eq!(
            extract_symbol_names(outline),
            ["describe(\"fn and class member\")", "it(\"method check\")"]
        );
    }

    #[test]
    fn spec_map_preserves_test_titles_instead_of_inventing_identifiers() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("service.spec.ts"),
            "describe('fn and class member', () => {\n it('method check', () => {});\n});\n",
        )
        .unwrap();
        let output = generate(root.path(), 2, None, &OutlineCache::new());
        assert!(
            output.contains("describe(\"fn and class member\")"),
            "{output}"
        );
        assert!(output.contains("it(\"method check\")"), "{output}");
        assert!(!output.contains("service.spec.ts: and"), "{output}");
    }

    fn entry(name: &str, tokens: u64) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            symbols: None,
            tokens,
        }
    }

    #[test]
    fn rollup_sums_descendants_into_each_ancestor() {
        // Layout:
        //   src/lang/  (file_a 100, file_b 50)        → 150
        //   src/search/ (file_c 200)                  → 200
        //   src/  (only subdirs, no direct files)     → 350
        //   ""    (root)                              → 350
        let mut tree: BTreeMap<PathBuf, Vec<FileEntry>> = BTreeMap::new();
        tree.insert(PathBuf::from(""), vec![]);
        tree.insert(PathBuf::from("src"), vec![]);
        tree.insert(
            PathBuf::from("src/lang"),
            vec![entry("a.rs", 100), entry("b.rs", 50)],
        );
        tree.insert(PathBuf::from("src/search"), vec![entry("c.rs", 200)]);

        let totals = compute_dir_totals(&tree);
        assert_eq!(totals.get(&PathBuf::from("src/lang")).copied(), Some(150));
        assert_eq!(totals.get(&PathBuf::from("src/search")).copied(), Some(200));
        assert_eq!(totals.get(&PathBuf::from("src")).copied(), Some(350));
        assert_eq!(totals.get(&PathBuf::from("")).copied(), Some(350));
    }

    #[test]
    fn rollup_handles_empty_directories() {
        let mut tree: BTreeMap<PathBuf, Vec<FileEntry>> = BTreeMap::new();
        tree.insert(PathBuf::from("empty"), vec![]);
        let totals = compute_dir_totals(&tree);
        assert_eq!(totals.get(&PathBuf::from("empty")).copied(), Some(0));
    }

    #[test]
    fn fmt_tokens_thresholds() {
        assert_eq!(fmt_tokens(0), "0");
        assert_eq!(fmt_tokens(999), "999");
        assert_eq!(fmt_tokens(1_000), "1.0k");
        assert_eq!(fmt_tokens(12_345), "12.3k");
        assert_eq!(fmt_tokens(1_000_000), "1.0M");
        assert_eq!(fmt_tokens(2_500_000), "2.5M");
    }

    /// Boundary: values that would round to "1000.0k" under `{:.1}`
    /// formatting must roll over to the M tier instead of producing a
    /// four-digit k label.
    #[test]
    fn fmt_tokens_rolls_over_at_999_950() {
        // Just below the rollover boundary — still formats as k.
        assert_eq!(fmt_tokens(999_949), "999.9k");
        // At and above the boundary — formats as M, not "1000.0k".
        assert_eq!(fmt_tokens(999_950), "1.0M");
        assert_eq!(fmt_tokens(999_999), "1.0M");
        assert_eq!(fmt_tokens(1_499_999), "1.5M");
    }

    #[test]
    fn format_tree_renders_dir_rollups_alongside_files() {
        let mut tree: BTreeMap<PathBuf, Vec<FileEntry>> = BTreeMap::new();
        tree.insert(PathBuf::from(""), vec![entry("README.md", 800)]);
        tree.insert(PathBuf::from("src"), vec![entry("main.rs", 4_200)]);
        let totals = compute_dir_totals(&tree);

        let mut out = String::new();
        format_tree(&tree, &totals, Path::new(""), 0, &mut out);

        assert!(out.contains("README.md (~800 tokens)"));
        // Subdir line: only main.rs lives under src/, so the rollup must
        // be exactly 4.2k. The previous OR-form hid the dead branch.
        assert!(
            out.contains("src/ (~4.2k tokens)"),
            "expected exact 'src/ (~4.2k tokens)' rollup, got: {out}"
        );
    }
}
