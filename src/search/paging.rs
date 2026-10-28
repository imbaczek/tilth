//! Ranked search pages shared by CLI and MCP. Unpaged search keeps its fast caps.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::cache::OutlineCache;
use crate::error::TilthError;
use crate::index::bloom::BloomFilterCache;
use crate::listing::Page;
use crate::session::Session;
use crate::types::{CountEstimate, SearchResult};

pub(crate) fn collect(
    query: &str,
    kind: &str,
    scopes: &[PathBuf],
    context: Option<&Path>,
    glob: Option<&str>,
    page: Page,
) -> Result<SearchResult, TilthError> {
    if kind == "concept" || kind == "fallthrough" {
        let mut primary = collect(
            query,
            if query.contains(' ') {
                "content"
            } else {
                "symbol"
            },
            scopes,
            context,
            glob,
            page,
        )?;
        let accept = if kind == "fallthrough" || query.contains(' ') {
            primary.total_found > 0
        } else {
            primary.definitions > 0
        };
        if accept {
            return Ok(primary);
        }
        let (fallback_query, fallback_kind) = if query.contains(' ') {
            let words: Vec<_> = query.split_whitespace().collect();
            let pattern = if words.len() == 2 {
                format!(
                    "{}.*{}|{}.*{}",
                    regex_syntax::escape(words[0]),
                    regex_syntax::escape(words[1]),
                    regex_syntax::escape(words[1]),
                    regex_syntax::escape(words[0])
                )
            } else {
                words
                    .iter()
                    .map(|word| regex_syntax::escape(word))
                    .collect::<Vec<_>>()
                    .join("|")
            };
            (pattern, "regex")
        } else {
            (query.to_string(), "content")
        };
        let mut fallback = collect(&fallback_query, fallback_kind, scopes, context, glob, page)?;
        crate::inherit_scope_failures(&mut fallback, &mut primary);
        fallback.query = query.into();
        if fallback.total_found > 0 {
            return Ok(fallback);
        }
        if kind == "concept" && primary.total_found > 0 {
            crate::inherit_scope_failures(&mut primary, &mut fallback);
            return Ok(primary);
        }
        if !fallback.scope_errors.is_empty() {
            return Ok(fallback);
        }
        let path = scopes
            .first()
            .map_or_else(|| PathBuf::from(query), |scope| scope.join(query));
        let suggestion = if scopes.len() == 1 {
            crate::read::suggest_similar_file(
                &scopes[0],
                query.split_whitespace().next().unwrap_or(query),
            )
        } else {
            None
        };
        return Err(TilthError::NotFound { path, suggestion });
    }
    let (pattern, is_regex) = if kind == "content" {
        super::parse_pattern(query)
    } else {
        (query, false)
    };
    super::merge::page_raw_scopes(
        pattern,
        if is_regex { "regex" } else { kind },
        scopes,
        context,
        glob,
        page.offset.saturating_add(page.limit),
    )
}

/// Render only the requested ranked matches, without reordering them into facets.
fn render(
    result: &SearchResult,
    scopes: &[PathBuf],
    page: Page,
    cache: &OutlineCache,
    session: &Session,
    bloom: &BloomFilterCache,
    expand: &mut usize,
    expanded_files: &mut HashSet<PathBuf>,
) -> (String, String) {
    let (start, end) = page.bounds(result.total_found);
    let mut out = super::search_header_for_scopes(result, scopes);
    if matches!(result.count_estimate, CountEstimate::Observed { .. }) {
        out = out.replacen(
            &format!("{} matches", result.total_found),
            &format!("at least {} matches", result.total_found),
            1,
        );
    }
    out.push_str("\n\n");
    let summary = page.summary(result.total_found, "matches");
    if matches!(result.count_estimate, CountEstimate::Observed { .. }) {
        out.push_str(&summary.replacen(
            &format!("of {}", result.total_found),
            &format!("of at least {}", result.total_found),
            1,
        ));
    } else {
        out.push_str(&summary);
    }
    if !result.scope_errors.is_empty() {
        out.push_str("\n\n## Scope errors\n");
        for failure in &result.scope_errors {
            let _ = writeln!(out, "- {}: {}", failure.scope.display(), failure.error);
        }
    }
    // `collect` retains at least the requested prefix; offset beyond total is empty.
    let matches = &result.matches[start.min(result.matches.len())..end.min(result.matches.len())];
    let mut segments = Vec::new();
    super::format_matches(
        matches,
        &result.scope,
        cache,
        Some(session),
        bloom,
        expand,
        expanded_files,
        &mut out,
        &mut segments,
    );
    let mut navigation = String::new();
    if matches!(result.count_estimate, CountEstimate::Exact(_)) {
        page.append_navigation(&mut navigation, result.total_found, false);
    } else {
        out.push_str("\n\n> Pagination is incomplete because some scopes failed; resolve scope errors before continuing.");
    }
    (out, navigation)
}

/// All navigation goes after all target bodies. If budget hides any body, none
/// of its continuation offsets can survive and skip undisplayed matches.
fn finish(bodies: &[String], navigation: &[String], budget: Option<u64>) -> String {
    let body = bodies.join("\n\n---\n");
    let complete = format!("{body}\n{}", navigation.join("\n"));
    let budget = budget.unwrap_or(crate::budget::DEFAULT_BUDGET);
    if crate::types::estimate_tokens(complete.len() as u64) <= budget {
        complete
    } else {
        truncate_page(&body, budget)
    }
}

pub(crate) fn search(
    query: &str,
    kind: &str,
    scopes: &[PathBuf],
    context: Option<&Path>,
    glob: Option<&str>,
    page: Page,
    cache: &OutlineCache,
    session: &Session,
    bloom: &BloomFilterCache,
    expand: usize,
    budget: Option<u64>,
    mcp: bool,
) -> Result<String, TilthError> {
    let targets: Vec<_> = if kind == "symbol" {
        query
            .split(',')
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .collect()
    } else {
        vec![query]
    };
    if targets.is_empty() || targets.len() > 5 {
        return Err(TilthError::InvalidQuery {
            query: query.into(),
            reason: "symbol search requires 1-5 nonempty queries".into(),
        });
    }
    let results: Vec<_> = targets
        .iter()
        .map(|target| {
            session.record_search(target);
            collect(target, kind, scopes, context, glob, page)
        })
        .collect::<Result<_, _>>()?;
    let render_pages = |expand: usize| {
        let mut remaining = if expand == 0 {
            0
        } else {
            expand.max(targets.len())
        };
        let mut expanded_files = HashSet::new();
        let mut bodies = Vec::new();
        let mut navigation = Vec::new();
        for (target, result) in targets.iter().zip(&results) {
            let total = result.total_found;
            let complete = matches!(result.count_estimate, CountEstimate::Exact(_));
            let (body, mut nav) = render(
                result,
                scopes,
                page,
                cache,
                session,
                bloom,
                &mut remaining,
                &mut expanded_files,
            );
            if mcp && complete && page.bounds(total).1 < total {
                nav = format!(
                    "\n> Next page: {} (keep scope, context and filters unchanged).",
                    serde_json::json!({"query":target,"kind":kind,"offset":page.bounds(total).1,"limit":page.limit})
                );
            }
            if !mcp && complete && page.bounds(total).1 < total {
                let _ = write!(nav, " Query: {target}.");
            }
            bodies.push(body);
            navigation.push(nav);
        }
        (bodies, navigation)
    };
    let (bodies, navigation) = render_pages(expand);
    let complete = format!("{}\n{}", bodies.join("\n\n---\n"), navigation.join("\n"));
    if expand > 0
        && crate::types::estimate_tokens(complete.len() as u64)
            > budget.unwrap_or(crate::budget::DEFAULT_BUDGET)
    {
        // Keep the same ranked page, but shed source expansion before match rows.
        let (mut compact, navigation) = render_pages(0);
        compact[0].insert_str(0, "> Source expansion omitted to fit budget.\n\n");
        return Ok(finish(&compact, &navigation, budget));
    }
    Ok(finish(&bodies, &navigation, budget))
}

/// Joining a scope warning must not retain continuations after budget truncation.
pub(crate) fn apply_final_budget(output: &str, budget: Option<u64>) -> String {
    let budget = budget.unwrap_or(crate::budget::DEFAULT_BUDGET);
    if crate::types::estimate_tokens(output.len() as u64) <= budget {
        return output.into();
    }
    let safe = output
        .lines()
        .filter(|line| !line.starts_with("> Next page:"))
        .collect::<Vec<_>>()
        .join("\n");
    truncate_page(&safe, budget)
}

fn truncate_page(body: &str, budget: u64) -> String {
    crate::budget::apply(&format!(
        "> Page or navigation omitted to fit budget; retry the same offset with a smaller limit or a larger budget.\n\n{body}"
    ), budget)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_large_expansion_falls_back_to_a_compact_page() {
        let root = tempfile::tempdir().unwrap();
        let mut body = String::from("pub fn needle() {\n");
        for i in 0..300 {
            writeln!(body, "    let value{i} = {i};").unwrap();
        }
        body.push_str("}\n");
        std::fs::write(root.path().join("one.rs"), body).unwrap();
        let output = search(
            "needle",
            "symbol",
            &[root.path().to_path_buf()],
            None,
            None,
            Page::new(0, 1).unwrap(),
            &OutlineCache::new(),
            &Session::new(),
            &BloomFilterCache::new(),
            2,
            Some(300),
            true,
        )
        .unwrap();
        assert!(output.contains("Source expansion omitted"), "{output}");
        assert!(
            output.contains("one.rs:") && !output.contains("Next page:"),
            "{output}"
        );
        assert!(!output.contains("Page or navigation omitted"), "{output}");
    }

    #[test]
    fn navigation_only_overflow_reports_retry_instead_of_silently_dropping_it() {
        let body = format!("# Search: needle\n\n{}", "result body line\n".repeat(20));
        let nav = "\n> Next page: --offset 10 --limit 10 (keep filters unchanged).".repeat(3);
        let budget = crate::types::estimate_tokens(body.len() as u64) + 10;
        let output = finish(&[body], &[nav], Some(budget));
        assert!(
            output.contains("navigation omitted to fit budget"),
            "{output}"
        );
        assert!(
            output.contains("retry the same offset with a smaller limit"),
            "{output}"
        );
        assert!(!output.contains("Next page:"), "{output}");
    }

    #[test]
    fn deep_pages_from_one_file_match_complete_ranking_for_every_kind() {
        let root = tempfile::tempdir().unwrap();
        // Beyond both old symbol early-stop and content per-file collection caps.
        let mut body = String::new();
        for i in 0..450 {
            writeln!(body, "fn use{i}() {{ needle(); }}").unwrap();
        }
        std::fs::write(root.path().join("one.rs"), body).unwrap();
        let scopes = [root.path().to_path_buf()];
        for kind in ["symbol", "content", "regex"] {
            let complete = collect(
                "needle",
                kind,
                &scopes,
                None,
                None,
                Page::new(0, 450).unwrap(),
            )
            .unwrap();
            assert_eq!(complete.total_found, 450);
            assert_eq!(complete.matches.len(), 450);
            let expected: Vec<_> = complete.matches.iter().map(|m| m.line).collect();
            let mut actual = Vec::new();
            for offset in (0..450).step_by(43) {
                let page = Page::new(offset, 43).unwrap();
                let result = collect("needle", kind, &scopes, None, None, page).unwrap();
                assert_eq!(result.total_found, 450);
                actual.extend(result.matches.iter().skip(offset).take(43).map(|m| m.line));
            }
            assert_eq!(actual, expected, "{kind}");
        }
    }
}
