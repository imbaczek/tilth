//! Bounded directory hints for misspelled search scopes.
use std::path::{Path, PathBuf};

const MAX_DIRECTORIES: usize = 5_000;
const MAX_SCAN_ENTRIES: usize = 10_000;
const MAX_HINTS: usize = 5;
const MAX_DEPTH: usize = 6;

pub(crate) fn suggestion_suffix(scope: &Path) -> String {
    if !matches!(std::fs::metadata(scope), Err(ref error) if error.kind() == std::io::ErrorKind::NotFound)
    {
        return String::new();
    }
    let hints = directory_suggestions(scope);
    if hints.is_empty() {
        String::new()
    } else {
        format!(
            " — did you mean directories: {}",
            hints
                .iter()
                .map(|p| format!("\"{}\"", p.display()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
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
