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
    let target = args
        .get("target")
        .and_then(|v| v.as_str())
        .ok_or("missing required parameter: target")?;
    let root = args
        .get("root")
        .and_then(|v| v.as_str())
        .map(std::path::Path::new);
    let (scope, scope_warning) = resolve_scope(args, root)?;
    let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
    let caps = if full {
        crate::search::grok::GrokCaps::full()
    } else {
        crate::search::grok::GrokCaps::default()
    };

    let result = crate::search::grok::grok_with_root(target, &scope, bloom, session, caps, root)
        .map_err(|e| e.to_string())?;
    let mut output = scope_warning.unwrap_or_default();
    output.push_str(&crate::search::grok::format_grok(&result, &scope));
    Ok(output)
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
}
