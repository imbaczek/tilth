//! Bounded file and directory hints for missing search scopes.
use std::path::{Path, PathBuf};

const MAX_DIRECTORIES: usize = 5_000;
const MAX_SCAN_ENTRIES: usize = 10_000;
const MAX_HINTS: usize = 5;
const MAX_DEPTH: usize = 6;
const STRONG_FILE_SIMILARITY: f64 = 0.8;
const RELATED_FILE_SIMILARITY: f64 = 0.4;

pub(crate) fn suggestion_suffix(scope: &Path) -> String {
    if !matches!(std::fs::metadata(scope), Err(ref error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return String::new();
    }
    let files = file_suggestions(scope);
    let (label, hints) = if files.is_empty() {
        ("directories", directory_suggestions(scope))
    } else {
        let label = if files[0].0 < STRONG_FILE_SIMILARITY {
            "files (lower confidence)"
        } else {
            "files"
        };
        (label, files.into_iter().map(|(_, path)| path).collect())
    };
    if hints.is_empty() {
        String::new()
    } else {
        format!(
            " — did you mean {label}: {}",
            hints
                .iter()
                .map(|p| format!("\"{}\"", p.display()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Compare basenames so a correct filename can still be found under a moved directory.
fn file_suggestions(scope: &Path) -> Vec<(f64, PathBuf)> {
    let absolute = std::path::absolute(scope).unwrap_or_else(|_| scope.to_path_buf());
    let Some(name) = absolute.file_name() else {
        return Vec::new();
    };
    let target = name.to_string_lossy().to_lowercase();
    let Some(ancestor) = absolute.ancestors().skip(1).find(|path| path.is_dir()) else {
        return Vec::new();
    };
    if ancestor.parent().is_none() {
        return Vec::new();
    }
    let mut builder = crate::search::base_walk_builder(ancestor);
    builder.follow_links(false).max_depth(Some(MAX_DEPTH));
    let mut strong = Vec::new();
    let mut related = Vec::new();
    for entry in builder
        .build()
        .take(MAX_SCAN_ENTRIES)
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let candidate = entry.file_name().to_string_lossy().to_lowercase();
        let score = strsim::normalized_levenshtein(&target, &candidate);
        if score >= STRONG_FILE_SIMILARITY {
            strong.push((score, entry.into_path()));
        } else if score >= RELATED_FILE_SIMILARITY && shares_filename_component(&target, &candidate)
        {
            related.push((score, entry.into_path()));
        }
    }
    // Never dilute strong spelling corrections with more speculative relatives.
    let mut candidates = if strong.is_empty() { related } else { strong };
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    candidates.truncate(MAX_HINTS);
    candidates
}

fn shares_filename_component(target: &str, candidate: &str) -> bool {
    let target = Path::new(target);
    let candidate = Path::new(candidate);
    let Some(extension) = target.extension().filter(|extension| !extension.is_empty()) else {
        return false;
    };
    if candidate.extension() != Some(extension) {
        return false;
    }
    let target_stem = target
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    let candidate_stem = candidate
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("");
    target_stem
        .split(|ch: char| !ch.is_alphanumeric())
        .any(|component| {
            component.chars().count() >= 2
                && candidate_stem
                    .split(|ch: char| !ch.is_alphanumeric())
                    .any(|other| other == component)
        })
}

fn directory_suggestions(scope: &Path) -> Vec<PathBuf> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let absolute = if scope.is_absolute() {
        scope.to_path_buf()
    } else {
        cwd.join(scope)
    };
    let Some(ancestor) = absolute.ancestors().skip(1).find(|p| p.is_dir()) else {
        return Vec::new();
    };
    let Ok(target) = absolute.strip_prefix(ancestor) else {
        return Vec::new();
    };
    let target = target.to_string_lossy().to_lowercase();
    // Do not recursively scan the filesystem root for a nonexistent top-level path.
    let depth = if ancestor.parent().is_none() {
        1
    } else {
        MAX_DEPTH
    };
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();
    // Inspect immediate siblings before walking into unrelated subtrees. Count
    // files as well as directories against each scan's entry budget, and avoid
    // walker sorting, which would read an entire large directory before yielding.
    for (pass, scan_depth) in [1, depth].into_iter().enumerate() {
        if candidates.len() >= MAX_DIRECTORIES || (pass == 1 && depth == 1) {
            break;
        }
        let mut builder = crate::search::base_walk_builder(ancestor);
        builder.follow_links(false).max_depth(Some(scan_depth));
        for entry in builder
            .build()
            .take(MAX_SCAN_ENTRIES)
            .filter_map(Result::ok)
        {
            if entry.depth() == 0
                || !entry.file_type().is_some_and(|kind| {
                    kind.is_dir() || (kind.is_symlink() && entry.path().is_dir())
                })
                || !crate::search::include_entry(&entry)
            {
                continue;
            }
            let path = entry.into_path();
            if !seen.insert(path.clone()) {
                continue;
            }
            let Ok(relative) = path.strip_prefix(ancestor) else {
                continue;
            };
            let spelling = relative.to_string_lossy().to_lowercase();
            let similarity = strsim::normalized_levenshtein(&target, &spelling);
            let display = if scope.is_absolute() {
                path.clone()
            } else {
                scope
                    .ancestors()
                    .find(|p| cwd.join(p) == ancestor)
                    .map_or_else(|| path.clone(), |prefix| prefix.join(relative))
            };
            candidates.push((similarity, display));
            if candidates.len() >= MAX_DIRECTORIES {
                break;
            }
        }
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    candidates
        .into_iter()
        .take(MAX_HINTS)
        .map(|(_, path)| path)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn related_filename_fallback_requires_shared_component_and_extension() {
        let root = tempfile::tempdir().unwrap();
        for name in ["wal.lisp", "bufmgr.lisp", "wal.rs", "notwal.lisp"] {
            fs::write(root.path().join(name), "").unwrap();
        }
        let missing = root.path().join("wal-checkpoint.lisp");
        let hints = file_suggestions(&missing);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].1, root.path().join("wal.lisp"));
        assert!((hints[0].0 - 8.0 / 19.0).abs() < 1e-9);
        assert!(suggestion_suffix(&missing).contains("files (lower confidence)"));
        assert!(!shares_filename_component(
            "a-checkpoint.lisp",
            "a-other.lisp"
        ));
        assert!(!shares_filename_component("wal-checkpoint", "wal"));
        assert!(file_suggestions(&root.path().join("wal-veryverylongcheckpoint.lisp")).is_empty());
    }

    #[test]
    fn strong_corrections_suppress_related_fallbacks() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("wal.lisp"), "").unwrap();
        let strong = root.path().join("wal-checkpoints.lisp");
        fs::write(&strong, "").unwrap();
        let missing = root.path().join("wal-checkpoint.lisp");
        let hints = file_suggestions(&missing);
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].1, strong);
        assert!(!suggestion_suffix(&missing).contains("lower confidence"));
    }

    #[test]
    fn related_fallbacks_are_ranked_capped_and_ignore_aware() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..10 {
            fs::write(root.path().join(format!("wal-{index}.lisp")), "").unwrap();
        }
        let missing = root.path().join("wal-checkpoint.lisp");
        let hints = file_suggestions(&missing);
        assert_eq!(hints.len(), MAX_HINTS);
        assert!(hints.windows(2).all(|pair| pair[0].0 >= pair[1].0));
        let paths: Vec<_> = hints.into_iter().map(|(_, path)| path).collect();
        assert!(paths.windows(2).all(|pair| pair[0] < pair[1]));
        fs::write(root.path().join(".tilthignore"), "wal-*.lisp\n").unwrap();
        assert!(file_suggestions(&missing).is_empty());
    }

    #[test]
    fn ranks_leaf_and_intermediate_typos_and_excludes_files() {
        let root = tempfile::tempdir().unwrap();
        for dir in [
            "quant/transformers",
            "quant/transport",
            "quantity/transformer",
            "other",
        ] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        fs::write(root.path().join("quant/transformer"), "file").unwrap();
        let hints = directory_suggestions(&root.path().join("quant/transfomer"));
        assert_eq!(hints[0], root.path().join("quant/transformers"));
        assert!(hints.iter().all(|p| p.is_dir()));
        let hints = directory_suggestions(&root.path().join("qunat/transformers"));
        assert_eq!(hints[0], root.path().join("quant/transformers"));
        assert!(hints.len() <= MAX_HINTS);
    }

    #[test]
    fn empty_directory_has_no_hints() {
        let root = tempfile::tempdir().unwrap();
        assert!(suggestion_suffix(&root.path().join("missing")).is_empty());
    }

    #[test]
    fn ties_are_sorted_and_hint_count_is_bounded() {
        let root = tempfile::tempdir().unwrap();
        for letter in ['f', 'e', 'd', 'c', 'b', 'a'] {
            fs::create_dir(root.path().join(format!("dir{letter}"))).unwrap();
        }
        let hints = directory_suggestions(&root.path().join("dirz"));
        assert_eq!(hints.len(), MAX_HINTS);
        assert_eq!(hints[0], root.path().join("dira"));
        assert_eq!(hints[4], root.path().join("dire"));
    }

    #[test]
    fn only_missing_scope_errors_receive_hints() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("existing")).unwrap();
        let missing = root.path().join("exsting");
        let error = crate::error::scope_io_error(&missing, fs::metadata(&missing).unwrap_err());
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("existing"));
        let error = crate::error::scope_io_error(
            &missing,
            std::io::Error::from(std::io::ErrorKind::PermissionDenied),
        );
        assert!(!error.to_string().contains("did you mean"));
    }
}

#[cfg(test)]
mod directory_walk_tests {
    use super::*;
    use std::fs;

    #[test]
    fn files_do_not_exhaust_directory_candidate_limit() {
        let root = tempfile::tempdir().unwrap();
        for index in 0..=MAX_DIRECTORIES {
            fs::write(root.path().join(format!("a{index:05}.txt")), "").unwrap();
        }
        let expected = root.path().join("ztransformers");
        fs::create_dir(&expected).unwrap();
        assert_eq!(
            directory_suggestions(&root.path().join("ztransformer"))[0],
            expected
        );
    }

    #[test]
    fn tilthignore_removes_directory_candidates() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("transformers")).unwrap();
        fs::create_dir(root.path().join("transport")).unwrap();
        fs::write(root.path().join(".tilthignore"), "transformers/\n").unwrap();
        let hints = directory_suggestions(&root.path().join("transformer"));
        assert_eq!(hints, vec![root.path().join("transport")]);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_directories_are_suggested_without_following_cycles() {
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(root.path(), root.path().join("transformers")).unwrap();
        let hints = directory_suggestions(&root.path().join("transformer"));
        assert_eq!(hints, vec![root.path().join("transformers")]);
    }
}
