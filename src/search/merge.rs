use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::TilthError;
use crate::types::{FacetTotals, Match, SearchResult};

use super::{content, facets, parse_pattern, rank, symbol};

const MAX_MATCHES: usize = 10;
const FULL_MAX_MATCHES: usize = 100;

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
    combine_scoped_results(scopes, full, context, glob, |scope| {
        symbol::search_collected(query, scope, context, glob, true)
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
    let visited = Mutex::new(HashSet::new());
    combine_scoped_results(scopes, full, context, glob, |scope| {
        content::search_collected_scoped(pattern, scope, is_regex, context, glob, &visited)
    })
}

pub fn regex_raw_scopes(
    pattern: &str,
    scopes: &[PathBuf],
    context: Option<&Path>,
    glob: Option<&str>,
    full: bool,
) -> Result<SearchResult, TilthError> {
    let visited = Mutex::new(HashSet::new());
    combine_scoped_results(scopes, full, context, glob, |scope| {
        content::search_collected_scoped(pattern, scope, true, context, glob, &visited)
    })
}

fn combine_scoped_results<F>(
    scopes: &[PathBuf],
    full: bool,
    context: Option<&Path>,
    glob: Option<&str>,
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

    let scopes = minimal_scopes(scopes, glob);

    if scopes.len() == 1 {
        let mut result = search(&scopes[0])?;
        result
            .matches
            .truncate(if full { FULL_MAX_MATCHES } else { MAX_MATCHES });
        return Ok(result);
    }

    let display_scope = common_display_root(&scopes);
    let mut query = String::new();
    let mut merged = Vec::new();
    let mut seen = HashSet::new();
    let mut first_err = None;
    let mut any_ok = false;
    let mut total_found = 0;
    let mut definitions = 0;
    let mut uncollected_tests = 0;
    let mut uncollected_usages_cross = 0;

    for scope in &scopes {
        match search(scope) {
            Ok(result) => {
                any_ok = true;
                // Content scopes count disjoint files before truncation; symbol
                // scopes retain every observed hit, so duplicates are visible.
                total_found += result.total_found;
                definitions += result.definitions;
                // Only content searches cap their collected rows. Their exact
                // facets must retain hits absent from the rows merged below.
                // Shared visited files make these withheld counts disjoint.
                if result.total_found > result.matches.len() {
                    let collected_tests = result
                        .matches
                        .iter()
                        .filter(|m| facets::is_test_match(m))
                        .count();
                    uncollected_tests += result.facet_totals.tests - collected_tests;
                    uncollected_usages_cross +=
                        result.facet_totals.usages_cross - (result.matches.len() - collected_tests);
                }
                if query.is_empty() {
                    query = result.query;
                }
                for m in result.matches {
                    if seen.insert(match_identity(&m)) {
                        merged.push(m);
                    } else {
                        total_found -= 1;
                        definitions -= usize::from(m.is_definition);
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

    rank::sort(&mut merged, &query, &display_scope, context);
    merged.sort_by_key(|m| {
        if m.is_definition {
            u8::from(m.def_weight < 60)
        } else {
            2
        }
    });

    let usages = total_found - definitions;
    let facet_totals = {
        let snapshot = merged.clone();
        let f = facets::facet_matches(snapshot, &display_scope);
        FacetTotals {
            definitions: f.definitions.len(),
            implementations: f.implementations.len(),
            tests: f.tests.len() + uncollected_tests,
            usages_local: f.usages_local.len(),
            usages_cross: f.usages_cross.len() + uncollected_usages_cross,
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

fn minimal_scopes(scopes: &[PathBuf], glob: Option<&str>) -> Vec<PathBuf> {
    let mut unique: Vec<PathBuf> = Vec::new();
    for scope in scopes {
        let canonical = scope.canonicalize().unwrap_or_else(|_| scope.clone());
        if !unique.contains(&canonical) {
            unique.push(canonical);
        }
    }
    if glob.is_none_or(str::is_empty) {
        let candidates = unique.clone();
        unique.retain(|scope| {
            !candidates
                .iter()
                .any(|other| scope != other && scope.starts_with(other))
        });
    }
    unique
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
        let f = facets::facet_matches(matches.clone(), scope);
        SearchResult {
            query: "target".to_string(),
            scope: scope.to_path_buf(),
            total_found,
            definitions,
            usages: total_found - definitions,
            matches,
            facet_totals: FacetTotals {
                definitions: f.definitions.len(),
                implementations: f.implementations.len(),
                tests: f.tests.len(),
                usages_local: f.usages_local.len(),
                usages_cross: f.usages_cross.len(),
            },
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

        let mut searches = 0;
        let combined = combine_scoped_results(
            &[root.clone(), nested.clone()],
            false,
            None,
            Some("*.rs"),
            |_| {
                searches += 1;
                Ok(result(
                    root.as_path(),
                    vec![test_match(file.clone(), 1, true)],
                ))
            },
        )
        .expect("combine should succeed");

        assert_eq!(
            searches, 2,
            "nested scopes must each be searched (union, not subsumption)"
        );
        assert_eq!(combined.total_found, 1);
        assert_eq!(combined.definitions, 1);
        assert_eq!(combined.matches.len(), 1, "identical matches must dedup");
    }

    #[test]
    fn minimal_scopes_keeps_one_copy_of_duplicate_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = tmp.path().to_path_buf();
        assert_eq!(
            minimal_scopes(&[scope.clone(), scope.clone()], None),
            vec![scope]
        );
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

        let combined = combine_scoped_results(
            &[earlier.clone(), later.clone()],
            false,
            None,
            None,
            |scope| {
                if scope == earlier {
                    Ok(result(
                        scope,
                        vec![test_match(usage_path.clone(), 1, false)],
                    ))
                } else {
                    Ok(result(scope, vec![test_match(def_path.clone(), 1, true)]))
                }
            },
        )
        .expect("combine should succeed");

        let first = combined.matches.first().expect("missing first match");
        assert!(
            first.is_definition,
            "definition should outrank earlier usage: {combined:#?}"
        );
    }

    #[test]
    fn combine_preserves_observed_union_beyond_display_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = tmp.path().join("scope");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&scope).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mut matches = Vec::new();
        for i in 0..120 {
            let path = scope.join(format!("hit_{i:03}.rs"));
            std::fs::write(&path, "fn use_it() { target(); }\n").unwrap();
            matches.push(test_match(path, 1, false));
        }

        let combined =
            combine_scoped_results(&[scope.clone(), other.clone()], false, None, None, |s| {
                if s == scope {
                    Ok(result(s, matches.clone()))
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

    #[test]
    fn duplicate_scopes_collect_once_preserving_totals_facets_and_display_caps() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = tmp.path().to_path_buf();
        let matches: Vec<_> = (1..=150)
            .map(|line| test_match(scope.join("lib.rs"), line, line <= 20))
            .collect();
        for full in [false, true] {
            for glob in [None, Some(""), Some("*.rs")] {
                let mut calls = 0;
                let combined = combine_scoped_results(
                    &[scope.clone(), scope.clone()],
                    full,
                    None,
                    glob,
                    |s| {
                        calls += 1;
                        Ok(result(s, matches.clone()))
                    },
                )
                .unwrap();
                assert_eq!(calls, 1);
                assert_eq!(combined.matches.len(), if full { 100 } else { 10 });
                assert_eq!(combined.total_found, 150);
                assert_eq!(combined.definitions, 20);
                assert_eq!(combined.usages, 130);
                assert_eq!(combined.facet_totals.definitions, 20);
                assert_eq!(combined.facet_totals.usages_cross, 130);
            }
        }
    }

    #[test]
    fn single_scope_alias_preserves_precap_metadata_and_full_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let scope = tmp.path().join("only");
        std::fs::create_dir_all(&scope).unwrap();
        let mut matches = Vec::new();
        for i in 0..120 {
            let path = scope.join(format!("hit_{i:03}.rs"));
            matches.push(test_match(path, 1, false));
        }

        let combined =
            combine_scoped_results(std::slice::from_ref(&scope), true, None, None, |_| {
                Ok(result(&scope, matches.clone()))
            })
            .expect("combine should succeed");

        assert_eq!(combined.matches.len(), 100);
        assert_eq!(combined.total_found, 120);
        assert_eq!(combined.usages, 120);
    }

    #[test]
    fn minimal_scopes_with_glob_keeps_nested_scopes_for_distinct_filters() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            minimal_scopes(&[root.clone(), nested.clone()], Some("*.rs")),
            vec![root.canonicalize().unwrap(), nested.canonicalize().unwrap()]
        );
    }

    #[test]
    fn minimal_scopes_without_glob_drops_nested_scopes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            minimal_scopes(&[root.clone(), nested.clone()], None),
            vec![root.canonicalize().unwrap()]
        );
    }

    #[test]
    fn symbol_raw_scopes_definition_found_from_nested_and_parent_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("lib.rs"), "pub fn anchored_target() {}\n").unwrap();
        std::fs::write(
            root.join("other.rs"),
            "fn caller() {\n    anchored_target();\n}\n",
        )
        .unwrap();

        let forward = symbol_raw_scopes(
            "anchored_target",
            &[nested.clone(), root.clone()],
            None,
            None,
            false,
        )
        .expect("forward order should succeed");
        let reversed = symbol_raw_scopes(
            "anchored_target",
            &[root.clone(), nested.clone()],
            None,
            None,
            false,
        )
        .expect("reversed order should succeed");

        for combined in [&forward, &reversed] {
            assert_eq!(
                combined.total_found, 2,
                "union must keep both hits: {combined:#?}"
            );
            assert_eq!(combined.definitions, 1);
            assert_eq!(combined.usages, 1);
            let first = combined.matches.first().expect("missing first match");
            assert!(
                first.is_definition,
                "definition must rank first: {combined:#?}"
            );
        }
        assert!(forward
            .matches
            .iter()
            .any(|m| m.path.ends_with("nested/lib.rs")));
        assert!(reversed
            .matches
            .iter()
            .any(|m| m.path.ends_with("nested/lib.rs")));
    }

    #[test]
    fn overlapping_glob_searches_count_observed_union_above_full_display_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("lib.rs"), "target();\n".repeat(150)).unwrap();
        std::fs::write(root.join("lib.rs"), "target();\n".repeat(20)).unwrap();

        for scopes in [vec![root.clone(), child.clone()], vec![child, root]] {
            for full in [false, true] {
                for search in [symbol_raw_scopes, content_raw_scopes, regex_raw_scopes] {
                    let result = search("target", &scopes, None, Some("*.rs"), full).unwrap();
                    assert_eq!(result.total_found, 170);
                    assert_eq!(result.usages, 170);
                    assert_eq!(result.definitions, 0);
                    assert_eq!(result.facet_totals.usages_cross, 170);
                    assert_eq!(result.matches.len(), if full { 100 } else { 10 });
                    let identities: HashSet<_> =
                        result.matches.iter().map(match_identity).collect();
                    assert_eq!(identities.len(), result.matches.len());
                }
            }
        }
    }

    #[test]
    fn invalid_glob_and_regex_return_errors_single_and_multi_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let one = tmp.path().join("one");
        let two = tmp.path().join("two");
        std::fs::create_dir_all(&one).unwrap();
        std::fs::create_dir_all(&two).unwrap();
        std::fs::write(one.join("lib.rs"), "target();\n").unwrap();

        let single = vec![one.clone()];
        let multi = vec![one.clone(), two.clone()];
        for scopes in [&single, &multi] {
            assert!(symbol_raw_scopes("target", scopes, None, Some("[unclosed"), false).is_err());
            assert!(content_raw_scopes("target", scopes, None, Some("[unclosed"), false).is_err());
            assert!(regex_raw_scopes("[unclosed", scopes, None, None, false).is_err());
        }
    }
}
