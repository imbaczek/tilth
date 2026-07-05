use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::error::TilthError;
use crate::types::{FacetTotals, Match, SearchResult};

use super::{content, facets, parse_pattern, rank, symbol};

type MatchIdentity = (
    PathBuf,
    u32,
    bool,
    Option<(u32, u32)>,
    Option<String>,
    Option<String>,
);

/// Raw symbol search over explicit scopes, merged before display caps.
pub fn symbol_raw_scopes(
    query: &str,
    scopes: &[PathBuf],
    context: Option<&Path>,
    glob: Option<&str>,
    full: bool,
) -> Result<SearchResult, TilthError> {
    combine_scoped_results(scopes, full, |scope| {
        // Collect with the raised cap, then apply the requested display cap
        // once after cross-scope ranking.
        symbol::search(query, scope, context, glob, true)
    })
}

pub fn content_raw_scopes(
    query: &str,
    scopes: &[PathBuf],
    context: Option<&Path>,
    glob: Option<&str>,
    full: bool,
) -> Result<SearchResult, TilthError> {
    let (pattern, is_regex) = parse_pattern(query);
    combine_scoped_results(scopes, full, |scope| {
        content::search(pattern, scope, is_regex, context, glob, true)
    })
}

pub fn regex_raw_scopes(
    pattern: &str,
    scopes: &[PathBuf],
    context: Option<&Path>,
    glob: Option<&str>,
    full: bool,
) -> Result<SearchResult, TilthError> {
    combine_scoped_results(scopes, full, |scope| {
        content::search(pattern, scope, true, context, glob, true)
    })
}

pub fn combine_scoped_results<F>(
    scopes: &[PathBuf],
    full: bool,
    mut search: F,
) -> Result<SearchResult, TilthError>
where
    F: FnMut(&Path) -> Result<SearchResult, TilthError>,
{
    if scopes.is_empty() {
        return Err(TilthError::NotFound {
            path: PathBuf::from("."),
            suggestion: None,
        });
    }

    if scopes.len() == 1 {
        return search(&scopes[0]);
    }

    let display_scope = common_display_root(scopes);
    let mut query = String::new();
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    let mut first_err = None;
    let mut any_ok = false;
    let mut hidden_total_found = 0usize;
    let mut hidden_definitions = 0usize;
    let mut hidden_usages = 0usize;

    for scope in scopes {
        match search(scope) {
            Ok(result) => {
                any_ok = true;
                if query.is_empty() {
                    query = result.query;
                }
                let collected_definitions =
                    result.matches.iter().filter(|m| m.is_definition).count();
                let collected_usages = result.matches.len().saturating_sub(collected_definitions);
                hidden_total_found += result.total_found.saturating_sub(result.matches.len());
                hidden_definitions += result.definitions.saturating_sub(collected_definitions);
                hidden_usages += result.usages.saturating_sub(collected_usages);
                for m in result.matches {
                    if seen.insert(match_identity(&m)) {
                        merged.push(m);
                    }
                }
            }
            Err(err) if first_err.is_none() => first_err = Some(err),
            Err(_) => {}
        }
    }

    if !any_ok {
        return Err(first_err.unwrap_or_else(|| TilthError::NotFound {
            path: display_scope.join(query),
            suggestion: None,
        }));
    }

    rank::sort(&mut merged, &query, &display_scope, None);
    merged.sort_by_key(|m| {
        if m.is_definition {
            u8::from(m.def_weight < 60)
        } else {
            2
        }
    });

    let collected_total_found = merged.len();
    let collected_definitions = merged.iter().filter(|m| m.is_definition).count();
    let collected_usages = collected_total_found - collected_definitions;
    let total_found = collected_total_found + hidden_total_found;
    let definitions = collected_definitions + hidden_definitions;
    let usages = collected_usages + hidden_usages;
    let facet_totals = {
        let snapshot = merged.clone();
        let f = facets::facet_matches(snapshot, &display_scope);
        FacetTotals {
            definitions: f.definitions.len(),
            implementations: f.implementations.len(),
            tests: f.tests.len(),
            usages_local: f.usages_local.len(),
            usages_cross: f.usages_cross.len(),
        }
    };

    let max_matches = if full { 100 } else { 10 };
    merged.truncate(max_matches);

    Ok(SearchResult {
        query,
        scope: display_scope,
        matches: merged,
        total_found,
        definitions,
        usages,
        facet_totals,
    })
}

fn common_display_root(scopes: &[PathBuf]) -> PathBuf {
    let mut common = scopes[0]
        .canonicalize()
        .unwrap_or_else(|_| scopes[0].clone());
    for scope in &scopes[1..] {
        let scope = scope.canonicalize().unwrap_or_else(|_| scope.clone());
        while !scope.starts_with(&common) {
            if !common.pop() {
                return PathBuf::from(".");
            }
        }
    }
    common
}

fn match_identity(m: &Match) -> MatchIdentity {
    (
        m.path.canonicalize().unwrap_or_else(|_| m.path.clone()),
        m.line,
        m.is_definition,
        m.def_range,
        m.def_name.clone(),
        m.impl_target.clone(),
    )
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;

    fn test_match(path: PathBuf, line: u32, is_definition: bool) -> Match {
        Match {
            path,
            line,
            text: "target".to_string(),
            is_definition,
            exact: true,
            file_lines: 10,
            mtime: SystemTime::UNIX_EPOCH,
            def_range: is_definition.then_some((line, line)),
            def_name: is_definition.then_some("target".to_string()),
            def_weight: if is_definition { 100 } else { 0 },
            impl_target: None,
        }
    }

    fn result(scope: &Path, matches: Vec<Match>) -> SearchResult {
        let total_found = matches.len();
        let definitions = matches.iter().filter(|m| m.is_definition).count();
        result_with_totals(scope, matches, total_found, definitions)
    }

    fn result_with_totals(
        scope: &Path,
        matches: Vec<Match>,
        total_found: usize,
        definitions: usize,
    ) -> SearchResult {
        SearchResult {
            query: "target".to_string(),
            scope: scope.to_path_buf(),
            total_found,
            definitions,
            usages: total_found - definitions,
            matches,
            facet_totals: FacetTotals::default(),
        }
    }

    #[test]
    fn combine_dedupes_nested_scope_matches_by_canonical_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("lib.rs");
        std::fs::write(&file, "pub fn target() {}\n").unwrap();

        let combined = combine_scoped_results(&[root.clone(), nested.clone()], false, |scope| {
            Ok(result(scope, vec![test_match(file.clone(), 1, true)]))
        })
        .expect("combine should succeed");

        assert_eq!(combined.total_found, 1);
        assert_eq!(combined.definitions, 1);
        assert_eq!(combined.matches.len(), 1);
    }

    #[test]
    fn combine_ranks_definition_from_later_scope_before_earlier_usage() {
        let tmp = tempfile::tempdir().unwrap();
        let earlier = tmp.path().join("earlier");
        let later = tmp.path().join("later");
        std::fs::create_dir_all(&earlier).unwrap();
        std::fs::create_dir_all(&later).unwrap();
        let usage_path = earlier.join("usage.rs");
        let def_path = later.join("lib.rs");
        std::fs::write(&usage_path, "fn use_it() { target(); }\n").unwrap();
        std::fs::write(&def_path, "pub fn target() {}\n").unwrap();

        let combined = combine_scoped_results(&[earlier.clone(), later.clone()], false, |scope| {
            if scope == earlier {
                Ok(result(
                    scope,
                    vec![test_match(usage_path.clone(), 1, false)],
                ))
            } else {
                Ok(result(scope, vec![test_match(def_path.clone(), 1, true)]))
            }
        })
        .expect("combine should succeed");

        let first = combined.matches.first().expect("missing first match");
        assert!(
            first.is_definition,
            "definition should outrank earlier usage: {combined:#?}"
        );
    }

    #[test]
    fn combine_preserves_per_scope_total_found_beyond_collection_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = tmp.path().join("scope");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&scope).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mut matches = Vec::new();
        for i in 0..100 {
            let path = scope.join(format!("hit_{i:03}.rs"));
            std::fs::write(&path, "fn use_it() { target(); }\n").unwrap();
            matches.push(test_match(path, 1, false));
        }

        let combined = combine_scoped_results(&[scope.clone(), other.clone()], false, |s| {
            if s == scope {
                Ok(result_with_totals(s, matches.clone(), 120, 0))
            } else {
                Ok(result(s, Vec::new()))
            }
        })
        .expect("combine should succeed");

        assert_eq!(
            combined.total_found, 120,
            "total_found must preserve hits beyond the collected match cap"
        );
        assert_eq!(combined.usages, 120);
        assert_eq!(combined.matches.len(), 10);
    }
}
