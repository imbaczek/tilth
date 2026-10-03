//! `tilth_list` — directory tree with per-directory token-cost rollups.
//!
//! Resolves each glob against a walk of `scope`, collects `(path, byte_len)`
//! pairs, and renders them as a single tree rooted at scope.

use std::fmt::Write as _;
use std::path::PathBuf;

use serde_json::Value;

use super::{apply_budget, resolve_scope};

pub(in crate::mcp) fn tool_list(args: &Value) -> Result<String, String> {
    use globset::Glob;
    let root = args
        .get("root")
        .and_then(|v| v.as_str())
        .map(std::path::Path::new);
    let (scope, scope_warning) = resolve_scope(args, root)?;
    let budget = args.get("budget").and_then(serde_json::Value::as_u64);
    let page = crate::listing::Page::from_args(args)?;

    let patterns_arr = args
        .get("patterns")
        .and_then(|v| v.as_array())
        .ok_or("missing required parameter: patterns (array of globs)")?;
    if patterns_arr.is_empty() {
        return Err("patterns must contain at least one glob".into());
    }
    if patterns_arr.len() > 20 {
        return Err(format!(
            "patterns limited to 20 per call (got {})",
            patterns_arr.len()
        ));
    }
    let patterns: Vec<String> = patterns_arr
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or("patterns must be an array of strings")
                .map(String::from)
        })
        .collect::<Result<_, _>>()?;

    let depth = args.get("depth").and_then(|v| {
        v.as_u64()
            .map(|d| d as usize)
            .or_else(|| v.as_f64().map(|f| f as usize))
    });

    let matchers: Vec<_> = patterns
        .iter()
        .filter_map(|p| Glob::new(p).ok().map(|g| g.compile_matcher()))
        .collect();
    if matchers.is_empty() {
        return Err("no valid globs provided".into());
    }

    // Walk the scope directory (shared junk-dir policy), bounding the walk itself
    // by `depth` (ignore's max_depth: root=0, direct children=1 — matches
    // rel.components().count() for a top-level file, so depth:1 keeps it),
    // and collect every file matching any pattern into one token-rolled-up tree.
    let mut builder = crate::search::base_walk_builder(&scope);
    if let Some(d) = depth {
        builder.max_depth(Some(d));
    }
    let walker = builder.build();
    let mut entries: Vec<(PathBuf, u64)> = Vec::new();
    let mut extensions: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in walker.filter_map(Result::ok) {
        let Some(ft) = entry.file_type() else {
            continue;
        };
        if !ft.is_file() {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(&scope).unwrap_or(path);
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            extensions.insert(ext.to_string());
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let matched = matchers.iter().any(|m| m.is_match(name) || m.is_match(rel));
        if matched {
            let bytes = entry.metadata().map_or(0, |m| m.len());
            entries.push((path.to_path_buf(), bytes));
        }
    }

    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let (start, end) = page.bounds(entries.len());
    let tree = crate::mcp::tree::render_tree(&scope, &entries[start..end]);
    let mut result = scope_warning.unwrap_or_default();
    result.push_str(&page.summary(entries.len(), "files"));
    result.push_str("\n> Tree counts and token rollups cover this page only.\n");
    result.push_str(&tree);
    page.append_navigation(&mut result, entries.len(), true);
    if entries.is_empty() {
        if extensions.is_empty() {
            result.push_str("\nno matches\n");
        } else {
            let exts: Vec<String> = extensions.into_iter().take(10).collect();
            let _ = write!(
                result,
                "\nno matches; found extensions: {}\n",
                exts.join(", ")
            );
        }
    }
    Ok(apply_budget(&result, budget))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a small scratch project with nested .rs and a .toml, returning the
    /// tempdir guard so the caller controls cleanup.
    fn scratch_project() -> tempfile::TempDir {
        let project = tempfile::tempdir().unwrap();
        let p = project.path();
        std::fs::write(p.join("Cargo.toml"), "[package]\nname = \"t\"").unwrap();
        std::fs::create_dir(p.join("src")).unwrap();
        std::fs::write(p.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(p.join("src/lib.rs"), "pub fn x() {}").unwrap();
        project
    }

    #[test]
    fn tool_list_renders_tree_with_dirs_files_and_rollups() {
        let project = scratch_project();
        let args = serde_json::json!({
            "patterns": ["*.rs"],
            "scope": project.path().to_str().unwrap(),
        });
        let out = tool_list(&args).expect("tool_list should succeed");
        // Tree groups the src/ directory and its two .rs leaves.
        assert!(out.contains("src/"), "expected src/ dir node: {out}");
        assert!(out.contains("main.rs"), "expected main.rs leaf: {out}");
        assert!(out.contains("lib.rs"), "expected lib.rs leaf: {out}");
        // *.rs must not pull in the Cargo.toml sibling.
        assert!(
            !out.contains("Cargo.toml"),
            "*.rs must not match Cargo.toml: {out}"
        );
        // Per-node token rollups are the whole point of the tree view.
        assert!(out.contains("tokens"), "expected token rollups: {out}");
    }

    #[test]
    fn tool_list_empty_patterns_errors() {
        let args = serde_json::json!({ "patterns": [], "scope": env!("CARGO_MANIFEST_DIR") });
        let err = tool_list(&args).expect_err("expected empty-patterns error");
        assert!(err.contains("at least one"), "unexpected error: {err}");
    }

    #[test]
    fn tool_list_missing_patterns_errors() {
        let args = serde_json::json!({ "scope": env!("CARGO_MANIFEST_DIR") });
        let err = tool_list(&args).expect_err("expected missing-patterns error");
        assert!(
            err.contains("missing required parameter"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn tool_list_patterns_capped_at_20() {
        let twenty_one: Vec<&str> = vec!["*.rs"; 21];
        let args =
            serde_json::json!({ "patterns": twenty_one, "scope": env!("CARGO_MANIFEST_DIR") });
        let err = tool_list(&args).expect_err("expected cap error");
        assert!(err.contains("limited to 20"), "unexpected error: {err}");
    }

    #[test]
    fn tool_list_depth_caps_nesting() {
        let project = scratch_project();
        // depth=1 keeps only top-level entries; src/*.rs is at depth 2 and drops.
        let args = serde_json::json!({
            "patterns": ["*.toml"],
            "depth": 1,
            "scope": project.path().to_str().unwrap(),
        });
        let out = tool_list(&args).expect("tool_list should succeed");
        assert!(out.contains("Cargo.toml"), "top-level toml kept: {out}");
    }

    #[test]
    fn tool_list_relative_scope_absolute_root_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.rs"), "fn a() {}\n").unwrap();
        let args = serde_json::json!({
            "patterns": ["*.rs"],
            "scope": "sub",
            "root": tmp.path().to_str().unwrap(),
        });
        let out = tool_list(&args).expect("relative scope + absolute root resolves");
        assert!(
            out.contains("a.rs"),
            "expected listing under anchored root: {out}"
        );
    }

    #[test]
    fn tool_list_explicit_relative_scope_no_root_errors() {
        let args = serde_json::json!({ "patterns": ["*.rs"], "scope": "some/relative/dir" });
        let err = tool_list(&args).expect_err("explicit relative scope must refuse without root");
        assert!(
            err.contains("relative scope") && err.contains("root"),
            "explicit relative scope without root must refuse: {err}"
        );
    }

    #[test]
    fn tool_list_budget_truncates_output() {
        let project = scratch_project();
        let args = serde_json::json!({
            "patterns": ["*.rs"],
            "scope": project.path().to_str().unwrap(),
            "budget": 1,
        });
        let out = tool_list(&args).expect("tool_list should succeed");
        assert!(
            out.contains("truncated ("),
            "expected truncation note: {out}"
        );
    }

    #[test]
    fn tool_list_no_match_hints_available_extensions() {
        let project = scratch_project();
        let args = serde_json::json!({
            "patterns": ["*.md"],
            "scope": project.path().to_str().unwrap(),
        });
        let out = tool_list(&args).expect("tool_list should succeed");
        assert!(
            out.contains("no matches; found extensions:") && out.contains("rs"),
            "expected no-match extension hint: {out}"
        );
    }
    #[test]
    fn list_pages_are_disjoint_and_sorted_across_nested_paths() {
        let project = tempfile::tempdir().unwrap();
        for dir in ["z", "a"] {
            std::fs::create_dir(project.path().join(dir)).unwrap();
            for name in ["second.rs", "first.rs"] {
                std::fs::write(project.path().join(dir).join(name), "fn example() {}").unwrap();
            }
        }
        let request = |offset| {
            tool_list(&serde_json::json!({
                "scope": project.path(), "patterns": ["*.rs", "**/*.rs"],
                "offset": offset, "limit": 2,
            }))
            .unwrap()
        };
        let first = request(0);
        let second = request(2);
        assert!(first.contains("a/") && !first.contains("z/"), "{first}");
        assert!(second.contains("z/") && !second.contains("a/"), "{second}");
        assert!(first.contains("1-2 of 4") && second.contains("3-4 of 4"));
        assert!(first.contains(r#""offset":2"#) && !second.contains("Next page:"));
        assert!(first.contains("2 files") && first.contains("rollups cover this page only"));
        let beyond = request(usize::MAX);
        assert!(beyond.contains("Showing files 0 of 4"));
        assert!(!beyond.contains("no matches") && !beyond.contains("Next page:"));
    }

    #[test]
    fn list_default_is_bounded_and_invalid_paging_is_rejected() {
        let project = tempfile::tempdir().unwrap();
        for i in (0..55).rev() {
            std::fs::write(project.path().join(format!("file{i:03}.rs")), "fn x() {}").unwrap();
        }
        let base = serde_json::json!({"scope":project.path(), "patterns":["*.rs"]});
        let out = tool_list(&base).unwrap();
        assert!(
            out.contains("1-50 of 55") && out.contains("file049.rs") && !out.contains("file050.rs"),
            "{out}"
        );
        for (key, value) in [
            ("limit", serde_json::json!(0)),
            ("offset", serde_json::json!(-1)),
            ("offset", serde_json::json!(1.5)),
            ("limit", serde_json::json!("2")),
            ("offset", serde_json::Value::Null),
        ] {
            let mut args = base.clone();
            args[key] = value;
            assert!(tool_list(&args).is_err(), "accepted {args}");
        }
        let mut args = base;
        args["budget"] = serde_json::json!(150);
        let out = tool_list(&args).unwrap();
        assert!(
            out.contains("truncated") && !out.contains("Next page:"),
            "{out}"
        );
        assert!(out.contains("retry the same offset"), "{out}");
    }
}
