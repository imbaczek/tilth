use std::sync::Arc;

use serde_json::Value;

use crate::index::bloom::BloomFilterCache;
use crate::session::Session;

use super::resolve_scope;

pub(in crate::mcp) fn tool_grok(
    args: &Value,
    bloom: &Arc<BloomFilterCache>,
    session: &Session,
) -> Result<String, String> {
    let page = crate::listing::Page::optional(args)?;
    let section = args
        .get("section")
        .map(|value| {
            value
                .as_str()
                .ok_or_else(|| "section must be a string".to_string())
                .and_then(crate::search::grok::Section::parse)
        })
        .transpose()?;
    if section.is_none() && page.is_some() {
        return Err(
            "Grok offset/limit require a section: callers, callees, siblings, or tests".into(),
        );
    }
    let budget = args
        .get("budget")
        .map(|value| value.as_u64().ok_or("budget must be a nonnegative integer"))
        .transpose()?;
    let target = args
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or("missing required parameter: target")?;
    let root = args
        .get("root")
        .and_then(|v| v.as_str())
        .map(std::path::Path::new);
    let (scope, scope_warning) = resolve_scope(args, root, crate::FileScopeHint::Directory)?;
    let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
    if let Some(section) = section {
        let page = page.unwrap_or(crate::listing::Page::new(0, crate::listing::DEFAULT_LIMIT)?);
        // Include scope warnings in the final budget so navigation never survives a hidden page.
        return crate::search::grok::section_page(
            target,
            &scope,
            bloom,
            session,
            root,
            section,
            page,
            budget,
            true,
            scope_warning.as_deref().unwrap_or(""),
        )
        .map_err(|error| error.to_string());
    }
    let caps = if full {
        crate::search::grok::GrokCaps::full()
    } else {
        crate::search::grok::GrokCaps::default()
    };

    let result = crate::search::grok::grok_with_root(target, &scope, bloom, session, caps, root)
        .map_err(|e| e.to_string())?;
    let mut output = scope_warning.unwrap_or_default();
    output.push_str(&crate::search::grok::format_grok(&result, &scope));
    Ok(match budget {
        Some(budget) => crate::budget::apply(&output, budget),
        None => output,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_relative_target_and_nested_scope_use_the_supplied_checkout() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src/nested")).unwrap();
        std::fs::write(
            root.path().join("src/nested/target.rs"),
            "fn target() {}\nfn inside() { target(); }\n",
        )
        .unwrap();
        std::fs::write(
            root.path().join("outside.rs"),
            "fn outside() { target(); }\n",
        )
        .unwrap();
        let bloom = Arc::new(BloomFilterCache::new());
        for target in ["src/nested/target.rs:1", "target.rs:1"] {
            let args =
                serde_json::json!({"target": target, "root": root.path(), "scope": "src/nested"});
            let output = tool_grok(&args, &bloom, &Session::default()).unwrap();
            assert!(output.starts_with("# grok: target ["), "{output}");
            assert!(
                output.contains("inside") && !output.contains("outside"),
                "{output}"
            );
        }
    }

    /// #195: a file passed as `scope` used to fall back to the whole root.
    /// `tilth_grok` has no glob parameter, so the hint names only the directory.
    #[test]
    fn file_scope_with_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/x.py"), "def needle():\n    pass\n").unwrap();
        let args = serde_json::json!({
            "target": "needle",
            "scope": "src/x.py",
            "root": tmp.path().to_str().unwrap(),
        });
        let bloom = Arc::new(BloomFilterCache::new());
        let err = tool_grok(&args, &bloom, &Session::new())
            .expect_err("a file scope must be refused, not widened to root");
        assert!(
            err.contains("scope \"src/x.py\" is a file")
                && err.contains("scope to its directory (\"src\")"),
            "refusal must name the file and its directory: {err}"
        );
        assert!(
            !err.contains("glob"),
            "tilth_grok has no glob parameter to suggest: {err}"
        );
    }
}
