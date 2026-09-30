use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use streaming_iterator::StreamingIterator;

use crate::error::TilthError;
use crate::lang::detect_file_type;
use crate::lang::outline::outline_language;
use crate::types::FileType;

#[cfg(test)]
const MAX_MATCHES: usize = 10;
/// Max unique caller functions to trace for 2nd hop. Above this = wide fan-out, skip.
const IMPACT_FANOUT_THRESHOLD: usize = 10;
/// Max 2nd-hop results to display.
const IMPACT_MAX_RESULTS: usize = 15;
/// Raw-match limit for bounded internal analyses. Caller queries collect fully
/// and apply their preview limits only after ranking.
pub(crate) const BATCH_EARLY_QUIT: usize = 50;

/// Display cap when `--full` is set. Mirrors symbol/content search previews.
#[cfg(test)]
const FULL_MAX_MATCHES: usize = 100;

/// A single caller match — a call site of a target symbol.
#[derive(Debug)]
pub struct CallerMatch {
    pub path: PathBuf,
    pub line: u32,
    pub calling_function: String,
    pub call_text: String,
    /// Object before a JavaScript/TypeScript member call, if any.
    pub receiver: Option<String>,
    /// Type named by the receiver's nearest lexical binding, when available.
    pub receiver_type: Option<String>,
    /// Byte position where the receiver's type or constructor name is bound.
    pub receiver_type_site: Option<usize>,
    pub receiver_site: Option<usize>,
    /// Exact byte range of the enclosing class, independent of line layout.
    pub enclosing_type_range: Option<(usize, usize)>,
    /// Line range of the calling function (for expand).
    pub caller_range: Option<(u32, u32)>,
    /// The call sits in test code that only the source says is test code: a
    /// Rust `#[test]` function, a `#[cfg(test)]` module, or a file under a
    /// crate's `tests/` directory. `is_test_file` sees only path conventions.
    pub in_test: bool,
    /// File content, already read during `find_callers_batch` — avoids re-reading during expand.
    /// Shared across all call sites in the same file via reference counting.
    pub content: Arc<String>,
}

/// Scan `scope` for the literal `target` byte sequence. Used by the
/// single-symbol `search_callers_expanded` path to distinguish "typo,
/// doesn't exist" from "real symbol with no direct callers" (indirect
/// dispatch, dead code, framework registration, …) when the caller walk
/// returned zero matches. mmap is lazy, so the scan only pages in regions
/// that contain the needle prefix.
fn target_seen_in_scope(target: &str, scope: &Path, glob: Option<&str>) -> bool {
    let Ok(walker) = super::walker(scope, glob) else {
        return false;
    };
    let needle = target.as_bytes();
    let seen = AtomicBool::new(false);

    walker.run(|| {
        let seen = &seen;
        Box::new(move |entry| {
            if seen.load(Ordering::Relaxed) {
                return ignore::WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return ignore::WalkState::Continue;
            }
            let path = entry.path();
            let Ok(file) = std::fs::File::open(path) else {
                return ignore::WalkState::Continue;
            };
            let Ok(mmap) = (unsafe { memmap2::Mmap::map(&file) }) else {
                return ignore::WalkState::Continue;
            };
            if memchr::memmem::find(&mmap, needle).is_some() {
                seen.store(true, Ordering::Relaxed);
                return ignore::WalkState::Quit;
            }
            ignore::WalkState::Continue
        })
    });

    seen.load(Ordering::Relaxed)
}

/// Find all call sites of any symbol in `targets` across the codebase using a single walk.
/// Returns tuples of (`target_name`, match) so callers know which symbol was matched.
pub(crate) fn find_callers_batch(
    targets: &HashSet<String>,
    scope: &Path,
    bloom: &crate::index::bloom::BloomFilterCache,
    glob: Option<&str>,
    early_quit_threshold: usize,
) -> Result<Vec<(String, CallerMatch)>, TilthError> {
    find_callers_batch_with_size_limit(
        targets,
        scope,
        bloom,
        glob,
        early_quit_threshold,
        super::bloom_walk::MAX_FILE_SIZE,
    )
    .map(|(matches, _)| matches)
}

/// Caller queries need the complete analyzed set for totals and ranking, even
/// when a common name or a large source file would exhaust an internal cap.
fn find_all_callers_batch(
    targets: &HashSet<String>,
    scope: &Path,
    bloom: &crate::index::bloom::BloomFilterCache,
    glob: Option<&str>,
) -> Result<Vec<(String, CallerMatch)>, TilthError> {
    find_callers_batch_with_size_limit(targets, scope, bloom, glob, usize::MAX, u64::MAX)
        .map(|(matches, _)| matches)
}

pub(crate) fn find_callers_batch_with_size_limit(
    targets: &HashSet<String>,
    scope: &Path,
    bloom: &crate::index::bloom::BloomFilterCache,
    glob: Option<&str>,
    early_quit_threshold: usize,
    max_file_size: u64,
) -> Result<(Vec<(String, CallerMatch)>, bool), TilthError> {
    let matches: Mutex<Vec<(String, CallerMatch)>> = Mutex::new(Vec::new());
    let found_count = AtomicUsize::new(0);
    let skipped_large = std::sync::atomic::AtomicBool::new(false);

    let walker = super::walker(scope, glob)?;

    walker.run(|| {
        let matches = &matches;
        let found_count = &found_count;
        let skipped_large = &skipped_large;

        Box::new(move |entry| {
            // Early termination: enough callers found
            if found_count.load(Ordering::Relaxed) >= early_quit_threshold {
                return ignore::WalkState::Quit;
            }

            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };

            if !entry.file_type().is_some_and(|ft| ft.is_file()) {
                return ignore::WalkState::Continue;
            }

            let path = entry.path();

            if matches!(detect_file_type(path), FileType::Code(_))
                && std::fs::metadata(path).is_ok_and(|meta| meta.len() > max_file_size)
            {
                skipped_large.store(true, Ordering::Relaxed);
                return ignore::WalkState::Continue;
            }

            // Read + size-gate + bloom prefilter in one shared step.
            let Some((content, _mtime)) =
                super::bloom_walk::read_with_bloom_check(path, targets, bloom, max_file_size)
            else {
                return ignore::WalkState::Continue;
            };

            // Fast byte check via memchr::memmem (SIMD) — cheap second pass that
            // eliminates bloom false positives before tree-sitter parses.
            if !targets
                .iter()
                .any(|t| memchr::memmem::find(content.as_bytes(), t.as_bytes()).is_some())
            {
                return ignore::WalkState::Continue;
            }

            // Only process files with tree-sitter grammars
            let file_type = detect_file_type(path);
            let FileType::Code(lang) = file_type else {
                return ignore::WalkState::Continue;
            };

            let Some(ts_lang) = outline_language(lang) else {
                return ignore::WalkState::Continue;
            };

            let file_callers =
                find_callers_treesitter_batch(path, scope, targets, &ts_lang, &content, lang);

            if !file_callers.is_empty() {
                found_count.fetch_add(file_callers.len(), Ordering::Relaxed);
                let mut all = matches
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                all.extend(file_callers);
            }

            ignore::WalkState::Continue
        })
    });

    let complete = !skipped_large.load(Ordering::Relaxed)
        && found_count.load(Ordering::Relaxed) < early_quit_threshold;
    Ok((
        matches
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        complete,
    ))
}

/// Tree-sitter call site detection for a set of target symbols.
/// Returns tuples of (`matched_target_name`, `CallerMatch`).
pub(crate) fn find_callers_treesitter_batch(
    path: &Path,
    scope: &Path,
    targets: &HashSet<String>,
    ts_lang: &tree_sitter::Language,
    content: &str,
    lang: crate::types::Lang,
) -> Vec<(String, CallerMatch)> {
    // Get the query string for this language
    let Some(query_str) = super::callee_query::callee_query_str(lang) else {
        return Vec::new();
    };

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(ts_lang).is_err() {
        return Vec::new();
    }

    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };

    let content_bytes = content.as_bytes();
    let lines: Vec<&str> = content.lines().collect();

    // One Arc per file — all call sites share the same allocation.
    let shared_content: Arc<String> = Arc::new(content.to_string());

    let callee_filter = crate::lang::spec::spec(lang).callee_filter;
    // Test code the path alone does not reveal (see `test_site`).
    let test_site = crate::lang::spec::spec(lang).test_site;

    let Some(callers) = super::callee_query::with_callee_query(ts_lang, query_str, |query| {
        let Some(callee_idx) = query.capture_index_for_name("callee") else {
            return Vec::new();
        };

        let mut cursor = tree_sitter::QueryCursor::new();
        let mut matches = cursor.matches(query, tree.root_node(), content_bytes);
        let mut callers = Vec::new();

        while let Some(m) = matches.next() {
            for cap in m.captures() {
                if cap.index != callee_idx {
                    continue;
                }

                // Check if the captured text matches any of our target symbols
                let Ok(text) = cap.node.utf8_text(content_bytes) else {
                    continue;
                };

                if !targets.contains(text) {
                    continue;
                }

                // Some patterns match more than a call (see `callee_filter`).
                if callee_filter.is_some_and(|keep| !keep(&cap.node)) {
                    continue;
                }

                let matched_target = text.to_string();

                // Found a call site! Now walk up to find the calling function
                let line = cap.node.start_position().row as u32 + 1;

                // Get the call text (the whole call expression, not just the
                // callee). When the parent spans lines no single line is the
                // call, so show the callee's own line instead of the bare name.
                // Applies to every language; a macro's token tree makes it the
                // common case for Rust.
                let call_node = cap.node.parent().unwrap_or(cap.node);
                let same_line = call_node.start_position().row == call_node.end_position().row;
                let row = if same_line {
                    call_node.start_position().row
                } else {
                    cap.node.start_position().row
                };
                let call_text: String = lines
                    .get(row)
                    .map_or_else(|| matched_target.clone(), |l| l.trim().to_string());
                let receiver_node = if matches!(
                    lang,
                    crate::types::Lang::TypeScript
                        | crate::types::Lang::Tsx
                        | crate::types::Lang::JavaScript
                ) {
                    cap.node
                        .parent()
                        .filter(|parent| parent.kind() == "member_expression")
                        .and_then(|parent| parent.child_by_field_name("object"))
                } else {
                    None
                };
                let receiver = receiver_node
                    .and_then(|object| object.utf8_text(content_bytes).ok())
                    .map(str::to_string);
                let receiver_type = receiver_node
                    .and_then(|object| js_receiver_type(object, cap.node, content_bytes));
                let receiver_type_site = receiver_type.as_ref().map(|(_, site)| *site);
                let receiver_type = receiver_type.map(|(ty, _)| ty);
                let receiver_site = receiver_node.map(|node| node.start_byte());
                let mut enclosing_type_range = None;
                let mut ancestor = cap.node.parent();
                while let Some(node) = ancestor {
                    if matches!(node.kind(), "class_declaration" | "class") {
                        enclosing_type_range = Some((node.start_byte(), node.end_byte()));
                        break;
                    }
                    ancestor = node.parent();
                }

                // Walk up the tree to find the enclosing function
                let (calling_function, caller_range) =
                    find_enclosing_function(cap.node, &lines, lang);

                callers.push((
                    matched_target,
                    CallerMatch {
                        path: path.to_path_buf(),
                        line,
                        calling_function,
                        call_text,
                        receiver,
                        receiver_type,
                        receiver_type_site,
                        receiver_site,
                        enclosing_type_range,
                        caller_range,
                        in_test: test_site
                            .is_some_and(|in_test| in_test(&cap.node, content_bytes, path, scope)),
                        content: Arc::clone(&shared_content),
                    },
                ));
            }
        }

        callers
    }) else {
        return Vec::new();
    };

    callers
}

fn js_receiver_type(
    object: tree_sitter::Node,
    call: tree_sitter::Node,
    bytes: &[u8],
) -> Option<(String, usize)> {
    let mut this_property = false;
    let name = match object.kind() {
        "new_expression" => {
            return new_expression_type(object, bytes).map(|ty| (ty, object.start_byte()))
        }
        "identifier" => object.utf8_text(bytes).ok()?,
        "member_expression" => {
            let base = object
                .child_by_field_name("object")?
                .utf8_text(bytes)
                .ok()?;
            if base != "this" {
                return None;
            }
            this_property = true;
            object
                .child_by_field_name("property")?
                .utf8_text(bytes)
                .ok()?
        }
        _ => return None,
    };
    if this_property {
        return this_property_type(call, name, bytes);
    }
    let call_byte = call.start_byte();
    let mut ancestor = call.parent();
    while let Some(scope) = ancestor {
        if matches!(
            scope.kind(),
            "statement_block"
                | "class_body"
                | "function_declaration"
                | "function_expression"
                | "generator_function"
                | "generator_function_declaration"
                | "method_definition"
                | "arrow_function"
                | "program"
        ) {
            let mut best = None;
            collect_js_binding(scope, name, call_byte, bytes, &mut best, true);
            if let Some((site, ty)) = best {
                return Some((ty, site));
            }
        }
        ancestor = scope.parent();
    }
    None
}

fn this_property_type(
    call: tree_sitter::Node,
    name: &str,
    bytes: &[u8],
) -> Option<(String, usize)> {
    let mut ancestor = call.parent();
    while let Some(node) = ancestor {
        if node.kind() == "class_body" {
            let mut cursor = node.walk();
            for method in node.named_children(&mut cursor) {
                if matches!(
                    method.kind(),
                    "public_field_definition" | "field_definition"
                ) {
                    let mut best = None;
                    collect_js_binding(method, name, usize::MAX, bytes, &mut best, true);
                    if let Some((site, ty)) = best {
                        return Some((ty, site));
                    }
                }
                if method.kind() != "method_definition" {
                    continue;
                }
                let method_name = method
                    .child_by_field_name("name")
                    .and_then(|name| name.utf8_text(bytes).ok());
                if method_name != Some("constructor") {
                    continue;
                }
                let mut method_cursor = method.walk();
                let params = method
                    .named_children(&mut method_cursor)
                    .find(|child| child.kind() == "formal_parameters");
                let Some(params) = params else { continue };
                let mut params_cursor = params.walk();
                for param in params.named_children(&mut params_cursor) {
                    let Ok(text) = param.utf8_text(bytes) else {
                        continue;
                    };
                    let Some((before, after)) = text.split_once(':') else {
                        continue;
                    };
                    if !before
                        .split_whitespace()
                        .any(|word| matches!(word, "private" | "public" | "protected"))
                    {
                        continue;
                    }
                    if before
                        .split_whitespace()
                        .last()
                        .map(|word| word.trim_end_matches('?'))
                        == Some(name)
                    {
                        return type_name(after).map(|ty| (ty, param.start_byte()));
                    }
                }
            }
            return None;
        }
        ancestor = node.parent();
    }
    None
}

fn collect_js_binding(
    node: tree_sitter::Node,
    name: &str,
    call_byte: usize,
    bytes: &[u8],
    best: &mut Option<(usize, String)>,
    root: bool,
) {
    if node.start_byte() > call_byte {
        return;
    }
    if !root
        && matches!(
            node.kind(),
            "statement_block"
                | "class_body"
                | "function_declaration"
                | "function_expression"
                | "generator_function"
                | "generator_function_declaration"
                | "method_definition"
                | "arrow_function"
                | "class_declaration"
                | "class"
        )
        && !(node.start_byte() <= call_byte && call_byte < node.end_byte())
    {
        return;
    }
    let declared_name = match node.kind() {
        "variable_declarator" | "public_field_definition" => node
            .child_by_field_name("name")
            .and_then(|name| name.utf8_text(bytes).ok()),
        "required_parameter" | "optional_parameter" => node
            .utf8_text(bytes)
            .ok()
            .and_then(|text| text.split(':').next())
            .and_then(|before| before.split_whitespace().last())
            .map(|name| name.trim_end_matches('?')),
        _ => None,
    };
    if declared_name == Some(name) {
        let ty = node
            .child_by_field_name("value")
            .and_then(|value| new_expression_type(value, bytes))
            .or_else(|| {
                let mut cursor = node.walk();
                let annotation = node
                    .named_children(&mut cursor)
                    .find(|child| child.kind() == "type_annotation")
                    .and_then(|annotation| annotation.utf8_text(bytes).ok())
                    .and_then(|text| type_name(text.trim_start_matches(':')));
                annotation
            })
            .unwrap_or_default();
        if best
            .as_ref()
            .is_none_or(|(byte, _)| node.start_byte() >= *byte)
        {
            *best = Some((node.start_byte(), ty));
        }
    }
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        collect_js_binding(child, name, call_byte, bytes, best, false);
    }
}

fn new_expression_type(node: tree_sitter::Node, bytes: &[u8]) -> Option<String> {
    if node.kind() != "new_expression" {
        return None;
    }
    let text = node.utf8_text(bytes).ok()?.strip_prefix("new ")?;
    type_name(text)
}

fn type_name(text: &str) -> Option<String> {
    let name: String = text
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '$' || *ch == '.')
        .collect();
    (!name.is_empty()).then_some(name)
}

/// Search a known set of files without a name-based early-exit threshold.
/// Used after import resolution has identified the files that can reference a
/// particular TypeScript module.
pub(crate) fn find_callers_in_files(
    files: &HashSet<std::path::PathBuf>,
    scope: &Path,
    targets: &HashSet<String>,
) -> Vec<(String, CallerMatch)> {
    let mut results = Vec::new();
    for path in files {
        let crate::types::FileType::Code(lang) = crate::lang::detect_file_type(path) else {
            continue;
        };
        let Some(ts_lang) = crate::lang::outline::outline_language(lang) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        if !targets.iter().any(|target| content.contains(target)) {
            continue;
        }
        results.extend(find_callers_treesitter_batch(
            path, scope, targets, &ts_lang, &content, lang,
        ));
    }
    results
}

/// Walk up the AST from a node to find the enclosing function definition.
/// Returns (`function_name`, `line_range`). Top-level renders as `"<top-level>"`.
fn find_enclosing_function(
    node: tree_sitter::Node,
    lines: &[&str],
    lang: crate::types::Lang,
) -> (String, Option<(u32, u32)>) {
    match super::scope::walk_to_enclosing_definition(node, lines, lang) {
        Some((_, name, range)) => (name, Some(range)),
        None => ("<top-level>".to_string(), None),
    }
}

/// Format and rank caller search results with optional expand.
#[cfg(test)]
pub fn search_callers_expanded(
    target: &str,
    scope: &Path,
    bloom: &crate::index::bloom::BloomFilterCache,
    expand: usize,
    context: Option<&Path>,
    glob: Option<&str>,
    full: bool,
) -> Result<String, TilthError> {
    search_callers_page(
        target,
        scope,
        bloom,
        expand,
        context,
        glob,
        (0, if full { FULL_MAX_MATCHES } else { MAX_MATCHES }),
    )
}

/// Render a zero-based page of ranked callers, with complete impact summaries.
pub fn search_callers_page(
    target: &str,
    scope: &Path,
    bloom: &crate::index::bloom::BloomFilterCache,
    expand: usize,
    context: Option<&Path>,
    glob: Option<&str>,
    page: (usize, usize),
) -> Result<String, TilthError> {
    let (offset, limit) = (page.0, page.1.max(1));
    let single: HashSet<String> = std::iter::once(target.to_string()).collect();
    let raw = find_all_callers_batch(&single, scope, bloom, glob)?;
    let callers: Vec<CallerMatch> = raw.into_iter().map(|(_, m)| m).collect();

    if callers.is_empty() {
        let target_seen = target_seen_in_scope(target, scope, glob);
        return Ok(no_callers_message(target, scope, target_seen, glob));
    }

    // Sort by relevance (context file first, then by proximity)
    let mut sorted_callers = callers;
    rank_callers(&mut sorted_callers, scope, context);

    let total = sorted_callers.len();

    // Collect unique caller names BEFORE truncation for accurate fan-out threshold
    let all_caller_names: HashSet<String> = sorted_callers
        .iter()
        .filter(|c| c.calling_function != "<top-level>")
        .map(|c| c.calling_function.clone())
        .collect();

    let all_direct_locations = sorted_callers
        .iter()
        .map(|c| (c.path.clone(), c.line))
        .collect();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let page_callers = &sorted_callers[start..end];

    let mut output = String::new();
    write_caller_bucket(
        &mut output,
        target,
        scope,
        total,
        page_callers,
        expand,
        (offset, limit),
    );
    write_second_hop_impact(
        &mut output,
        &all_caller_names,
        &all_direct_locations,
        scope,
        bloom,
        glob,
    );

    let tokens = crate::types::estimate_tokens(output.len() as u64);
    let _ = write!(
        output,
        "\n\n({} tokens)",
        crate::search::format_token_count(tokens)
    );
    Ok(output)
}

/// Render one target's caller bucket in the canonical shape shared by both
/// the single-target and multi-target callers search: a
/// `# Callers of "<target>" in <scope> — N call site(s)` header, then one
/// `## <path>:<line> [caller: <fn>]` block per call site (with an optional
/// expanded source excerpt). Multi-target search calls this once per target
/// so a bucket inside a comma query renders byte-identically to what a lone
/// single-target search of the same symbol, scope, and hits would produce.
fn write_caller_bucket(
    output: &mut String,
    target: &str,
    scope: &Path,
    total: usize,
    sorted_callers: &[CallerMatch],
    expand: usize,
    page: (usize, usize),
) {
    let _ = writeln!(
        output,
        "# Callers of \"{}\" in {} — {} call site{}",
        target,
        scope.display(),
        total,
        if total == 1 { "" } else { "s" }
    );

    let (offset, limit) = page;
    let start = offset.min(total);
    let end = start.saturating_add(sorted_callers.len()).min(total);
    if sorted_callers.is_empty() {
        let _ = writeln!(output, "> No call sites at offset {offset}; {total} found.");
    } else {
        let _ = write!(output, "> Showing call sites {}-{end} of {total} (offset {offset}, limit {limit}); {} call sites omitted.", start + 1, total - sorted_callers.len());
        if end < total {
            let _ = write!(output, " Next page: --offset {end} --limit {limit} (keep the same query, scope and filters).");
        }
        output.push('\n');
    }

    for (i, caller) in sorted_callers.iter().enumerate() {
        // Header: file:line [caller: calling_function]
        let _ = write!(
            output,
            "\n## {}:{} [caller: {}]\n",
            caller
                .path
                .strip_prefix(scope)
                .unwrap_or(&caller.path)
                .display(),
            caller.line,
            caller.calling_function
        );

        // Show the call text
        let _ = writeln!(output, "-> {}", caller.call_text);

        // Expand if requested and we have the range
        if i < expand {
            if let Some((start, end)) = caller.caller_range {
                // Use cached content — no re-read needed
                let lines: Vec<&str> = caller.content.lines().collect();
                let start_idx = (start as usize).saturating_sub(1);
                let end_idx = (end as usize).min(lines.len());

                output.push('\n');
                output.push_str("```\n");

                for (idx, line) in lines[start_idx..end_idx].iter().enumerate() {
                    let line_num = start_idx + idx + 1;
                    let prefix = if line_num == caller.line as usize {
                        "> "
                    } else {
                        "  "
                    };
                    let _ = writeln!(output, "{prefix}{line_num:4} | {line}");
                }

                output.push_str("```\n");
            }
        }
    }
}

/// Adaptive 2nd-hop impact analysis, shared by single- and multi-target
/// callers search (extracted so multi-target reuses this exact block per
/// target bucket instead of re-implementing it — PR #138 review HIGH
/// finding: the multi-target path originally omitted this entirely).
///
/// `all_caller_names` must be the target's unique direct-caller names
/// collected BEFORE preview truncation, so the fan-out threshold
/// check reflects the true hop-1 breadth rather than the display-capped one.
fn write_second_hop_impact(
    output: &mut String,
    all_caller_names: &HashSet<String>,
    all_direct_locations: &HashSet<(PathBuf, u32)>,
    scope: &Path,
    bloom: &crate::index::bloom::BloomFilterCache,
    glob: Option<&str>,
) {
    if all_caller_names.is_empty() || all_caller_names.len() > IMPACT_FANOUT_THRESHOLD {
        return;
    }
    let Ok(hop2) = find_all_callers_batch(all_caller_names, scope, bloom, glob) else {
        return;
    };

    // Filter out hop-1 matches (same file+line = same call site)
    let mut hop2_filtered: Vec<_> = hop2
        .into_iter()
        .filter(|(_, m)| !all_direct_locations.contains(&(m.path.clone(), m.line)))
        .collect();
    hop2_filtered.sort_by(|(a_via, a), (b_via, b)| {
        let a_rel = a.path.strip_prefix(scope).unwrap_or(&a.path);
        let b_rel = b.path.strip_prefix(scope).unwrap_or(&b.path);
        a_rel
            .components()
            .count()
            .cmp(&b_rel.components().count())
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
            .then_with(|| a_via.cmp(b_via))
    });

    if hop2_filtered.is_empty() {
        return;
    }

    output.push_str("\n-- impact (2nd hop) --\n");

    let mut seen: HashSet<(String, PathBuf)> = HashSet::new();
    let mut count = 0;
    for (via, m) in &hop2_filtered {
        let key = (m.calling_function.clone(), m.path.clone());
        if !seen.insert(key) {
            continue;
        }
        if count >= IMPACT_MAX_RESULTS {
            break;
        }

        let rel_path = m.path.strip_prefix(scope).unwrap_or(&m.path).display();
        let _ = writeln!(
            output,
            "  {:<20} {}:{}  -> {}",
            m.calling_function, rel_path, m.line, via
        );
        count += 1;
    }

    let unique_total = hop2_filtered
        .iter()
        .map(|(_, m)| (&m.calling_function, &m.path))
        .collect::<HashSet<_>>()
        .len();
    if unique_total > IMPACT_MAX_RESULTS {
        let _ = writeln!(
            output,
            "  ... and {} more",
            unique_total - IMPACT_MAX_RESULTS
        );
    }

    // Affected-count formula matches main's evolution: pre-truncation
    // hop-1 caller-name count + deduplicated hop-2 unique_total (NOT the
    // post-truncation display `count`, which under-reports once the
    // `IMPACT_MAX_RESULTS` cap above stops incrementing it while more
    // unique callers still exist beyond the cap).
    let _ = writeln!(
        output,
        "\n{} functions affected across 2 hops.",
        all_caller_names.len() + unique_total
    );
}

/// Multi-target caller search: find call sites of 2..=5 symbols in a single
/// walk via `find_callers_batch`, then render one labeled section per target.
/// Mirrors `search_multi_symbol_expanded` for the `kind=callers` comma path.
///
/// Each target's bucket renders via the same `write_caller_bucket` +
/// `write_second_hop_impact` helpers the single-target path uses, so a
/// bucket here is byte-identical to what a lone `search_callers_expanded`
/// call for that target would produce (PR #138 review: HIGH — 2nd-hop parity;
/// MED — header shape parity). Collection is complete before partitioning,
/// so a hit-rich target cannot starve a rarer one.
#[cfg(test)]
pub fn search_callers_multi_expanded(
    targets: &[&str],
    scope: &Path,
    bloom: &crate::index::bloom::BloomFilterCache,
    expand: usize,
    context: Option<&Path>,
    glob: Option<&str>,
    full: bool,
) -> Result<String, TilthError> {
    search_callers_multi_page(
        targets,
        scope,
        bloom,
        expand,
        context,
        glob,
        (0, if full { FULL_MAX_MATCHES } else { MAX_MATCHES }),
    )
}

/// Render the same zero-based ranked page independently for each target.
/// Collect once, and compute totals and impact from each complete target bucket.
pub fn search_callers_multi_page(
    targets: &[&str],
    scope: &Path,
    bloom: &crate::index::bloom::BloomFilterCache,
    expand: usize,
    context: Option<&Path>,
    glob: Option<&str>,
    page: (usize, usize),
) -> Result<String, TilthError> {
    let (offset, limit) = (page.0, page.1.max(1));

    // Dedupe targets, preserving first-seen order: a repeated target (e.g.
    // query "foo,foo") must not render an empty no-callers section on its
    // second occurrence after the first consumed the matched bucket. The
    // deduped list also feeds the batch search, so the input is deduped once.
    let mut seen: HashSet<&str> = HashSet::new();
    let ordered: Vec<&str> = targets
        .iter()
        .copied()
        .filter(|t| seen.insert(*t))
        .collect();

    let target_set: HashSet<String> = ordered.iter().map(ToString::to_string).collect();
    let raw = find_all_callers_batch(&target_set, scope, bloom, glob)?;

    // Bucket matches by which target they call. Preserve the caller-supplied
    // target order so output is deterministic.
    let mut by_target: std::collections::HashMap<String, Vec<CallerMatch>> =
        std::collections::HashMap::new();
    for (name, m) in raw {
        by_target.entry(name).or_default().push(m);
    }

    let mut output = String::new();
    for target in &ordered {
        let mut callers = by_target.remove(*target).unwrap_or_default();

        if callers.is_empty() {
            let target_seen = target_seen_in_scope(target, scope, glob);
            output.push_str(&no_callers_message(target, scope, target_seen, glob));
            output.push_str("\n\n");
            continue;
        }

        rank_callers(&mut callers, scope, context);
        let total = callers.len();

        // Unique direct-caller names BEFORE truncation, same as the
        // single-target path — feeds the 2nd-hop fan-out threshold check
        // with the true hop-1 breadth rather than the display-capped one.
        let all_caller_names: HashSet<String> = callers
            .iter()
            .filter(|c| c.calling_function != "<top-level>")
            .map(|c| c.calling_function.clone())
            .collect();

        let all_direct_locations = callers.iter().map(|c| (c.path.clone(), c.line)).collect();
        let start = offset.min(total);
        let end = start.saturating_add(limit).min(total);

        write_caller_bucket(
            &mut output,
            target,
            scope,
            total,
            &callers[start..end],
            expand,
            (offset, limit),
        );
        write_second_hop_impact(
            &mut output,
            &all_caller_names,
            &all_direct_locations,
            scope,
            bloom,
            glob,
        );
        output.push('\n');
    }

    let tokens = crate::types::estimate_tokens(output.len() as u64);
    // Single leading '\n' (not single-target's '\n\n'): the per-bucket loop
    // above already ends each target's section with its own `output.push('\n')`,
    // so a second blank line here would double up before the token count.
    let _ = write!(
        output,
        "\n({} tokens)",
        crate::search::format_token_count(tokens)
    );
    Ok(output)
}

/// Build the user-facing message when callers search returns no hits.
/// Splits two cases that mean very different things to an agent:
/// `target_seen = true` means the symbol exists somewhere but has no direct
/// call sites — probable indirect dispatch, so we show a richer hint
/// listing the common indirection mechanisms. `target_seen = false` means
/// the literal name never appears in scope — most often a typo or wrong
/// scope, so we suppress the indirect-dispatch hint to avoid misleading
/// the agent.
fn no_callers_message(target: &str, scope: &Path, target_seen: bool, glob: Option<&str>) -> String {
    if !target_seen {
        return format!(
            "# Callers of \"{target}\" in {scope_disp} — no call sites found\n\n\
             The name \"{target}\" does not appear anywhere in scope. \
             Check the spelling, or widen scope if you expected hits outside this directory.",
            scope_disp = scope.display()
        );
    }
    // Only mention glob-driven test exclusion when a glob was actually used.
    // Otherwise the line implies a filter that the caller didn't apply, which
    // would mislead an agent reasoning about what tilth searched.
    let glob_hint = if glob.is_some() {
        "\n  • test files (if `glob` excluded them)"
    } else {
        ""
    };
    format!(
        "# Callers of \"{target}\" in {scope_disp} — no direct call sites found\n\n\
         \"{target}\" appears in the codebase but has no syntactic call sites. \
         tilth detects only direct, by-name calls; this symbol may still be reachable via:\n\
         \n  • interface / trait dispatch (Rust `dyn Trait`, Go interface, Java/Kotlin abstract method)\
         \n  • reflection or dynamic dispatch (`getattr`, `Method::invoke`, `eval`)\
         \n  • framework registration (HTTP routes, JSON-RPC, plugin systems, decorators)\
         \n  • function values stored in maps, structs, or passed as callbacks{glob_hint}\n\
         \nVerify with `tilth_search \"{target}\"` to see how it's referenced before assuming dead code.",
        scope_disp = scope.display()
    )
}

/// Simple ranking: context file first, then by path length (proximity heuristic).
fn rank_callers(callers: &mut [CallerMatch], scope: &Path, context: Option<&Path>) {
    callers.sort_by(|a, b| {
        // Context file wins
        if let Some(ctx) = context {
            match (a.path == ctx, b.path == ctx) {
                (true, false) => return std::cmp::Ordering::Less,
                (false, true) => return std::cmp::Ordering::Greater,
                _ => {}
            }
        }

        // Shorter paths (more similar to scope) rank higher
        let a_rel = a.path.strip_prefix(scope).unwrap_or(&a.path);
        let b_rel = b.path.strip_prefix(scope).unwrap_or(&b.path);
        a_rel
            .components()
            .count()
            .cmp(&b_rel.components().count())
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caller_queries_collect_all_matches_before_ranking_and_target_partitioning() {
        let dir = tempfile::tempdir().unwrap();
        for file in 0..40 {
            let mut source = String::new();
            for i in 0..50 {
                writeln!(source, "fn caller_{file}_{i}() {{ hot(); }}").unwrap();
            }
            std::fs::write(dir.path().join(format!("file{file:02}.rs")), source).unwrap();
        }
        let preferred = dir.path().join("zz_preferred.rs");
        std::fs::write(
            &preferred,
            format!("fn best() {{ hot(); cold(); }}\n//{}", "x".repeat(500_001)),
        )
        .unwrap();
        let bloom = crate::index::bloom::BloomFilterCache::new();
        for full in [false, true] {
            let cap = if full { FULL_MAX_MATCHES } else { MAX_MATCHES };
            let single =
                search_callers_expanded("hot", dir.path(), &bloom, 0, Some(&preferred), None, full)
                    .unwrap();
            assert!(single.contains("— 2001 call sites"), "{single}");
            assert_eq!(single.matches("[caller:").count(), cap, "{single}");
            assert!(
                single
                    .lines()
                    .find(|line| line.starts_with("## "))
                    .unwrap()
                    .contains("zz_preferred.rs:1 [caller: best]"),
                "{single}"
            );
            let multi = search_callers_multi_expanded(
                &["hot", "cold", "hot"],
                dir.path(),
                &bloom,
                0,
                Some(&preferred),
                None,
                full,
            )
            .unwrap();
            assert!(
                multi.contains("— 2001 call sites") && multi.contains("— 1 call site"),
                "{multi}"
            );
            assert_eq!(multi.matches("# Callers of \"hot\"").count(), 1);
            assert_eq!(multi.matches("[caller:").count(), cap + 1, "{multi}");
            let cold =
                search_callers_expanded("cold", dir.path(), &bloom, 0, None, None, full).unwrap();
            assert!(
                cold.contains("— 1 call site") && cold.contains("[caller: best]"),
                "{cold}"
            );
        }
    }

    #[test]
    fn caller_query_second_hop_counts_complete_set_and_excludes_hidden_direct_sites() {
        let dir = tempfile::tempdir().unwrap();
        let bridge = format!(
            "fn bridge() {{\n{}}}\n",
            " target(); bridge();\n".repeat(40)
        );
        std::fs::write(dir.path().join("bridge.rs"), bridge).unwrap();
        for i in 0..160 {
            std::fs::write(
                dir.path().join(format!("hop{i:03}.rs")),
                format!("fn outer{i:03}() {{ bridge(); bridge(); bridge(); }}\n"),
            )
            .unwrap();
        }
        let bloom = crate::index::bloom::BloomFilterCache::new();
        let mut previous = None;
        for full in [false, true] {
            for _ in 0..2 {
                let result =
                    search_callers_expanded("target", dir.path(), &bloom, 0, None, None, full)
                        .unwrap();
                assert!(result.contains("— 40 call sites"), "{result}");
                let impact = result.split("-- impact (2nd hop) --\n").nth(1).unwrap();
                let impact = impact
                    .split("functions affected across 2 hops.")
                    .next()
                    .unwrap();
                assert!(
                    impact.ends_with("161 ") && impact.contains("... and 145 more"),
                    "{result}"
                );
                assert!(
                    !impact.contains("bridge.rs"),
                    "hidden direct sites must not become hop2: {result}"
                );
                for i in 0..15 {
                    assert!(impact.contains(&format!("outer{i:03}")), "{result}");
                }
                assert!(!impact.contains("outer015"), "{result}");
                if let Some(previous) = &previous {
                    assert_eq!(impact, previous);
                }
                previous = Some(impact.to_string());
            }
        }
    }

    #[test]
    fn no_callers_message_for_unseen_symbol_says_typo_or_scope() {
        let msg = no_callers_message("doesNotExist", Path::new("/repo"), false, None);
        assert!(msg.contains("does not appear anywhere in scope"));
        assert!(msg.contains("Check the spelling"));
        // Must NOT include the indirect-dispatch hint — that would mislead.
        assert!(!msg.contains("interface"));
        assert!(!msg.contains("reflection"));
    }

    #[test]
    fn no_callers_message_for_seen_symbol_lists_indirection_modes() {
        let msg = no_callers_message("Foo", Path::new("/repo"), true, None);
        assert!(msg.contains("appears in the codebase"));
        assert!(msg.contains("interface"));
        assert!(msg.contains("reflection"));
        assert!(msg.contains("framework registration"));
        assert!(msg.contains("Verify with `tilth_search"));
        // Must NOT pretend the symbol is missing — different signal than typo case.
        assert!(!msg.contains("does not appear"));
    }

    /// The "test files (if glob excluded them)" hint is only meaningful when
    /// the caller actually used a glob. Without a glob it would mislead an
    /// agent into thinking tilth filtered something it did not.
    #[test]
    fn no_callers_message_omits_glob_hint_when_no_glob() {
        let msg = no_callers_message("Foo", Path::new("/repo"), true, None);
        assert!(
            !msg.contains("test files"),
            "glob-driven hint must not appear when glob is None: {msg}"
        );
    }

    #[test]
    fn no_callers_message_includes_glob_hint_when_glob_set() {
        let msg = no_callers_message("Foo", Path::new("/repo"), true, Some("*.rs"));
        assert!(
            msg.contains("test files"),
            "glob-driven hint should appear when glob is Some: {msg}"
        );
    }
    /// A call inside a macro's arguments is parsed as raw tokens in a
    /// `token_tree`, not a `call_expression`, so the call-expression patterns
    /// alone never saw it — `write!(out, "{}", alpha(1))` reported no caller at
    /// all. The negative fixture is the shape the token-tree pattern matches
    /// too loosely and `callee_filter` has to reject.
    #[test]
    fn callers_sees_calls_inside_macro_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let bloom = crate::index::bloom::BloomFilterCache::new();

        std::fs::write(
            dir.path().join("a.rs"),
            "fn alpha(_n: u32) {}\n\
             fn macro_arg_caller(out: &mut String) { let _ = write!(out, \"{}\", alpha(1)); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("b.rs"),
            "fn tuple_not_a_call() { assert_eq!(alpha, (1, 2)); }\n",
        )
        .unwrap();

        let result =
            search_callers_expanded("alpha", dir.path(), &bloom, 0, None, None, false).unwrap();

        assert!(
            result.contains("[caller: macro_arg_caller]"),
            "call site inside macro arguments not reported:\n{result}"
        );
        assert!(
            !result.contains("tuple_not_a_call"),
            "a bare token before a tuple must not count as a call:\n{result}"
        );
    }

    /// `call_text` shows the call's source line. When the capture's parent
    /// spans lines there is no one line that is the call, so it falls back to
    /// the callee's own line — in every language, not only Rust. A macro's
    /// token tree nearly always spans lines, so without this the call sites the
    /// token-tree pattern finds would render as a bare echo of the query.
    #[test]
    fn call_text_falls_back_to_the_callee_line_across_languages() {
        let dir = tempfile::tempdir().unwrap();
        let bloom = crate::index::bloom::BloomFilterCache::new();

        std::fs::write(
            dir.path().join("wrapped.py"),
            "def target(n):\n    return n\n\ndef py_multi():\n    return target(\n        1,\n    )\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("wrapped.rs"),
            "fn target(n: u32) -> u32 { n }\n\
             fn rs_macro(out: &mut String) {\n\
             \x20   let _ = write!(\n\
             \x20       out,\n\
             \x20       \"{}\",\n\
             \x20       target(1),\n\
             \x20   );\n\
             }\n",
        )
        .unwrap();

        let result =
            search_callers_expanded("target", dir.path(), &bloom, 0, None, None, false).unwrap();

        assert!(
            result.contains("-> return target("),
            "a wrapped Python call should render its own line:\n{result}"
        );
        assert!(
            result.contains("-> target(1),"),
            "a call inside a multi-line macro should render its own line:\n{result}"
        );
    }

    /// Regression test: when there are more than `MAX_MATCHES` (10) hop-1 call
    /// sites but still <= `IMPACT_FANOUT_THRESHOLD` unique callers, the footer
    /// "N functions affected across 2 hops" must use the pre-truncation unique
    /// count, not the post-truncation count.
    ///
    /// Setup: 8 unique functions, each calling `target_fn` twice = 16 call
    /// sites. Truncation to `MAX_MATCHES=10` only keeps the first ~5 functions,
    /// dropping functions 6-8. The old code rebuilt the hop-1 set from
    /// `sorted_callers` AFTER truncation and undercounted. The fix uses
    /// `all_caller_names` (pre-truncation) which always holds 8.
    #[test]
    fn footer_count_uses_pre_truncation_caller_set() {
        let dir = tempfile::tempdir().unwrap();
        let bloom = crate::index::bloom::BloomFilterCache::new();

        // 8 files: each declares one function that calls `target_fn` twice.
        // Total: 16 call sites from 8 unique caller names.
        // One hop-2 file calls caller_a_0 so the 2nd-hop block fires.
        for i in 0..8usize {
            let content = format!(
                "fn target_fn() {{}}\
                \nfn caller_a_{i}() {{ target_fn(); target_fn(); }}\
                \n"
            );
            std::fs::write(dir.path().join(format!("f{i}.rs")), content).unwrap();
        }
        std::fs::write(
            dir.path().join("hop2.rs"),
            "fn hop2_fn() { caller_a_0(); }\n",
        )
        .unwrap();

        let result =
            search_callers_expanded("target_fn", dir.path(), &bloom, 0, None, None, false).unwrap();

        let footer_line = result
            .lines()
            .find(|l| l.contains("functions affected across 2 hops"))
            .unwrap_or_else(|| panic!("footer line missing from output:\n{result}"));

        let reported: usize = footer_line
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap_or_else(|_| panic!("footer count not a number: {footer_line}"));

        // hop-1 = 8 (all_caller_names, pre-truncation); hop-2 = 1 (hop2_fn → caller_a_0).
        assert_eq!(
            reported, 9,
            "footer reported {reported} but expected exactly 9 (8 hop-1 + 1 hop-2); \
             old post-truncation rebuild would undercount: {footer_line}"
        );
    }
}
