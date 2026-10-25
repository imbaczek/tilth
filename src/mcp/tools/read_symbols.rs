//! Symbol-addressed reads shared by CLI and MCP.
use crate::lang::declarations::exact_body;
use crate::{cache::OutlineCache, session::Session, types::Lang};
use serde_json::Value;
use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::Path;

pub(super) fn read(
    args: &Value,
    cache: &OutlineCache,
    session: &Session,
    edit: bool,
) -> Result<String, String> {
    for key in ["section", "sections", "slices", "offset", "limit"] {
        if args.get(key).is_some() {
            return Err(format!("symbols cannot be combined with {key}"));
        }
    }
    let mode = args.get("mode").and_then(Value::as_str).unwrap_or("auto");
    if !matches!(mode, "auto" | "full" | "signature") {
        return Err("symbol reads support auto, full, or signature mode".into());
    }
    if edit && mode == "signature" {
        return Err("symbol signature mode cannot be combined with edit mode; use auto/full for complete physical hash lines".into());
    }
    let symbols = args["symbols"]
        .as_array()
        .ok_or("symbols must be an array of strings")?;
    if symbols.is_empty() || symbols.len() > 20 {
        return Err("symbols requires 1-20 names".into());
    }
    let symbols = symbols
        .iter()
        .map(|v| {
            v.as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or("symbols must contain nonempty strings")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if args.get("path").is_some() && args.get("paths").is_some() {
        return Err("provide path or paths, not both".into());
    }
    let paths = if let Some(paths) = args.get("paths") {
        paths
            .as_array()
            .ok_or("paths must be an array")?
            .iter()
            .map(|p| p.as_str().ok_or("paths must contain strings"))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        vec![args
            .get("path")
            .and_then(Value::as_str)
            .ok_or("symbols requires path or paths")?]
    };
    if paths.is_empty() || paths.len() > 20 {
        return Err("symbol reads require 1-20 files".into());
    }
    let root = args.get("root").and_then(Value::as_str).map(Path::new);
    let budget = args.get("budget").and_then(Value::as_u64);
    let mut declarations = Vec::new();
    let mut notices = Vec::new();
    let mut seen_paths = HashSet::new();
    for path in paths {
        let path = super::resolve_read_path(Path::new(path), root)?;
        let identity = path.canonicalize().unwrap_or_else(|_| path.clone());
        if !seen_paths.insert(identity) {
            continue;
        }
        session.record_read(&path);
        match cache.get_or_parse(&path) {
            Some(parsed)=> {
                if parsed.tree.root_node().has_error() { notices.push(format!("{}: parse errors; symbol results may be incomplete",path.display())); }
                declarations.extend(crate::lang::declarations::collect(&path,&parsed));
            }
            None=>notices.push(format!("{}: cannot index symbols (missing/unreadable file, unsupported language, or file exceeds 500 KB)",path.display())),
        }
    }
    let mut summary = String::from("# Symbol reads\n");
    for notice in notices {
        let _ = writeln!(summary, "> {notice}");
    }
    let mut bodies = Vec::new();
    let mut emitted = HashSet::new();
    for symbol in symbols {
        let (name, line) = symbol
            .rsplit_once('@')
            .and_then(|(name, line)| line.parse::<u32>().ok().map(|line| (name, Some(line))))
            .unwrap_or((symbol, None));
        let matches: Vec<_> = declarations
            .iter()
            .filter(|d| {
                let eq = |a: &str, b: &str| {
                    if d.parsed.lang == Lang::CommonLisp {
                        crate::lang::commonlisp::names_equal(a, b)
                    } else {
                        a == b
                    }
                };
                (eq(name, &d.entry.name)
                    || eq(name, &d.qualified)
                    || (d.parsed.lang == Lang::Go
                        && d.entry.name.split(", ").any(|alias| eq(name, alias))))
                    && line.is_none_or(|line| d.entry.start_line == line)
            })
            .collect();
        match matches.as_slice() {
            [] => {
                let _ = writeln!(summary, "- {symbol}: not found in indexed declarations");
            }
            [d] => {
                let _ = writeln!(
                    summary,
                    "- {symbol}: {}:{}-{}",
                    d.path.display(),
                    d.entry.start_line,
                    d.entry.end_line
                );
                let key = (
                    d.path.clone(),
                    d.entry.start_line,
                    d.entry.end_line,
                    d.qualified.clone(),
                    d.span,
                );
                if !emitted.insert(key) {
                    continue;
                }
                let body = if mode == "signature" {
                    crate::lang::declarations::signature_text(d)
                        .unwrap_or_else(|| format!("{} [signature unavailable]", d.entry.name))
                } else if !edit {
                    exact_body(d).unwrap_or_else(|| {
                        format!("[exact source unavailable for {}]", d.qualified)
                    })
                } else {
                    crate::read::read_file_with_budget(
                        &d.path,
                        Some(&format!("{}-{}", d.entry.start_line, d.entry.end_line)),
                        true,
                        cache,
                        edit,
                        budget,
                    )
                    .map_err(|e| e.to_string())?
                };
                bodies.push(format!(
                    "## {} — {}:{}\n\n{body}",
                    d.qualified,
                    d.path.display(),
                    d.entry.start_line
                ));
            }
            _ => {
                let _=writeln!(summary,"- {symbol}: ambiguous ({} definitions); use a qualified name, narrower files, or name@line",matches.len());
                for d in matches {
                    let _ = writeln!(
                        summary,
                        "  {}@{} — {}",
                        d.qualified,
                        d.entry.start_line,
                        d.path.display()
                    );
                }
            }
        }
    }
    Ok(crate::listing::finish(
        &format!("{summary}\n{}", bodies.join("\n\n")),
        budget,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mcp_symbol_reads_validate_inputs_preserve_statuses_and_hash_anchors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn alpha() {\n let value=1;\n}\n").unwrap();
        let run = |extra: Value, edit| {
            let mut args = serde_json::json!({"root":dir.path(),"path":"a.rs"});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            super::super::read::read_with_navigation(
                &args,
                &OutlineCache::new(),
                &Session::new(),
                edit,
                true,
            )
        };
        let out = run(
            serde_json::json!({"symbols":["alpha","missing","alpha"]}),
            true,
        )
        .unwrap();
        assert!(
            out.contains("1:") && out.contains("missing: not found"),
            "{out}"
        );
        assert_eq!(out.matches("let value=1;").count(), 1, "{out}");
        for extra in [
            serde_json::json!({"symbols":[]}),
            serde_json::json!({"symbols":[false]}),
            serde_json::json!({"symbols":[""]}),
            serde_json::json!({"symbols":["alpha"],"paths":["a.rs"]}),
            serde_json::json!({"symbols":["alpha"],"sections":["1-2"]}),
        ] {
            assert!(run(extra.clone(), false).is_err(), "{extra}");
        }
        let out = run(
            serde_json::json!({"symbols":["alpha"],"path":"missing.rs"}),
            false,
        )
        .unwrap();
        assert!(out.contains("cannot index symbols"), "{out}");
    }
}
