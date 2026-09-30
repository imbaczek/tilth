use std::path::PathBuf;
use std::sync::Arc;

use serde_json::Value;

use crate::index::bloom::BloomFilterCache;

use super::resolve_scope;

pub(in crate::mcp) fn tool_deps(
    args: &Value,
    bloom: &Arc<BloomFilterCache>,
) -> Result<String, String> {
    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("missing required parameter: path")?;
    let root = args
        .get("root")
        .and_then(|v| v.as_str())
        .map(std::path::Path::new);
    let path = super::resolve_read_path(&PathBuf::from(path_str), root)?;
    let (scope, scope_warning) = resolve_scope(args, root)?;
    let budget = args
        .get("budget")
        .and_then(serde_json::Value::as_u64)
        .map(|b| b as usize);

    let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
    let deps_result = crate::search::deps::analyze_deps_with_options(&path, &scope, bloom, full)
        .map_err(|e| e.to_string())?;
    let mut output = scope_warning.unwrap_or_default();
    output.push_str(&crate::search::deps::format_deps_with_options(
        &deps_result,
        &scope,
        budget,
        full,
    ));
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fmt::Write as _;

    #[test]
    fn full_argument_expands_local_dependency_preview() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target.ts");
        let mut source = String::new();
        for i in 0..12 {
            std::fs::write(
                root.path().join(format!("dep{i:02}.ts")),
                "export const value = 1;\n",
            )
            .unwrap();
            writeln!(source, "import {{value as v{i}}} from './dep{i:02}';").unwrap();
        }
        std::fs::write(&target, source).unwrap();
        let preview = tool_deps(
            &serde_json::json!({"path": target, "scope": root.path()}),
            &bloom(),
        )
        .unwrap();
        let full = tool_deps(
            &serde_json::json!({"path": target, "scope": root.path(), "full": true}),
            &bloom(),
        )
        .unwrap();
        assert!(preview.contains("12 local"));
        assert!(!preview.contains("dep11.ts"));
        assert!(full.contains("dep11.ts"));
    }

    fn bloom() -> Arc<BloomFilterCache> {
        Arc::new(BloomFilterCache::new())
    }

    #[test]
    fn relative_path_no_root_errors() {
        // WHY: tilth_deps resolves its `path` arg through resolve_read_path. A
        // relative path with no absolute root silently resolved against the
        // frozen server cwd before this spec. The `?` on the path resolution must
        // propagate the refusal, naming the path and the root escape hatch.
        let args = serde_json::json!({ "path": "src/foo.rs" });
        let err = tool_deps(&args, &bloom()).unwrap_err();
        assert!(
            err.contains("src/foo.rs") && err.contains("root"),
            "relative deps path without root must refuse: {err}"
        );
    }

    #[test]
    fn absolute_path_omitted_scope_no_root_defaults_to_cwd() {
        // WHY: the require-root discipline fires ONLY when a caller EXPLICITLY
        // passes a relative path/scope without an absolute root. `scope` is
        // never required by tilth_deps — an absolute `path` with an omitted
        // `scope` must resolve scope to the server's default cwd (exactly as on
        // main), not refuse. This inverts the PR's original (too strict)
        // assertion, which broke the default flow (path-only tilth_deps calls).
        let tmp = tempfile::tempdir().unwrap();
        let abs = tmp.path().join("foo.rs");
        std::fs::write(&abs, "fn foo() {}\n").unwrap();
        let args = serde_json::json!({ "path": abs.to_str().unwrap() });
        let out = tool_deps(&args, &bloom())
            .expect("absolute path + omitted scope must default to cwd, not refuse");
        assert!(
            !out.contains("cannot be resolved"),
            "unexpected refusal: {out}"
        );
    }

    #[test]
    fn absolute_path_explicit_relative_scope_no_root_errors() {
        // An EXPLICITLY passed relative `scope` with no absolute root is
        // unresolvable (the server cannot see the caller's shell cwd) — this
        // must still refuse, even though `path` is absolute.
        let tmp = tempfile::tempdir().unwrap();
        let abs = tmp.path().join("foo.rs");
        std::fs::write(&abs, "fn foo() {}\n").unwrap();
        let args = serde_json::json!({
            "path": abs.to_str().unwrap(),
            "scope": "some/relative/dir",
        });
        let err = tool_deps(&args, &bloom()).unwrap_err();
        assert!(
            err.contains("relative scope") && err.contains("root"),
            "explicit relative scope without root must refuse: {err}"
        );
    }
}
