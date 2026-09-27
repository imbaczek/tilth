use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::error::TilthError;
use crate::types::{CountEstimate, FacetTotals, FileTotals, Match, ScopeError, SearchResult};

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
    combine_scoped_results(scopes, full, context, glob, |scope| {
        symbol::search_collected(query, scope, context, glob, full)
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
    combine_scoped_results(scopes, full, context, glob, |scope| {
        content::search_collected(pattern, scope, is_regex, context, glob, full)
    })
}

pub fn regex_raw_scopes(
    pattern: &str,
    scopes: &[PathBuf],
    context: Option<&Path>,
    glob: Option<&str>,
    full: bool,
) -> Result<SearchResult, TilthError> {
    combine_scoped_results(scopes, full, context, glob, |scope| {
        content::search_collected(pattern, scope, true, context, glob, full)
    })
}

fn combine_scoped_results<F>(
    scopes: &[PathBuf],
    full: bool,
    context: Option<&Path>,
    _glob: Option<&str>,
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

    let scopes = minimal_scopes(scopes);

    let display_scope = common_display_root(&scopes);
    let mut query = String::new();
    let mut observations = Vec::new();
    let mut counted_identities = HashSet::new();
    let mut scope_errors = Vec::new();
    let mut any_ok = false;
    let mut total_found = 0;
    let mut definitions = 0;
    let mut scope_counts = Vec::new();
    let mut uncollected_tests = 0;
    let mut uncollected_usages_cross = 0;
    let mut file_ledger: Option<HashMap<PathBuf, FileTotals>> = None;
    let mut complete = true;

    for scope in &scopes {
        match search(scope) {
            Ok(result) => {
                any_ok = true;
                if matches!(result.count_estimate, CountEstimate::Observed { .. }) {
                    complete = false;
                }
                if let Some(files) = &result.file_ledger {
                    let union = file_ledger.get_or_insert_with(HashMap::new);
                    for (path, totals) in files {
                        if let Some(previous) = union.get_mut(path) {
                            if previous != totals {
                                complete = false;
                                previous.hits = previous.hits.max(totals.hits);
                                previous.tests = previous.tests.max(totals.tests);
                            }
                        } else {
                            union.insert(path.clone(), *totals);
                        }
                    }
                }
                scope_counts.push((scope.clone(), result.total_found));
                // Content scopes count disjoint files before truncation; symbol
                // scopes retain every observed hit, so duplicates are visible.
                total_found += result.total_found;
                definitions += result.definitions;
                // Only content searches cap their collected rows. Their exact
                // facets must retain hits absent from the rows merged below.
                // Shared visited files make these withheld counts disjoint.
                if result.file_ledger.is_none() && result.total_found > result.matches.len() {
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
                    if !counted_identities.insert(match_identity(&m)) {
                        total_found -= 1;
                        definitions -= usize::from(m.is_definition);
                    }
                    let physical = m.path.canonicalize().unwrap_or_else(|_| m.path.clone());
                    if let Some(paths) = result.alias_paths.get(&(physical, m.is_definition)) {
                        for path in paths {
                            let mut observation = m.clone();
                            observation.path.clone_from(path);
                            observations.push((observation, scope.clone()));
                        }
                    } else {
                        observations.push((m, scope.clone()));
                    }
                }
            }
            Err(err) => scope_errors.push(ScopeError {
                scope: scope.clone(),
                error: err,
            }),
        }
    }

    if !any_ok {
        if scope_errors.len() == 1 {
            return Err(scope_errors.pop().expect("one failure").error);
        }
        return Err(TilthError::ScopedFailures {
            failures: scope_errors,
        });
    }
    if !scope_errors.is_empty() {
        complete = false;
    }

    let now = SystemTime::now();
    let mut unique: HashMap<MatchIdentity, (Match, i32)> = HashMap::new();
    for (m, observed_scope) in observations {
        let identity = match_identity(&m);
        let score = rank::observation_score(&m, &query, &observed_scope, context, now);
        match unique.entry(identity) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert((m, score));
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                let (selected, best_score) = entry.get_mut();
                *best_score = (*best_score).max(score);
                if preferred_observed_path(&m.path, &selected.path) {
                    selected.path = m.path;
                }
            }
        }
    }
    let mut ranked: Vec<_> = unique.into_values().collect();
    ranked.sort_by(|(a, sa), (b, sb)| {
        definition_stratum(a)
            .cmp(&definition_stratum(b))
            .then_with(|| sb.cmp(sa))
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.line.cmp(&b.line))
    });
    let mut merged: Vec<Match> = ranked.into_iter().map(|(m, _)| m).collect();

    if let Some(files) = &file_ledger {
        total_found = files.values().map(|totals| totals.hits).sum();
        definitions = 0;
    }
    let usages = total_found - definitions;
    let facet_totals = {
        let snapshot = merged
            .iter()
            .cloned()
            .map(|mut m| {
                m.path = m.path.canonicalize().unwrap_or(m.path);
                m
            })
            .collect();
        let f = facets::facet_matches(snapshot, &display_scope);
        let mut totals = FacetTotals {
            definitions: f.definitions.len(),
            implementations: f.implementations.len(),
            tests: f.tests.len() + uncollected_tests,
            usages_local: f.usages_local.len(),
            usages_cross: f.usages_cross.len() + uncollected_usages_cross,
        };
        if let Some(files) = &file_ledger {
            totals.tests = files.values().map(|file| file.tests).sum();
            totals.usages_cross = total_found - totals.tests;
        }
        totals
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
        scope_counts,
        count_estimate: if complete {
            CountEstimate::Exact(total_found)
        } else {
            CountEstimate::Observed { count: total_found }
        },
        scope_errors,
        file_ledger,
        alias_paths: HashMap::new(),
    })
}

fn definition_stratum(m: &Match) -> u8 {
    if m.is_definition {
        u8::from(m.def_weight < 60)
    } else {
        2
    }
}

fn preferred_observed_path(candidate: &Path, current: &Path) -> bool {
    let canonical = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.to_path_buf());
    let candidate_absolute =
        std::path::absolute(candidate).unwrap_or_else(|_| candidate.to_path_buf());
    let current_absolute = std::path::absolute(current).unwrap_or_else(|_| current.to_path_buf());
    if candidate_absolute == canonical {
        return current_absolute != canonical;
    }
    if current_absolute == canonical {
        return false;
    }
    let candidate = candidate_absolute.to_string_lossy();
    let current = current_absolute.to_string_lossy();
    (candidate.len(), candidate.as_ref()) < (current.len(), current.as_ref())
}

pub(super) fn minimal_scopes(scopes: &[PathBuf]) -> Vec<PathBuf> {
    let mut unique: Vec<PathBuf> = Vec::new();
    for scope in scopes {
        let canonical = scope.canonicalize().unwrap_or_else(|_| scope.clone());
        if !unique
            .iter()
            .any(|root| root.canonicalize().unwrap_or_else(|_| root.clone()) == canonical)
        {
            unique.push(scope.clone());
        }
    }
    unique
}

fn common_display_root(scopes: &[PathBuf]) -> PathBuf {
    if scopes.len() == 1 {
        return scopes[0].clone();
    }
    let mut common = std::path::absolute(&scopes[0]).unwrap_or_else(|_| scopes[0].clone());
    for scope in &scopes[1..] {
        let scope = std::path::absolute(scope).unwrap_or_else(|_| scope.clone());
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
    use std::collections::HashSet;
    use std::time::SystemTime;

    #[test]
    fn singleton_relative_display_scope_stays_relative() {
        assert_eq!(
            common_display_root(&[PathBuf::from(".")]),
            PathBuf::from(".")
        );
    }

    #[cfg(unix)]
    #[test]
    fn observed_relative_physical_path_beats_shorter_symlink_alias() {
        use std::os::unix::fs::symlink;

        let physical_dir = tempfile::tempdir_in(".").unwrap();
        let physical = physical_dir.path().join("very_long_original_filename.rs");
        std::fs::write(&physical, "target\n").unwrap();
        let alias_dir = tempfile::tempdir().unwrap();
        let alias = alias_dir.path().join("a");
        symlink(physical.canonicalize().unwrap(), &alias).unwrap();
        assert!(preferred_observed_path(&physical, &alias));
        assert!(!preferred_observed_path(&alias, &physical));
    }

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
            scope_counts: Vec::new(),
            count_estimate: CountEstimate::Exact(total_found),
            scope_errors: Vec::new(),
            file_ledger: None,
            alias_paths: HashMap::new(),
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
        assert_eq!(minimal_scopes(&[scope.clone(), scope.clone()]), vec![scope]);
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
    fn minimal_scopes_keeps_nested_scopes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            minimal_scopes(&[root.clone(), nested.clone()]),
            vec![root.canonicalize().unwrap(), nested.canonicalize().unwrap()]
        );
    }

    #[test]
    fn explicit_nested_scope_searches_ignored_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let nested = root.join("ignored");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join(".tilthignore"), "ignored/\n").unwrap();
        std::fs::write(nested.join("match.rs"), "unique_scoped_needle\n").unwrap();

        let parent_only = content_raw_scopes(
            "unique_scoped_needle",
            std::slice::from_ref(&root),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(parent_only.total_found, 0);

        let combined =
            content_raw_scopes("unique_scoped_needle", &[root, nested], None, None, false).unwrap();
        assert_eq!(combined.total_found, 1);
        assert_eq!(combined.matches.len(), 1);
    }

    #[test]
    fn multi_scope_gitignore_override_preserves_explicit_ignored_child() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let ignored = root.join("ignored");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&ignored).unwrap();
        std::fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::write(root.join("visible.rs"), "gitignore_multiscope_needle\n").unwrap();
        std::fs::write(ignored.join("inside.rs"), "gitignore_multiscope_needle\n").unwrap();

        for (respect_gitignore, parent_count) in [(true, 1), (false, 2)] {
            crate::search::with_gitignore_override(Some(respect_gitignore), || {
                let parent = content_raw_scopes(
                    "gitignore_multiscope_needle",
                    std::slice::from_ref(&root),
                    None,
                    None,
                    false,
                )
                .unwrap();
                assert_eq!(parent.total_found, parent_count);

                for scopes in [
                    vec![root.clone(), ignored.clone()],
                    vec![ignored.clone(), root.clone()],
                ] {
                    let combined = content_raw_scopes(
                        "gitignore_multiscope_needle",
                        &scopes,
                        None,
                        None,
                        false,
                    )
                    .unwrap();
                    assert_eq!(combined.total_found, 2);
                    assert_eq!(combined.matches.len(), 2);
                    assert!(combined
                        .scope_counts
                        .iter()
                        .any(|(scope, count)| scope == &root && *count == parent_count));
                    assert!(combined
                        .scope_counts
                        .iter()
                        .any(|(scope, count)| scope == &ignored && *count == 1));
                }
            });
        }
    }

    #[test]
    fn overlapping_scopes_report_independent_content_and_regex_counts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let nested = root.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("match.rs"), "scope_count_needle\n").unwrap();

        for scopes in [
            vec![root.clone(), nested.clone()],
            vec![nested.clone(), root.clone()],
        ] {
            for search in [content_raw_scopes, regex_raw_scopes] {
                let result = search("scope_count_needle", &scopes, None, None, false).unwrap();
                assert_eq!(result.total_found, 1);
                assert_eq!(result.scope_counts.len(), 2);
                assert!(result.scope_counts.iter().all(|(_, count)| *count == 1));
            }
        }
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
                for (kind, search) in [
                    (
                        "symbol",
                        symbol_raw_scopes
                            as fn(
                                &str,
                                &[PathBuf],
                                Option<&Path>,
                                Option<&str>,
                                bool,
                            ) -> Result<SearchResult, TilthError>,
                    ),
                    ("content", content_raw_scopes),
                    ("regex", regex_raw_scopes),
                ] {
                    let result = search("target", &scopes, None, Some("*.rs"), full).unwrap();
                    if kind == "symbol" && !full {
                        assert!((150..=170).contains(&result.total_found));
                    } else {
                        assert_eq!(result.total_found, 170);
                        assert_eq!(result.count_estimate, CountEstimate::Exact(170));
                    }
                    assert_eq!(result.usages, result.total_found);
                    assert_eq!(result.definitions, 0);
                    assert_eq!(result.facet_totals.usages_cross, result.total_found);
                    assert_eq!(result.matches.len(), if full { 100 } else { 10 });
                    let identities: HashSet<_> =
                        result.matches.iter().map(match_identity).collect();
                    assert_eq!(identities.len(), result.matches.len());
                }
            }
        }
    }

    #[test]
    fn disjoint_hit_uses_its_own_scope_rank_in_either_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("p");
        let scope_a = root.join("A");
        let scope_b = root.join("B/deep/tree");
        let path_a = scope_a.join("deep/a.rs");
        let path_b = scope_b.join("b.rs");
        std::fs::create_dir_all(path_a.parent().unwrap()).unwrap();
        std::fs::create_dir_all(path_b.parent().unwrap()).unwrap();
        std::fs::write(&path_a, "target\n").unwrap();
        std::fs::write(&path_b, "target\n").unwrap();

        let match_a = test_match(path_a.clone(), 1, false);
        let match_b = test_match(path_b.clone(), 1, false);
        let mut ranked_from_a = vec![match_a.clone(), match_b.clone()];
        rank::sort(&mut ranked_from_a, "target", &scope_a, None);
        assert_eq!(ranked_from_a[0].path, path_a);
        let mut ranked_from_b = vec![match_a.clone(), match_b.clone()];
        rank::sort(&mut ranked_from_b, "target", &scope_b, None);
        assert_eq!(ranked_from_b[0].path, path_b);

        for scopes in [
            vec![scope_a.clone(), scope_b.clone()],
            vec![scope_b.clone(), scope_a.clone()],
        ] {
            let combined = combine_scoped_results(&scopes, false, None, None, |scope| {
                if scope == scope_a {
                    Ok(result(scope, vec![match_a.clone()]))
                } else {
                    Ok(result(scope, vec![match_b.clone()]))
                }
            })
            .expect("disjoint scopes should combine");

            assert_eq!(combined.total_found, 2);
            assert_eq!(combined.matches[0].path, path_b);
            assert_eq!(combined.scope_counts.len(), 2);
            assert!(combined.scope_counts.iter().all(|(_, count)| *count == 1));
        }
    }

    #[test]
    fn overlapping_hit_uses_best_observed_scope_rank_in_either_order() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let child = parent.join("c");
        let nested_file = child.join("m.rs");
        let parent_file = parent.join("a").join("n.rs");
        std::fs::create_dir_all(nested_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(parent_file.parent().unwrap()).unwrap();
        std::fs::write(&nested_file, "target\n").unwrap();
        std::fs::write(&parent_file, "target\n").unwrap();

        let nested_match = test_match(nested_file.clone(), 1, false);
        let parent_match = test_match(parent_file.clone(), 1, false);

        let mut parent_ranked = vec![nested_match.clone(), parent_match.clone()];
        rank::sort(&mut parent_ranked, "target", &parent, None);
        assert_eq!(parent_ranked[0].path, parent_file);

        let mut child_ranked = vec![parent_match.clone(), nested_match.clone()];
        rank::sort(&mut child_ranked, "target", &child, None);
        assert_eq!(child_ranked[0].path, nested_file);

        for scopes in [
            vec![parent.clone(), child.clone()],
            vec![child.clone(), parent.clone()],
        ] {
            let combined = combine_scoped_results(&scopes, false, None, None, |scope| {
                if scope == parent {
                    Ok(result(
                        scope,
                        vec![nested_match.clone(), parent_match.clone()],
                    ))
                } else {
                    Ok(result(scope, vec![nested_match.clone()]))
                }
            })
            .expect("overlapping scopes should combine");

            assert_eq!(combined.total_found, 2);
            assert_eq!(combined.matches[0].path, nested_file);
            assert_eq!(combined.scope_counts.len(), 2);
            assert!(combined
                .scope_counts
                .iter()
                .any(|(scope, count)| { scope == &parent && *count == 2 }));
            assert!(combined
                .scope_counts
                .iter()
                .any(|(scope, count)| { scope == &child && *count == 1 }));
        }
    }

    #[test]
    fn nested_path_observed_only_by_parent_does_not_gain_child_proximity() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let child = parent.join("c");
        let nested_file = child.join("m.rs");
        let parent_file = parent.join("a").join("n.rs");
        std::fs::create_dir_all(nested_file.parent().unwrap()).unwrap();
        std::fs::create_dir_all(parent_file.parent().unwrap()).unwrap();
        std::fs::write(&nested_file, "target\n").unwrap();
        std::fs::write(&parent_file, "target\n").unwrap();

        let combined =
            combine_scoped_results(&[parent.clone(), child], false, None, None, |scope| {
                if scope == parent {
                    Ok(result(
                        scope,
                        vec![
                            test_match(nested_file.clone(), 1, false),
                            test_match(parent_file.clone(), 1, false),
                        ],
                    ))
                } else {
                    Ok(result(scope, Vec::new()))
                }
            })
            .expect("parent observation should combine");

        assert_eq!(combined.matches[0].path, parent_file);
    }

    #[cfg(unix)]
    #[test]
    fn canonical_observation_controls_alias_display_and_facets() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let source_scope = root.join("src");
        let test_scope = root.join("tests");
        std::fs::create_dir_all(&source_scope).unwrap();
        std::fs::create_dir_all(&test_scope).unwrap();
        let source = source_scope.join("a.rs");
        let alias = test_scope.join("a.rs");
        std::fs::write(&source, "target\n").unwrap();
        symlink("../src/a.rs", &alias).unwrap();

        for scopes in [
            vec![source_scope.clone(), test_scope.clone()],
            vec![test_scope.clone(), source_scope.clone()],
        ] {
            let combined = combine_scoped_results(&scopes, false, None, None, |scope| {
                if scope == source_scope {
                    Ok(result(scope, vec![test_match(source.clone(), 1, false)]))
                } else {
                    Ok(result(scope, vec![test_match(alias.clone(), 1, false)]))
                }
            })
            .expect("alias observations should combine");

            assert_eq!(combined.total_found, 1);
            assert_eq!(combined.matches.len(), 1);
            assert_eq!(combined.matches[0].path, source);
            assert_eq!(combined.facet_totals.tests, 0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn alias_only_observations_choose_shortest_observed_display_path() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let physical_scope = root.join("physical");
        let source_scope = root.join("src");
        let test_scope = root.join("tests");
        for scope in [&physical_scope, &source_scope, &test_scope] {
            std::fs::create_dir_all(scope).unwrap();
        }
        let physical = physical_scope.join("a.rs");
        let source_alias = source_scope.join("a.rs");
        let test_alias = test_scope.join("a.rs");
        std::fs::write(&physical, "target\n").unwrap();
        symlink("../physical/a.rs", &source_alias).unwrap();
        symlink("../physical/a.rs", &test_alias).unwrap();

        for scopes in [
            vec![source_scope.clone(), test_scope.clone()],
            vec![test_scope.clone(), source_scope.clone()],
        ] {
            let combined = combine_scoped_results(&scopes, false, None, None, |scope| {
                if scope == source_scope {
                    Ok(result(
                        scope,
                        vec![test_match(source_alias.clone(), 1, false)],
                    ))
                } else {
                    Ok(result(
                        scope,
                        vec![test_match(test_alias.clone(), 1, false)],
                    ))
                }
            })
            .expect("alias observations should combine");

            assert_eq!(combined.total_found, 1);
            assert_eq!(combined.matches[0].path, source_alias);
            assert_ne!(combined.matches[0].path, physical);
        }
    }

    #[test]
    fn one_root_content_count_and_hidden_facet_are_exact() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let test_dir = root.join("tests");
        std::fs::create_dir_all(&test_dir).unwrap();
        std::fs::write(test_dir.join("needles.rs"), "one_root_needle\n".repeat(31)).unwrap();

        let result = content_raw_scopes(
            "one_root_needle",
            std::slice::from_ref(&root),
            None,
            Some("*.rs"),
            false,
        )
        .expect("one-root content search should succeed");

        assert_eq!(result.total_found, 31);
        assert_eq!(
            result.scope_counts,
            vec![(root.canonicalize().unwrap(), 31)]
        );
        assert_eq!(result.matches.len(), 10);
        assert_eq!(result.facet_totals.tests, 31);
    }

    #[test]
    fn overlapping_content_ledgers_count_hidden_hits_and_facets_once() {
        let tmp = tempfile::tempdir().unwrap();
        let parent = tmp.path().join("parent");
        let child = parent.join("c");
        let test_dir = child.join("tests");
        std::fs::create_dir_all(&test_dir).unwrap();
        std::fs::write(
            test_dir.join("hidden.rs"),
            "overlap_ledger_needle\n".repeat(350),
        )
        .unwrap();
        std::fs::write(
            parent.join("extra.rs"),
            "overlap_ledger_needle\n".repeat(360),
        )
        .unwrap();
        let parent_canonical = parent.canonicalize().unwrap();
        let child_canonical = child.canonicalize().unwrap();

        for scopes in [
            vec![parent.clone(), child.clone()],
            vec![child.clone(), parent.clone()],
        ] {
            let combined =
                content_raw_scopes("overlap_ledger_needle", &scopes, None, Some("*.rs"), false)
                    .expect("overlapping content search should succeed");

            assert_eq!(combined.total_found, 710);
            assert_eq!(combined.usages, 710);
            assert_eq!(combined.matches.len(), 10);
            assert_eq!(combined.facet_totals.tests, 350);
            assert_eq!(combined.facet_totals.usages_cross, 360);
            assert!(combined
                .scope_counts
                .iter()
                .any(|(scope, count)| { scope == &parent_canonical && *count == 710 }));
            assert!(combined
                .scope_counts
                .iter()
                .any(|(scope, count)| { scope == &child_canonical && *count == 350 }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn content_aliases_are_deduped_before_candidate_retention() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let physical_dir = root.join("physical");
        std::fs::create_dir_all(&physical_dir).unwrap();
        std::fs::write(root.join(".tilthignore"), "physical/\n").unwrap();
        let physical = physical_dir.join("alias.rs");
        std::fs::write(&physical, "alias_retention_needle\n").unwrap();
        for i in 0..301 {
            symlink("physical/alias.rs", root.join(format!("alias_{i:03}.rs"))).unwrap();
        }
        for i in 0..10 {
            std::fs::write(
                root.join(format!("unique_{i:02}.rs")),
                "alias_retention_needle\n",
            )
            .unwrap();
        }

        let result = content_raw_scopes(
            "alias_retention_needle",
            std::slice::from_ref(&root),
            None,
            Some("*.rs"),
            false,
        )
        .expect("alias-heavy content search should succeed");
        let identities: HashSet<_> = result
            .matches
            .iter()
            .map(|m| m.path.canonicalize().unwrap())
            .collect();

        assert_eq!(result.total_found, 11);
        assert_eq!(result.matches.len(), 10);
        assert_eq!(identities.len(), 10);
    }

    #[test]
    fn all_failed_scopes_are_reported_in_input_order() {
        let tmp = tempfile::tempdir().unwrap();
        let first = tmp.path().join("first");
        let second = tmp.path().join("second");
        let third = tmp.path().join("third");
        let scopes = [first.clone(), second.clone(), third.clone()];

        let error = combine_scoped_results(&scopes, false, None, None, |scope| {
            Err(TilthError::NotFound {
                path: scope.to_path_buf(),
                suggestion: None,
            })
        })
        .expect_err("all failed scopes should return an aggregate error");
        let message = error.to_string();
        let first_position = message.find(first.to_str().unwrap()).unwrap();
        let second_position = message.find(second.to_str().unwrap()).unwrap();
        let third_position = message.find(third.to_str().unwrap()).unwrap();

        assert!(first_position < second_position && second_position < third_position);
    }

    #[test]
    fn partial_scopes_keep_successes_and_each_typed_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let good = tmp.path().join("good");
        let denied = tmp.path().join("denied");
        let broken = tmp.path().join("broken");
        std::fs::create_dir_all(&good).unwrap();
        let file = good.join("hit.rs");
        std::fs::write(&file, "target\ntarget\n").unwrap();
        let combined = combine_scoped_results(
            &[good.clone(), denied.clone(), broken.clone()],
            false,
            None,
            None,
            |scope| {
                if scope == good {
                    Ok(result(
                        scope,
                        vec![
                            test_match(file.clone(), 1, false),
                            test_match(file.clone(), 2, false),
                        ],
                    ))
                } else if scope == denied {
                    Err(TilthError::PermissionDenied {
                        path: scope.to_path_buf(),
                    })
                } else {
                    Err(TilthError::IoError {
                        path: scope.to_path_buf(),
                        source: std::io::Error::other("broken"),
                    })
                }
            },
        )
        .unwrap();
        assert_eq!(combined.total_found, 2);
        assert_eq!(
            combined.count_estimate,
            CountEstimate::Observed { count: 2 }
        );
        assert_eq!(combined.scope_counts, vec![(good, 2)]);
        assert_eq!(combined.scope_errors.len(), 2);
        assert_eq!(combined.scope_errors[0].scope, denied);
        assert_eq!(combined.scope_errors[0].error.exit_code(), 4);
        assert_eq!(combined.scope_errors[1].scope, broken);
        assert_eq!(combined.scope_errors[1].error.exit_code(), 2);
    }

    #[test]
    fn singleton_failure_keeps_legacy_error_variant_and_code() {
        let scope = PathBuf::from("/one");
        let error = combine_scoped_results(std::slice::from_ref(&scope), false, None, None, |_| {
            Err(TilthError::PermissionDenied {
                path: scope.clone(),
            })
        })
        .unwrap_err();
        assert!(matches!(error, TilthError::PermissionDenied { .. }));
        assert_eq!(error.exit_code(), 4);
    }

    #[cfg(unix)]
    #[test]
    fn one_root_aliases_use_physical_identity_and_canonical_facet() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        let source = root.join("src/a.rs");
        let alias = root.join("tests/a.rs");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        std::fs::create_dir_all(alias.parent().unwrap()).unwrap();
        std::fs::write(&source, "one_root_alias_needle\n").unwrap();
        symlink("../src/a.rs", &alias).unwrap();
        for search in [content_raw_scopes, regex_raw_scopes] {
            let result = search(
                "one_root_alias_needle",
                std::slice::from_ref(&root),
                None,
                Some("*.rs"),
                false,
            )
            .unwrap();
            assert_eq!(result.total_found, 1);
            assert_eq!(result.count_estimate, CountEstimate::Exact(1));
            assert_eq!(result.scope_counts[0].1, 1);
            assert_eq!(result.matches[0].path, source);
            assert_eq!(result.facet_totals.tests, 0);
            assert_eq!(result.facet_totals.usages_cross, 1);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symbol_aliases_do_not_consume_distinct_candidate_quota() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.rs");
        std::fs::write(&source, "fn alias_quota_target() {}\n").unwrap();
        for i in 0..60 {
            symlink("source.rs", root.join(format!("alias_{i:02}.rs"))).unwrap();
        }
        for i in 0..10 {
            std::fs::write(
                root.join(format!("unique_{i:02}.rs")),
                "fn alias_quota_target() {}\n",
            )
            .unwrap();
        }
        let result = symbol_raw_scopes(
            "alias_quota_target",
            std::slice::from_ref(&root),
            None,
            Some("*.rs"),
            false,
        )
        .unwrap();
        assert_eq!(result.total_found, 11);
        assert_eq!(result.count_estimate, CountEstimate::Exact(11));
        assert_eq!(result.matches.len(), 10);
        let identities: HashSet<_> = result.matches.iter().map(match_identity).collect();
        assert_eq!(identities.len(), 10);
        assert!(result.matches.iter().any(|m| m.path == source));
    }

    #[cfg(unix)]
    #[test]
    fn alias_score_uses_actual_observation_while_displaying_canonical_path() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let physical = root.join("other.rs");
        let alias = root.join("target.rs");
        let competitor = root.join("targetish.rs");
        std::fs::write(&physical, "target\n").unwrap();
        std::fs::write(&competitor, "target\n").unwrap();
        symlink("other.rs", &alias).unwrap();
        let mut collected = result(
            root,
            vec![
                test_match(physical.clone(), 1, false),
                test_match(competitor.clone(), 1, false),
            ],
        );
        collected
            .alias_paths
            .insert((physical.clone(), false), vec![physical.clone(), alias]);
        let mut collected = Some(collected);
        let combined = combine_scoped_results(&[root.to_path_buf()], false, None, None, |_| {
            Ok(collected.take().expect("one root"))
        })
        .unwrap();
        assert_eq!(combined.total_found, 2);
        assert_eq!(combined.matches[0].path, physical);
    }

    #[cfg(unix)]
    #[test]
    fn alias_rank_survives_local_candidate_retention() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let physical = root.join("other.rs");
        std::fs::write(&physical, "target\n").unwrap();
        symlink("other.rs", root.join("target.rs")).unwrap();
        for i in 0..31 {
            std::fs::write(root.join(format!("targetish_{i:02}.rs")), "target\n").unwrap();
        }
        let result =
            content_raw_scopes("target", &[root.to_path_buf()], None, Some("*.rs"), false).unwrap();
        assert_eq!(result.total_found, 32);
        assert_eq!(result.matches.len(), 10);
        assert_eq!(result.matches[0].path, physical);
    }

    #[cfg(unix)]
    #[test]
    fn usage_only_alias_does_not_boost_definition_rank() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let physical = root.join("other.rs");
        let alias = root.join("target.rs");
        let competitor = root.join("targetish.rs");
        std::fs::write(&physical, "fn target() {}\n").unwrap();
        std::fs::write(&competitor, "fn target() {}\n").unwrap();
        symlink(&physical, &alias).unwrap();
        let mut collected = result(
            root,
            vec![
                test_match(physical.clone(), 1, true),
                test_match(competitor.clone(), 1, true),
            ],
        );
        collected
            .alias_paths
            .insert((physical.clone(), true), vec![physical.clone()]);
        collected
            .alias_paths
            .insert((physical.clone(), false), vec![alias]);
        let mut collected = Some(collected);
        let combined = combine_scoped_results(&[root.to_path_buf()], false, None, None, |_| {
            Ok(collected.take().expect("one root"))
        })
        .unwrap();
        assert_eq!(combined.matches[0].path, competitor);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_root_keeps_root_relative_display_path() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let physical = tmp.path().join("physical");
        let alias = tmp.path().join("alias");
        std::fs::create_dir_all(&physical).unwrap();
        std::fs::write(physical.join("lib.rs"), "root_alias_needle\n").unwrap();
        symlink(&physical, &alias).unwrap();
        let result = content_raw_scopes(
            "root_alias_needle",
            std::slice::from_ref(&alias),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(result.scope, alias);
        let out = crate::search::format_raw_result_scopes(
            &result,
            std::slice::from_ref(&alias),
            &crate::cache::OutlineCache::new(),
        )
        .unwrap();
        assert!(out.contains("lib.rs"), "{out}");
        assert!(!out.contains(&format!("{}:", physical.display())), "{out}");
    }

    #[cfg(unix)]
    #[test]
    fn alias_only_test_path_uses_physical_facet_in_display() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("src/a.rs"), "facet_identity_needle\n".repeat(6)).unwrap();
        symlink("../src/a.rs", root.join("tests/a.rs")).unwrap();
        let tests_scope = root.join("tests");
        let result = content_raw_scopes(
            "facet_identity_needle",
            std::slice::from_ref(&tests_scope),
            None,
            None,
            false,
        )
        .unwrap();
        assert_eq!(result.total_found, 6);
        assert_eq!(result.facet_totals.tests, 0);
        let out = crate::search::format_raw_result_scopes(
            &result,
            std::slice::from_ref(&tests_scope),
            &crate::cache::OutlineCache::new(),
        )
        .unwrap();
        assert!(out.contains("tests/a.rs") || out.contains("a.rs"), "{out}");
        assert!(!out.contains("## Tests"), "{out}");
        assert!(out.contains("## Usages — other"), "{out}");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_and_relative_scope_headers_show_successful_counts() {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir().unwrap();
        let physical = tmp.path().join("physical");
        let alias = tmp.path().join("alias");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&physical).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(physical.join("a.rs"), "scope_header_needle\n").unwrap();
        std::fs::write(other.join("b.rs"), "scope_header_needle\n").unwrap();
        symlink(&physical, &alias).unwrap();
        let scopes = [alias.clone(), other.clone()];
        let result = content_raw_scopes("scope_header_needle", &scopes, None, None, false).unwrap();
        let out = crate::search::format_raw_result_scopes(
            &result,
            &scopes,
            &crate::cache::OutlineCache::new(),
        )
        .unwrap();
        assert!(out.contains(&format!("{} (1)", alias.display())), "{out}");
        assert!(out.contains(&format!("{} (1)", other.display())), "{out}");
        assert!(!out.contains("(error)"), "{out}");
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
