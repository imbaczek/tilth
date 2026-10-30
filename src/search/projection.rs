//! Compact projections and literal query batches, without source expansion.
use crate::{
    cache::OutlineCache,
    error::TilthError,
    listing::Page,
    types::{CountEstimate, Match},
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub(crate) fn run(
    queries: &[String],
    kind: &str,
    select: Option<&str>,
    scopes: &[PathBuf],
    context: Option<&Path>,
    glob: Option<&str>,
    page: Page,
    budget: Option<u64>,
    mcp: bool,
    cache: &OutlineCache,
) -> Result<String, TilthError> {
    let invalid = |reason: &str| TilthError::InvalidQuery {
        query: queries.join("; "),
        reason: reason.into(),
    };
    if queries.is_empty() || queries.len() > 20 || queries.iter().any(|q| q.trim().is_empty()) {
        return Err(invalid("requires 1-20 nonempty literal queries"));
    }
    if !matches!(kind, "symbol" | "content" | "regex") {
        return Err(invalid(
            "literal batches and projections support symbol, content, or regex search",
        ));
    }
    if !matches!(
        select,
        None | Some("files" | "definitions" | "signatures" | "count")
    ) {
        return Err(invalid(
            "select must be files, definitions, signatures, or count",
        ));
    }
    if matches!(select, Some("definitions" | "signatures")) && kind != "symbol" {
        return Err(invalid("definitions and signatures require kind=symbol"));
    }
    let mut bodies = Vec::new();
    let mut signatures = HashMap::new();
    let mut seen = HashSet::new();
    for query in queries.iter().filter(|q| seen.insert(q.as_str())) {
        // Project before paging. Count needs no retained rows.
        let prefix = if select == Some("count") || (select == Some("files") && kind != "symbol") {
            1
        } else if select.is_some() {
            usize::MAX
        } else {
            page.prefix()
        };
        let result = super::merge::page_raw_scopes(query, kind, scopes, context, glob, prefix)?;
        let lower_bound = matches!(result.count_estimate, CountEstimate::Observed { .. });
        let mut header = format!("# Search: {query:?} [{}]", select.unwrap_or("matches"));
        if lower_bound {
            header.push_str("\n> Results may be incomplete: some eligible work was skipped.");
        }
        for error in &result.scope_errors {
            let _ = write!(
                header,
                "\n> Scope error: {}: {}",
                error.scope.display(),
                error.error
            );
        }
        if select == Some("count") {
            let qualifier = if lower_bound { "at least " } else { "" };
            let _ = write!(header, "\n{qualifier}{} matches", result.total_found);
            bodies.push(header);
            continue;
        }
        let display_root = if result.scope.is_file() {
            result.scope.parent().unwrap_or(&result.scope)
        } else {
            &result.scope
        };
        let noun = if select == Some("files") {
            "files"
        } else if matches!(select, Some("definitions" | "signatures")) {
            "definitions"
        } else {
            "matches"
        };
        let (rows, total) = if select == Some("files") {
            let paths: Vec<_> = result.file_ledger.as_ref().map_or_else(
                || result.matches.iter().map(|m| &m.path).collect(),
                |ledger| ledger.keys().collect(),
            );
            let rows: Vec<_> = paths
                .into_iter()
                .map(|path| {
                    path.strip_prefix(display_root)
                        .unwrap_or(path)
                        .display()
                        .to_string()
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            let total = rows.len();
            let (start, end) = page.bounds(total);
            (rows[start..end].to_vec(), total)
        } else {
            let matches: Vec<_> = result
                .matches
                .iter()
                .filter(|m| {
                    !matches!(select, Some("definitions" | "signatures")) || m.is_definition
                })
                .collect();
            let total = if select.is_none() {
                result.total_found
            } else {
                matches.len()
            };
            let (start, end) = page.bounds(total);
            let rows = matches[start.min(matches.len())..end.min(matches.len())]
                .iter()
                .map(|m| {
                    let path = m.path.strip_prefix(display_root).unwrap_or(&m.path);
                    let text = if select == Some("signatures") {
                        signature(m, query, cache, &mut signatures)
                    } else {
                        m.text.trim().to_owned()
                    };
                    format!("{}:{} {text}", path.display(), m.line)
                })
                .collect();
            if matches!(select, Some("definitions" | "signatures"))
                && matches[start.min(matches.len())..end.min(matches.len())]
                    .iter()
                    .any(|m| {
                        cache
                            .get_or_parse(&m.path)
                            .is_some_and(|parsed| parsed.tree.root_node().has_error())
                    })
            {
                header.push_str(
                    "\n> Structural matches may be incomplete: a matching file has parse errors.",
                );
            }
            (rows, total)
        };
        let mut body = page.render_selected(&header, &rows, total, noun, mcp);
        if lower_bound {
            body = body
                .lines()
                .filter(|line| !line.starts_with("> Next page:"))
                .collect::<Vec<_>>()
                .join("\n");
        }
        bodies.push(body);
    }
    Ok(crate::listing::finish(&bodies.join("\n\n---\n\n"), budget))
}

fn signature(
    m: &Match,
    query: &str,
    cache: &OutlineCache,
    catalog: &mut HashMap<(PathBuf, String), Vec<crate::lang::declarations::Declaration>>,
) -> String {
    let unavailable = || {
        format!(
            "{} [signature unavailable]",
            m.def_name.as_deref().unwrap_or("declaration")
        )
    };
    let declarations = catalog
        .entry((m.path.clone(), query.to_owned()))
        .or_insert_with(|| {
            cache
                .get_or_parse(&m.path)
                .map(|parsed| {
                    if let Some((owner, name)) = crate::lang::declarations::qualified_parts(query)
                        .filter(|_| {
                            matches!(
                                parsed.lang,
                                crate::types::Lang::TypeScript
                                    | crate::types::Lang::Tsx
                                    | crate::types::Lang::JavaScript
                            )
                        })
                    {
                        crate::lang::outline::qualified_declarations(
                            &parsed.content,
                            parsed.lang,
                            owner,
                            Some(name),
                            true,
                        )
                        .into_iter()
                        .map(|(entry, span)| crate::lang::declarations::Declaration {
                            path: m.path.clone(),
                            qualified: query.into(),
                            entry,
                            parsed: parsed.clone(),
                            span: Some((span.start, span.end)),
                        })
                        .collect()
                    } else {
                        crate::lang::declarations::collect(&m.path, &parsed)
                    }
                })
                .unwrap_or_default()
        });
    let candidates: Vec<_> = declarations
        .iter()
        .filter(|d| {
            d.entry.start_line == m.line
                && m.def_name
                    .as_deref()
                    .is_none_or(|name| d.entry.name == name)
        })
        .collect();
    match candidates.as_slice() {
        [declaration] => {
            crate::lang::declarations::signature_text(declaration).unwrap_or_else(unavailable)
        }
        [] => unavailable(),
        _ => format!(
            "{} [signature ambiguous on this line; use a qualified symbol read]",
            m.def_name.as_deref().unwrap_or("declaration")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn projection_filters_before_paging_and_literal_commas_stay_literal() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn needle(\n x: i32,\n) -> i32 { x }\nfn use_it() { needle(1); }\n// alpha,beta\n",
        )
        .unwrap();
        let run = |queries: &[String], kind, select, page| {
            run(
                queries,
                kind,
                select,
                &[dir.path().to_path_buf()],
                None,
                None,
                page,
                None,
                false,
                &OutlineCache::new(),
            )
            .unwrap()
        };
        let out = run(
            &["needle".into()],
            "symbol",
            Some("signatures"),
            Page::new(0, 1).unwrap(),
        );
        assert!(
            out.contains("fn needle(") && out.contains("x: i32") && out.contains("-> i32"),
            "{out}"
        );
        assert!(!out.contains("{ x }"), "{out}");
        assert!(out.contains("definitions 1-1 of 1"), "{out}");
        let out = run(
            &["alpha,beta".into(), "absent".into()],
            "content",
            Some("count"),
            Page::new(0, 1).unwrap(),
        );
        assert!(
            out.contains("1 matches") && out.contains("0 matches"),
            "{out}"
        );
    }
    #[test]
    fn files_are_unique_and_count_is_not_page_size() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn needle() {}\nfn caller() { needle(); }\n",
        )
        .unwrap();
        let run = |select| {
            run(
                &["needle".into()],
                "symbol",
                Some(select),
                &[dir.path().to_path_buf()],
                None,
                None,
                Page::new(0, 1).unwrap(),
                None,
                true,
                &OutlineCache::new(),
            )
            .unwrap()
        };
        assert!(run("files").contains("files 1-1 of 1"));
        assert!(run("count").contains("2 matches"));
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    #[test]
    fn file_projection_uses_all_file_counts_without_retaining_every_hit() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.txt", "b.txt"] {
            std::fs::write(dir.path().join(name), "needle\n".repeat(1000)).unwrap();
        }
        let scopes = [dir.path().to_path_buf()];
        let out = run(
            &["needle".into()],
            "content",
            Some("files"),
            &scopes,
            None,
            None,
            Page::new(0, 10).unwrap(),
            None,
            false,
            &OutlineCache::new(),
        )
        .unwrap();
        assert!(
            out.contains("files 1-2 of 2") && out.contains("a.txt") && out.contains("b.txt"),
            "{out}"
        );
    }
    #[test]
    fn unavailable_signature_never_exposes_inline_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "struct Secret { hidden: i32 }\n").unwrap();
        let out = run(
            &["Secret".into()],
            "symbol",
            Some("signatures"),
            &[dir.path().to_path_buf()],
            None,
            None,
            Page::new(0, 10).unwrap(),
            None,
            false,
            &OutlineCache::new(),
        )
        .unwrap();
        assert!(!out.contains("hidden"), "{out}");
    }
}

#[cfg(test)]
mod qualified_tests {
    use super::*;
    #[test]
    fn qualified_methods_filter_before_paging_and_preserve_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let mut source = String::new();
        for n in 0..120 {
            writeln!(source, "class Other{n} {{ method() {{}} }}").unwrap();
        }
        source.push_str("class Target { method(value: number): number { return value; } }\n");
        source.push_str("namespace Left { export class Inner { method() {} } }\nnamespace Right { export class Inner { method() {} } }\n");
        std::fs::write(dir.path().join("a.ts"), source).unwrap();
        for query in [
            "Target.method",
            "Target::method",
            "Left.Inner.method",
            "Left::Inner::method",
        ] {
            let out = run(
                &[query.into()],
                "symbol",
                Some("signatures"),
                &[dir.path().to_owned()],
                None,
                None,
                Page::new(0, 1).unwrap(),
                None,
                false,
                &OutlineCache::new(),
            )
            .unwrap();
            assert!(
                out.contains("definitions 1-1 of 1") && out.contains("method("),
                "{query}: {out}"
            );
            if query.starts_with("Target") {
                assert!(
                    out.contains("value: number") && !out.contains("return value"),
                    "{out}"
                );
            }
        }
        let out = run(
            &["Missing.Target.method".into()],
            "symbol",
            Some("count"),
            &[dir.path().to_owned()],
            None,
            None,
            Page::new(0, 1).unwrap(),
            None,
            false,
            &OutlineCache::new(),
        )
        .unwrap();
        assert!(out.contains("0 matches"), "{out}");
    }
}
