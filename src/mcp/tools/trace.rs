use crate::cache::OutlineCache;
use serde_json::Value;
pub(in crate::mcp) fn tool_trace(args: &Value, cache: &OutlineCache) -> Result<String, String> {
    let budget = args
        .get("budget")
        .map(|v| v.as_u64().ok_or("budget must be a nonnegative integer"))
        .transpose()?;
    let result = (|| {
        let from = args
            .get("from")
            .and_then(Value::as_str)
            .ok_or("from is required and must be a string")?;
        let to = args
            .get("to")
            .map(|v| v.as_str().ok_or("to must be a string"))
            .transpose()?;
        let direction = args
            .get("direction")
            .map(|v| v.as_str().ok_or("direction must be a string"))
            .transpose()?
            .unwrap_or("callees");
        let depth = args
            .get("depth")
            .map(|v| {
                v.as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or("depth must be an integer")
            })
            .transpose()?
            .unwrap_or(2);
        let root = args
            .get("root")
            .map(|v| v.as_str().ok_or("root must be a string"))
            .transpose()?
            .map(std::path::Path::new);
        let (scope, warning) = super::resolve_scope(args, root, crate::FileScopeHint::Directory)?;
        if let Some(warning) = warning {
            return Err(warning);
        }
        crate::search::trace::run(
            &scope,
            crate::search::trace::Request {
                from,
                to,
                direction,
                depth,
                budget,
            },
            cache,
        )
    })();
    result.map_err(|error| super::apply_budget(&error, budget))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_mcp_arguments_and_matches_cli_service() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn start() { end(); }\nfn end() {}\n",
        )
        .unwrap();
        let args = serde_json::json!({"root":dir.path(),"from":"start","to":"end"});
        assert_eq!(
            tool_trace(&args, &OutlineCache::new()).unwrap(),
            crate::mcp::trace_for_cli(&args).unwrap()
        );
        for extra in [
            serde_json::json!({"depth":0}),
            serde_json::json!({"depth":1.5}),
            serde_json::json!({"direction":false}),
            serde_json::json!({"budget":-1}),
            serde_json::json!({"to":false}),
            serde_json::json!({"from":""}),
        ] {
            let mut args = args.clone();
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(tool_trace(&args, &OutlineCache::new()).is_err(), "{args}");
        }
        assert!(tool_trace(
            &serde_json::json!({"from":"start","scope":"relative"}),
            &OutlineCache::new()
        )
        .unwrap_err()
        .contains("root"));
    }
}
