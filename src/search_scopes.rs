//! Resolve search scopes without widening file or glob selections.
use crate::error::TilthError;
use std::path::{Path, PathBuf};

const MAX_SCAN_ENTRIES: usize = 100_000;
const MAX_EXPANDED_SCOPES: usize = 1_000;

/// Expand glob scopes into eligible paths; keep literal paths for normal error handling.
pub fn expand(scopes: &[PathBuf]) -> Result<Vec<PathBuf>, TilthError> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut scanned_entries = 0;
    let mut expanded_paths = 0;
    for scope in scopes {
        if scope.exists() || !has_glob(scope) {
            let path = scope.canonicalize().unwrap_or_else(|_| scope.clone());
            if seen.insert(path.clone()) {
                result.push(path);
            }
            continue;
        }
        let matches = expand_glob(scope, &mut scanned_entries, &mut expanded_paths)?;
        for path in matches {
            let path = path.canonicalize().unwrap_or(path);
            if seen.insert(path.clone()) {
                result.push(path);
            }
        }
    }
    Ok(result)
}

fn expand_glob(
    scope: &Path,
    scanned_entries: &mut usize,
    expanded_paths: &mut usize,
) -> Result<Vec<PathBuf>, TilthError> {
    let root = scope
        .ancestors()
        .skip(1)
        .find(|path| !has_glob(path))
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let pattern = scope.strip_prefix(root).unwrap_or(scope).to_string_lossy();
    let matcher = globset::GlobBuilder::new(&pattern)
        .literal_separator(true)
        .build()
        .map_err(|error| TilthError::InvalidQuery {
            query: scope.display().to_string(),
            reason: format!("invalid scope glob: {error}"),
        })?
        .compile_matcher();
    if !root.is_dir() {
        return Ok(vec![scope.to_path_buf()]);
    }
    let mut matches = Vec::new();
    for entry in crate::search::base_walk_builder(root).build() {
        *scanned_entries += 1;
        if *scanned_entries > MAX_SCAN_ENTRIES {
            return Err(TilthError::InvalidQuery {
                query: scope.display().to_string(),
                reason: "scope glob scan limit reached; use a narrower scope".into(),
            });
        }
        let entry = entry.map_err(|error| TilthError::InvalidQuery {
            query: scope.display().to_string(),
            reason: error.to_string(),
        })?;
        if entry.depth() > 0
            && !matches
                .iter()
                .any(|path: &PathBuf| entry.path().starts_with(path))
            && entry
                .file_type()
                .is_some_and(|kind| kind.is_file() || kind.is_dir())
            && matcher.is_match(entry.path().strip_prefix(root).unwrap_or(entry.path()))
        {
            matches.push(entry.into_path());
            *expanded_paths += 1;
            if *expanded_paths > MAX_EXPANDED_SCOPES {
                return Err(TilthError::InvalidQuery {
                    query: scope.display().to_string(),
                    reason: "scope glob matched too many paths; use a narrower scope".into(),
                });
            }
        }
    }
    matches.sort();
    if matches.is_empty() {
        // Preserve a failed scope for the normal multi-scope reducer, which
        // can still return valid results from the other selections.
        matches.push(scope.to_path_buf());
    }
    Ok(matches)
}

pub(crate) fn has_glob(path: &Path) -> bool {
    path.to_string_lossy().contains(['*', '?', '[', '{'])
}
