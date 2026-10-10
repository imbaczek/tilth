use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::cache::OutlineCache;
use crate::error::TilthError;
use crate::lang::detect_file_type;
use crate::lang::outline::get_outline_entries;
use crate::session::Session;
use crate::types::{estimate_tokens, FileType, OutlineEntry, ViewMode};

use super::apply_budget;

pub(in crate::mcp) fn tool_read(
    args: &Value,
    cache: &OutlineCache,
    session: &Session,
    edit_mode: bool,
) -> Result<String, String> {
    read_with_navigation(args, cache, session, edit_mode, true)
}

pub(in crate::mcp) fn read_with_navigation(
    args: &Value,
    cache: &OutlineCache,
    session: &Session,
    edit_mode: bool,
    mcp_navigation: bool,
) -> Result<String, String> {
    let budget = args.get("budget").and_then(serde_json::Value::as_u64);
    // Extract optional root for anchoring relative paths. A relative path
    // without an absolute `root` is unresolvable (the server cannot see the
    // caller's shell cwd) — propagate that refusal at each path-resolution site.
    let root = args
        .get("root")
        .and_then(|v| v.as_str())
        .map(std::path::Path::new);
    let full_flag = args
        .get("full")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let mode_str = args.get("mode").and_then(|v| v.as_str()).unwrap_or("auto");
    if !matches!(mode_str, "auto" | "full" | "signature" | "stripped") {
        return Err(format!(
            "unknown read mode: {mode_str}. Use: auto, full, signature, stripped"
        ));
    }
    // Precedence when full:true is combined with a reshaping mode: signature and
    // stripped win over full. The dispatch below checks force_signature/force_stripped
    // before falling back to force_full, so `full:true` + `mode:signature` yields a
    // signature view, not a full dump. Pinned by tool_read_signature_beats_full_flag.
    let force_full = full_flag || mode_str == "full";
    let force_signature = mode_str == "signature";
    let force_stripped = mode_str == "stripped";

    let paginated = args.get("offset").is_some() || args.get("limit").is_some();
    let page = crate::listing::Page::from_args(args)?;
    if paginated && args.get("paths").is_some() {
        return Err(
            "offset and limit require a single directory or Markdown path, not paths".into(),
        );
    }

    // Multi-file batch read (capped at 20 to bound I/O)
    if let Some(paths_arr) = args.get("paths").and_then(|v| v.as_array()) {
        if paths_arr.len() > 20 {
            return Err(format!(
                "batch read limited to 20 files (got {})",
                paths_arr.len()
            ));
        }

        // Aggregate deadline for batch reads: 60s default, override with TILTH_BATCH_TIMEOUT
        // Note: deadline is checked between files, so a single massive file could still
        // exceed it. The per-request timeout (handle_tool_call) catches that case.
        let batch_timeout = std::env::var("TILTH_BATCH_TIMEOUT")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(batch_timeout);

        let mut results = Vec::with_capacity(paths_arr.len());
        for (i, p) in paths_arr.iter().enumerate() {
            // Check deadline before each file
            if std::time::Instant::now() > deadline {
                results.push(format!(
                    "# batch read stopped — deadline exceeded after {}/{} files. \
                     Reduce batch size or set TILTH_BATCH_TIMEOUT=<seconds>.",
                    i,
                    paths_arr.len()
                ));
                break;
            }

            let path_str = p.as_str().ok_or("paths must be an array of strings")?;
            let path = super::resolve_read_path(&PathBuf::from(path_str), root)?;
            session.record_read(&path);
            let read = if path.is_dir() {
                if force_signature || force_stripped {
                    Err(TilthError::InvalidQuery {
                        query: path.display().to_string(),
                        reason: "directory listings do not support signature/stripped modes".into(),
                    })
                } else {
                    crate::read::list_directory(&path, page, mcp_navigation)
                }
            } else if force_signature {
                read_signature_file(&path, cache).map(|(body, _)| body)
            } else if force_stripped {
                read_stripped_file(&path, cache).map(|(body, _, _)| body)
            } else {
                crate::read::read_file_with_budget(
                    &path, None, force_full, cache, edit_mode, budget,
                )
            };
            match read {
                Ok(output) => results.push(output),
                Err(e) => results.push(format!("# {} — error: {}", path.display(), e)),
            }
        }
        let combined = results.join("\n\n");
        return Ok(apply_budget(&combined, budget));
    }

    // Single file read
    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or("missing required parameter: path (or use paths for batch read)")?;
    let path = super::resolve_read_path(&PathBuf::from(path_str), root)?;
    let section = args.get("section").and_then(|v| v.as_str());
    let sections_arr = args.get("sections").and_then(|v| v.as_array());

    if section.is_some() && sections_arr.is_some() {
        return Err("provide either section (single) or sections (array), not both".into());
    }

    // signature/stripped reshape the whole file; a section selection has no
    // meaning there. Error rather than silently dropping the mode.
    if (force_signature || force_stripped) && (section.is_some() || sections_arr.is_some()) {
        return Err(format!(
            "mode={mode_str} cannot be combined with section/sections — \
             {mode_str} reshapes the whole file. Drop section/sections or pick mode=auto/full."
        ));
    }

    if path.is_dir() {
        if section.is_some() || sections_arr.is_some() || force_signature || force_stripped {
            return Err(
                "directory listings do not support section/sections or signature/stripped modes"
                    .into(),
            );
        }
        session.record_read(&path);
        let output =
            crate::read::list_directory(&path, page, mcp_navigation).map_err(|e| e.to_string())?;
        return Ok(apply_budget(&output, budget));
    }
    if paginated {
        if section.is_some()
            || sections_arr.is_some()
            || force_full
            || force_signature
            || force_stripped
        {
            return Err(
                "Markdown outline pagination requires auto mode without full, section, or sections"
                    .into(),
            );
        }
        session.record_read(&path);
        let output = crate::read::read_markdown_outline_page(&path, page, mcp_navigation)
            .map_err(|e| e.to_string())?;
        return Ok(apply_budget(&output, budget));
    }

    // Multi-section path: bypass smart view + related-file hints (those only
    // apply to whole-file reads).
    if let Some(arr) = sections_arr {
        let ranges: Vec<&str> = arr
            .iter()
            .map(|v| v.as_str().ok_or("sections must be an array of strings"))
            .collect::<Result<Vec<_>, _>>()?;
        if ranges.is_empty() {
            return Err("sections must contain at least one range".into());
        }
        if ranges.len() > 20 {
            return Err(format!(
                "sections limited to 20 per call (got {})",
                ranges.len()
            ));
        }
        session.record_read(&path);
        let output = match budget {
            Some(b) => crate::read::read_ranges_with_budget(&path, &ranges, edit_mode, b)
                .map_err(|e| e.to_string())?,
            None => {
                crate::read::read_ranges(&path, &ranges, edit_mode).map_err(|e| e.to_string())?
            }
        };
        return Ok(output);
    }

    session.record_read(&path);

    // Only genuine AUTO reads are credited with savings — where tilth transparently
    // returns an outline instead of the full file a naive `cat` would dump. An
    // explicit section/signature/stripped/full read asked for a specific view, so
    // crediting "saved vs the whole file" would overstate.
    let auto_read = section.is_none() && !force_signature && !force_stripped && !force_full;
    // Capture the file size up front, close to `read_file`'s own read. Statting
    // after the read+format pipeline would let an external append in that window
    // inflate the baseline and overstate savings; statting here means a concurrent
    // grow can only *understate* it, keeping the number a conservative lower bound.
    let savings_baseline = if auto_read {
        std::fs::metadata(&path).map(|m| m.len()).ok()
    } else {
        None
    };

    let mut output = if section.is_none() && force_signature {
        read_signature_file(&path, cache)
            .map(|(body, _)| body)
            .map_err(|e| e.to_string())?
    } else if section.is_none() && force_stripped {
        read_stripped_file(&path, cache)
            .map(|(body, _, _)| body)
            .map_err(|e| e.to_string())?
    } else {
        crate::read::read_file_with_budget(&path, section, force_full, cache, edit_mode, budget)
            .map_err(|e| e.to_string())?
    };

    // Append related-file hint for outlined code files (not section reads, not batch).
    if section.is_none() && crate::read::would_outline(&path) {
        let related = crate::read::imports::resolve_related_files(&path);
        if !related.is_empty() {
            output.push_str("\n\n> Related: ");
            for (i, p) in related.iter().enumerate() {
                if i > 0 {
                    output.push_str(", ");
                }
                let _ = write!(output, "{}", p.display());
            }
        }
    }

    let response = apply_budget(&output, budget);
    // Credit savings vs the full file using the baseline captured before the read.
    if let Some(file_byte_len) = savings_baseline {
        session.record_savings(
            estimate_tokens(file_byte_len),
            estimate_tokens(response.len() as u64),
        );
    }
    Ok(response)
}

// `cache` is intentionally unwired on the tree-sitter path: OutlineCache stores
// formatted outline strings, not Vec<OutlineEntry>, so get_outline_entries below
// re-parses every call. Wiring a structured cache is a separate change. The param
// is still used by the non-code fallback (read_file), so it keeps its real name.
fn read_signature_file(path: &Path, cache: &OutlineCache) -> Result<(String, u32), TilthError> {
    let content = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => TilthError::NotFound {
            path: path.to_path_buf(),
            suggestion: None,
        },
        std::io::ErrorKind::PermissionDenied => TilthError::PermissionDenied {
            path: path.to_path_buf(),
        },
        _ => TilthError::IoError {
            path: path.to_path_buf(),
            source: e,
        },
    })?;
    let meta = std::fs::metadata(path).map_err(|e| TilthError::IoError {
        path: path.to_path_buf(),
        source: e,
    })?;
    let line_count = u32::try_from(content.lines().count()).unwrap_or(u32::MAX);

    let FileType::Code(lang) = detect_file_type(path) else {
        let body = crate::read::read_file(path, None, false, cache, false)?;
        return Ok((body, line_count));
    };

    // Build the signature header only on the code path — the non-code fallback
    // above returns the normal read and never uses it.
    let header = crate::format::file_header(path, meta.len(), line_count, ViewMode::Signature);
    let entries = get_outline_entries(&content, lang);
    let lines: Vec<&str> = content.lines().collect();
    let mut body = String::new();
    render_signature_entries(&entries, &lines, &mut body);
    if body.is_empty() {
        body = crate::format::hashlines(&content, 1);
    }
    Ok((format!("{header}\n\n{}", body.trim_end()), line_count))
}

fn render_signature_entries(entries: &[OutlineEntry], lines: &[&str], out: &mut String) {
    for entry in entries {
        let start_idx = entry.start_line.saturating_sub(1) as usize;
        let end = entry.signature_end;
        let end_idx = end
            .map_or(start_idx, |position| {
                position.line.saturating_sub(1) as usize
            })
            .max(start_idx)
            .min(lines.len().saturating_sub(1));

        for idx in start_idx..=end_idx {
            let Some(line) = lines.get(idx) else {
                continue;
            };
            let visible = match end.filter(|_| idx == end_idx) {
                Some(position) if position.column < line.len() => {
                    line.get(..position.column).unwrap_or(line).trim_end()
                }
                _ => line,
            };
            let hash = crate::format::line_hash(line.as_bytes());
            let _ = writeln!(out, "{}:{hash:03x}|{visible}", idx + 1);
        }

        render_signature_entries(&entry.children, lines, out);
    }
}

// `cache` is intentionally unwired on the tree-sitter path: OutlineCache stores
// formatted outline strings, not Vec<OutlineEntry>, so strip_noise re-parses every
// call. Wiring a structured cache is a separate change. The param is still used by
// the non-code fallback (read_file), so it keeps its real name.
fn read_stripped_file(path: &Path, cache: &OutlineCache) -> Result<(String, u32, u32), TilthError> {
    let content = std::fs::read_to_string(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => TilthError::NotFound {
            path: path.to_path_buf(),
            suggestion: None,
        },
        std::io::ErrorKind::PermissionDenied => TilthError::PermissionDenied {
            path: path.to_path_buf(),
        },
        _ => TilthError::IoError {
            path: path.to_path_buf(),
            source: e,
        },
    })?;
    let meta = std::fs::metadata(path).map_err(|e| TilthError::IoError {
        path: path.to_path_buf(),
        source: e,
    })?;
    let total_lines = u32::try_from(content.lines().count()).unwrap_or(u32::MAX);

    if !matches!(detect_file_type(path), FileType::Code(_)) {
        let body = crate::read::read_file(path, None, false, cache, false)?;
        return Ok((body, total_lines, 0));
    }

    let skip_lines = crate::search::strip::strip_noise(&content, path, Some((1, total_lines)));
    let width = total_lines.max(1).to_string().len();
    let mut body = String::with_capacity(content.len());
    let mut kept: u32 = 0;
    for (i, line) in content.lines().enumerate() {
        let line_num = u32::try_from(i + 1).unwrap_or(u32::MAX);
        if skip_lines.contains(&line_num) {
            continue;
        }
        let _ = writeln!(body, "{line_num:>width$}  {line}");
        kept += 1;
    }

    let stripped = total_lines.saturating_sub(kept);
    let header = crate::format::file_header(path, meta.len(), total_lines, ViewMode::Stripped);
    let note = format!(
        "// stripped {stripped} of {total_lines} lines (plain comments, debug logs, blank collapse) — non-editable view"
    );
    Ok((
        format!("{header}\n{note}\n\n{}", body.trim_end()),
        total_lines,
        stripped,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::tools::tool_definitions;

    #[test]
    fn tool_read_signature_mode_emits_hash_prefixed_signatures() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signature.rs");
        std::fs::write(
            &path,
            "fn signature_target() {\n    let body_marker = 42;\n}\n",
        )
        .unwrap();
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "mode": "signature",
        });
        let cache = OutlineCache::new();
        let session = Session::new();

        let out = tool_read(&args, &cache, &session, false).expect("signature read");

        assert!(
            out.contains("[signature]"),
            "signature header missing: {out}"
        );
        assert!(
            out.lines()
                .any(|l| l.starts_with("1:") && l.contains("fn signature_target")),
            "hash-prefixed signature line missing: {out}"
        );
        assert!(
            !out.contains("body_marker"),
            "signature mode should omit function body: {out}"
        );
    }

    #[test]
    fn tool_read_stripped_mode_drops_comments_and_keeps_doc_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stripped.rs");
        std::fs::write(
            &path,
            "/// keep docs\nfn keep() {\n    // drop plain comment\n    dbg!(1);\n    println!(\"keep\");\n}\n",
        )
        .unwrap();
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "mode": "stripped",
        });
        let cache = OutlineCache::new();
        let session = Session::new();

        let out = tool_read(&args, &cache, &session, true).expect("stripped read");

        assert!(out.contains("[stripped]"), "stripped header missing: {out}");
        assert!(out.contains("/// keep docs"), "doc comment missing: {out}");
        assert!(out.contains("println!"), "kept code missing: {out}");
        assert!(
            !out.contains("drop plain comment"),
            "plain comment should be stripped: {out}"
        );
        assert!(!out.contains("dbg!"), "debug log should be stripped: {out}");
        assert!(
            !out.lines().any(|l| l.contains(':') && l.contains('|')),
            "stripped output must not expose hash anchors: {out}"
        );
    }

    #[test]
    fn tool_read_unknown_mode_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("any.rs");
        std::fs::write(&path, "fn f() {}\n").unwrap();
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "mode": "outline",
        });
        let cache = OutlineCache::new();
        let session = Session::new();

        let err = tool_read(&args, &cache, &session, false).expect_err("unknown mode must error");
        assert!(
            err.starts_with("unknown read mode: outline"),
            "error must name the bad mode: {err}"
        );
        assert!(
            err.contains("auto, full, signature, stripped"),
            "error must list valid modes: {err}"
        );
    }

    #[test]
    fn tool_read_signature_mode_non_code_falls_back_to_normal_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "alpha line\nbeta line\ngamma line\n").unwrap();
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "mode": "signature",
        });
        let cache = OutlineCache::new();
        let session = Session::new();

        let out = tool_read(&args, &cache, &session, false).expect("signature read on text");

        // Non-code falls back to the normal read: no signature header, full content.
        assert!(
            !out.contains("[signature]"),
            "non-code must not emit signature header: {out}"
        );
        assert!(out.contains("alpha line"), "content must survive: {out}");
        assert!(out.contains("gamma line"), "content must survive: {out}");
    }

    #[test]
    fn tool_read_stripped_mode_non_code_falls_back_to_normal_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        std::fs::write(&path, "alpha line\nbeta line\ngamma line\n").unwrap();
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "mode": "stripped",
        });
        let cache = OutlineCache::new();
        let session = Session::new();

        let out = tool_read(&args, &cache, &session, false).expect("stripped read on text");

        assert!(
            !out.contains("[stripped]"),
            "non-code must not emit stripped header: {out}"
        );
        assert!(out.contains("alpha line"), "content must survive: {out}");
        assert!(out.contains("gamma line"), "content must survive: {out}");
    }

    #[test]
    fn tool_read_full_flag_is_legacy_alias_for_mode_full() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aliased.rs");
        // Body must exceed TOKEN_THRESHOLD (6k tokens ≈ 24KB) AND compress well
        // so `auto` returns an outline rather than full content — making the
        // alias equivalence observable, not a trivial small-file match where
        // auto and full coincide. Functions have large bodies so the outline
        // (signatures only) is a small fraction of the full-file token cost,
        // ensuring OGATE does not fire and auto != full.
        let mut src = String::from("// header comment\n");
        for i in 0..80 {
            let _ = writeln!(src, "fn f_{i}() {{");
            // Large body: many statements so the outline compresses well
            for j in 0..30 {
                let _ = writeln!(src, "    let local_var_{j}_in_fn_{i}: u64 = {j} + {i};");
            }
            src.push_str("}\n");
        }
        std::fs::write(&path, &src).unwrap();
        let cache = OutlineCache::new();
        let session = Session::new();

        let via_flag = tool_read(
            &serde_json::json!({ "path": path.to_str().unwrap(), "full": true }),
            &cache,
            &session,
            false,
        )
        .expect("full:true read");
        let via_mode = tool_read(
            &serde_json::json!({ "path": path.to_str().unwrap(), "mode": "full" }),
            &cache,
            &session,
            false,
        )
        .expect("mode:full read");
        let via_auto = tool_read(
            &serde_json::json!({ "path": path.to_str().unwrap() }),
            &cache,
            &session,
            false,
        )
        .expect("auto read");

        assert_eq!(
            via_flag, via_mode,
            "full:true must be a byte-identical alias for mode='full'"
        );
        assert!(
            via_flag.contains("[full]"),
            "alias must force full view: {}",
            &via_flag[..via_flag.len().min(80)]
        );
        assert_ne!(
            via_auto, via_flag,
            "auto must outline a large file, differing from forced full"
        );
    }

    #[test]
    fn tool_read_signature_beats_full_flag() {
        // full:true + mode:signature must resolve to a signature view, not a full
        // dump. If a future change flips the dispatch order this test fails loudly.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("precedence.rs");
        std::fs::write(
            &path,
            "fn precedence_target() {\n    let body_marker = 99;\n}\n",
        )
        .unwrap();
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "mode": "signature",
            "full": true,
        });
        let cache = OutlineCache::new();
        let session = Session::new();

        let out = tool_read(&args, &cache, &session, false).expect("signature+full read");

        assert!(
            out.contains("[signature]"),
            "signature must win over full:true (header): {out}"
        );
        assert!(
            !out.contains("body_marker"),
            "signature must win over full:true (body omitted): {out}"
        );
    }

    #[test]
    fn tool_read_signature_mode_rejects_section() {
        // Combining a reshaping mode with section must error, not silently drop the
        // mode (which would return a section slice and ignore signature entirely).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("conflict.rs");
        std::fs::write(&path, "fn a() {}\nfn b() {}\n").unwrap();
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "mode": "signature",
            "section": "1-1",
        });
        let cache = OutlineCache::new();
        let session = Session::new();

        let err =
            tool_read(&args, &cache, &session, false).expect_err("signature + section must error");
        assert!(
            err.contains("signature") && err.contains("section"),
            "error must name the conflict: {err}"
        );
    }

    #[test]
    fn tilth_read_schema_lists_stripped_mode() {
        let tools = tool_definitions(false);
        let read = tools
            .iter()
            .find(|tool| tool.get("name").and_then(Value::as_str) == Some("tilth_read"))
            .expect("tilth_read definition");
        let modes = read
            .pointer("/inputSchema/properties/mode/enum")
            .and_then(Value::as_array)
            .expect("mode enum");

        assert!(
            modes.iter().any(|v| v.as_str() == Some("stripped")),
            "mode enum must advertise stripped: {read}"
        );
    }

    #[test]
    fn root_param_anchors_relative_path_under_root() {
        // Guards #78: tilth_read with a relative path + root must read from
        // <root>/<path>, not from <cwd>/<path>. Prevents worktree agents from
        // silently reading the wrong checkout.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("hello.rs"), "fn hello() {}").unwrap();

        let cache = OutlineCache::new();
        let session = Session::new();
        let args = serde_json::json!({
            "paths": ["hello.rs"],
            "mode": "full",
            "root": root.to_str().unwrap()
        });
        let result = tool_read(&args, &cache, &session, false).unwrap();
        assert!(
            result.contains("fn hello()"),
            "expected file content via root-anchored path, got: {result}"
        );
    }

    #[test]
    fn root_param_absolute_path_unaffected() {
        // Absolute paths must be used as-is even when root is set.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let abs_file = root.join("abs.rs");
        std::fs::write(&abs_file, "fn abs() {}").unwrap();

        let unrelated_root = tempfile::tempdir().unwrap();
        let cache = OutlineCache::new();
        let session = Session::new();
        let args = serde_json::json!({
            "paths": [abs_file.to_str().unwrap()],
            "mode": "full",
            "root": unrelated_root.path().to_str().unwrap()
        });
        let result = tool_read(&args, &cache, &session, false).unwrap();
        assert!(
            result.contains("fn abs()"),
            "absolute path must resolve independently of root, got: {result}"
        );
    }

    #[test]
    fn no_root_reads_absolute_path_unchanged() {
        // Omitting root must behave identically to before #78: absolute paths
        // resolve as-is regardless of whether root is set or not.
        let tmp = tempfile::tempdir().unwrap();
        let abs_file = tmp.path().join("check.rs");
        std::fs::write(&abs_file, "fn check() {}").unwrap();

        let cache = OutlineCache::new();
        let session = Session::new();
        let args = serde_json::json!({
            "paths": [abs_file.to_str().unwrap()],
            "mode": "full"
        });
        let result = tool_read(&args, &cache, &session, false).unwrap();
        assert!(
            result.contains("fn check()"),
            "no-root regression: absolute path must be readable without root, got: {result}"
        );
    }

    #[test]
    fn relative_path_no_root_errors() {
        // WHY: a relative path + no root silently resolved against the frozen
        // server cwd before this spec — the worktree bug. It must now refuse
        // with a message naming the path and the absolute-root escape hatch.
        let cache = OutlineCache::new();
        let session = Session::new();
        let args = serde_json::json!({ "paths": ["src/foo.rs"], "mode": "full" });
        let err = tool_read(&args, &cache, &session, false).unwrap_err();
        assert!(
            err.contains("src/foo.rs") && err.contains("root"),
            "relative path without root must refuse with an actionable message: {err}"
        );
    }

    // -- savings recording tests ------------------------------------------

    /// A large file with large function bodies read in auto mode (outline) must record
    /// saved > 0 and baseline > 0 on the session.
    #[test]
    fn tool_read_large_file_records_positive_savings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.rs");
        // Build a file large enough to exceed TOKEN_THRESHOLD (6 000 tokens ≈ 24 KB)
        // with functions that have substantial bodies so the outline compresses well.
        let mut src = String::from("// header\n");
        for i in 0..200 {
            let _ = writeln!(src, "fn func_{i}() {{");
            // 20 lines of body per function so outline is much smaller than full content
            for j in 0..20 {
                let _ = writeln!(src, "    let v_{i}_{j}: u64 = {j} * {i} + 42;");
            }
            src.push_str("}\n");
        }
        std::fs::write(&path, &src).unwrap();
        let file_size = std::fs::metadata(&path).unwrap().len();
        assert!(
            file_size > 24_000,
            "test file must be large enough to trigger outline: {file_size} bytes"
        );
        let cache = OutlineCache::new();
        let session = Session::new();
        let args = serde_json::json!({ "path": path.to_str().unwrap() });

        tool_read(&args, &cache, &session, false).expect("large file read");

        let (baseline, saved) = session.savings();
        assert!(
            baseline > 0,
            "baseline must be > 0 for a non-empty file: baseline={baseline}"
        );
        assert!(
            saved > 0,
            "large outlined file must record positive savings: saved={saved}, baseline={baseline}"
        );
    }

    /// A small file read in auto mode (full content) must record baseline > 0
    /// but saved == 0 (no reduction applied).
    #[test]
    fn tool_read_small_file_records_zero_savings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.rs");
        std::fs::write(&path, "fn small() {}\n").unwrap();
        let cache = OutlineCache::new();
        let session = Session::new();
        let args = serde_json::json!({ "path": path.to_str().unwrap() });

        tool_read(&args, &cache, &session, false).expect("small file read");

        let (baseline, saved) = session.savings();
        assert!(baseline > 0, "baseline must be > 0 for a non-empty file");
        assert_eq!(
            saved, 0,
            "small file returned in full must record zero savings"
        );
    }

    /// A single-section read requested an explicit range — the naive baseline is
    /// that range, not the whole file — so it must NOT record a (bogus) full-file
    /// saving. Guards against over-counting explicit sub-view reads.
    #[test]
    fn tool_read_section_records_no_savings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sectioned.rs");
        // Large file: a full-file baseline would book a big (bogus) "saving".
        let mut src = String::new();
        for i in 0..500 {
            let _ = writeln!(src, "fn f_{i}() {{ let v = {i}; }}");
        }
        std::fs::write(&path, &src).unwrap();
        let cache = OutlineCache::new();
        let session = Session::new();
        let args = serde_json::json!({ "path": path.to_str().unwrap(), "section": "1-5" });

        tool_read(&args, &cache, &session, false).expect("section read");

        let (baseline, saved) = session.savings();
        assert_eq!(
            baseline, 0,
            "section reads must not record a full-file baseline"
        );
        assert_eq!(saved, 0, "section reads must not record savings");
    }
    #[test]
    fn read_directory_pages_and_validation() {
        let project = tempfile::tempdir().unwrap();
        for i in (0..55).rev() {
            std::fs::write(project.path().join(format!("file{i:03}.txt")), "data").unwrap();
        }
        let cache = OutlineCache::new();
        let session = Session::default();
        let run = |args: Value| tool_read(&args, &cache, &session, false);
        let base = serde_json::json!({"path":project.path()});
        let first = run(base.clone()).unwrap();
        assert!(
            first.contains("1-50 of 55") && first.contains(r#""offset":50"#),
            "{first}"
        );
        assert!(first.contains("file049.txt") && !first.contains("file050.txt"));
        let second =
            run(serde_json::json!({"path":project.path(), "offset":50, "limit":5})).unwrap();
        assert!(second.contains("51-55 of 55") && second.contains("End of listing."));
        for i in 0..55 {
            let name = format!("  file{i:03}.txt ");
            assert_eq!(
                first.matches(&name).count() + second.matches(&name).count(),
                1,
                "{name}"
            );
        }
        for (key, value) in [
            ("offset", serde_json::json!(-1)),
            ("offset", serde_json::json!(0.5)),
            ("limit", serde_json::json!(0)),
            ("limit", serde_json::json!("3")),
            ("sections", serde_json::json!(["1-2"])),
            ("mode", serde_json::json!("stripped")),
        ] {
            let mut args = base.clone();
            args[key] = value;
            assert!(run(args.clone()).is_err(), "accepted {args}");
        }
        assert!(
            run(serde_json::json!({"path":project.path().join("file000.txt"),"offset":0})).is_err()
        );
        assert!(run(serde_json::json!({"paths":[project.path()],"limit":2})).is_err());
        let batch = run(serde_json::json!({"paths":[project.path()]})).unwrap();
        assert!(
            batch.contains(r#""offset":50"#) && !batch.contains("--offset"),
            "{batch}"
        );
    }
}

#[cfg(test)]
mod multiline_signature_regression_tests {
    use super::*;

    #[test]
    fn signature_view_preserves_multiline_declaration_with_valid_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multiline.rs");
        let source = "pub fn multiline_handler(\n    request: &str,\n    retries: u8,\n) -> Result<usize, &'static str> {\n    let body_only_marker = 42;\n    Ok(request.len())\n}\n";
        std::fs::write(&path, source).unwrap();

        let output = tool_read(
            &serde_json::json!({"path": path, "mode": "signature"}),
            &OutlineCache::new(),
            &Session::new(),
            false,
        )
        .unwrap();

        assert!(output.contains("[signature]"));
        for (index, line) in source.lines().take(4).enumerate() {
            let hash = crate::format::line_hash(line.as_bytes());
            let expected = format!("{}:{hash:03x}|{line}", index + 1);
            assert!(
                output.contains(&expected),
                "missing declaration line: {expected}"
            );
        }
        assert!(!output.contains("body_only_marker"));
        assert!(!output.contains("Ok(request.len())"));
    }

    #[test]
    fn signature_view_preserves_multiline_typescript_object_return_type() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multiline.ts");
        let source = "export function inspect(value: string): {\n    rendered: string;\n    count: number;\n} {\n    const body_only_marker = 42;\n    return { rendered: value, count: 1 };\n}\n";
        std::fs::write(&path, source).unwrap();

        let output = tool_read(
            &serde_json::json!({"path": path, "mode": "signature"}),
            &OutlineCache::new(),
            &Session::new(),
            false,
        )
        .unwrap();

        for (index, line) in source.lines().take(4).enumerate() {
            let hash = crate::format::line_hash(line.as_bytes());
            let expected = format!("{}:{hash:03x}|{line}", index + 1);
            assert!(
                output.contains(&expected),
                "missing TypeScript declaration line: {expected}"
            );
        }
        assert!(!output.contains("body_only_marker"));
    }

    #[test]
    fn signature_view_truncates_inline_body_after_opening_brace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inline.rs");
        let source = "pub fn inline_handler() { let inline_body_marker = 42; }\n";
        let visible_signature = "pub fn inline_handler() {";
        std::fs::write(&path, source).unwrap();

        let output = tool_read(
            &serde_json::json!({"path": path, "mode": "signature"}),
            &OutlineCache::new(),
            &Session::new(),
            false,
        )
        .unwrap();

        let source_line = source.lines().next().unwrap();
        let hash = crate::format::line_hash(source_line.as_bytes());
        let expected = format!("1:{hash:03x}|{visible_signature}");
        assert!(
            output.contains(&expected),
            "missing inline signature: {expected}"
        );
        assert!(!output.contains("inline_body_marker"));
    }
}

#[cfg(test)]
mod outline_cache_regression_tests {
    use super::*;
    use std::fs;

    fn read(path: &Path, cache: &OutlineCache, budget: Option<u64>) -> String {
        tool_read(
            &serde_json::json!({"path": path, "budget": budget}),
            cache,
            &Session::new(),
            false,
        )
        .unwrap()
    }
    fn large_outline_fixture(root: &Path) -> std::path::PathBuf {
        let path = root.join("sample.rs");
        let mut source = String::new();
        for i in 0..150 {
            writeln!(source, "pub fn sample_{i:03}() {{").unwrap();
            for j in 0..45 {
                writeln!(source, "    let item_{j} = \"{}\";", "a".repeat(85)).unwrap();
            }
            source.push_str("}\n\n");
        }
        assert!(source.len() > 500_000 && source.len() < 2_000_000);
        fs::write(&path, source).unwrap();
        path
    }

    fn file_overview(root: &Path, cache: &OutlineCache) -> String {
        crate::search::search_symbol_expanded(
            "sample",
            root,
            cache,
            &Session::new(),
            &crate::index::bloom::BloomFilterCache::new(),
            0,
            None,
            None,
            false,
            Some(100_000),
        )
        .unwrap()
    }

    #[test]
    fn capped_read_is_independent_of_prior_uncapped_search() {
        let dir = tempfile::tempdir().unwrap();
        let path = large_outline_fixture(dir.path());
        let baseline = read(&path, &OutlineCache::new(), None);
        assert!(baseline.contains("outline truncated"));
        assert!(!baseline.contains("sample_149"));
        let cache = OutlineCache::new();
        let overview = file_overview(dir.path(), &cache);
        assert!(
            overview.contains("sample_149"),
            "fixture must prime an uncapped outline"
        );
        let after_search = read(&path, &cache, None);
        assert_eq!(
            after_search.lines().count(),
            baseline.lines().count(),
            "prior search changed the read's cap"
        );
        assert_eq!(after_search, baseline);
    }

    #[test]
    fn uncapped_search_is_independent_of_prior_capped_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = large_outline_fixture(dir.path());
        let baseline = file_overview(dir.path(), &OutlineCache::new());
        assert!(baseline.contains("sample_149"));
        let cache = OutlineCache::new();
        let first_read = read(&path, &cache, None);
        assert!(first_read.contains("outline truncated"));
        let after_read = file_overview(dir.path(), &cache);
        assert!(
            after_read.contains("sample_149"),
            "prior read hid the final symbol"
        );
        assert_eq!(after_read, baseline);
    }

    #[test]
    fn explicit_read_budget_reveals_entries_beyond_default_cached_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = large_outline_fixture(dir.path());
        let cache = OutlineCache::new();
        let default = read(&path, &cache, None);
        assert!(!default.contains("sample_149"));
        let expanded = read(&path, &cache, Some(100_000));
        assert!(expanded.contains("sample_149"), "{expanded}");
        assert!(!expanded.contains("outline truncated"), "{expanded}");
        assert_eq!(read(&path, &cache, None), default);
    }
}

#[cfg(test)]
mod elixir_signature_view_regression_tests {
    use super::*;

    #[test]
    fn multiline_keyword_body_stops_at_do_key_with_source_hashes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keyword.ex");
        let source = "def g(x),\n  do: (\n    IO.puts(x)\n    x + 123\n  )\n";
        std::fs::write(&path, source).unwrap();
        let output = tool_read(
            &serde_json::json!({"path": path, "mode": "signature"}),
            &OutlineCache::new(),
            &Session::new(),
            false,
        )
        .unwrap();

        for (index, visible) in ["def g(x),", "  do:"].iter().enumerate() {
            let original = source.lines().nth(index).unwrap();
            let hash = crate::format::line_hash(original.as_bytes());
            assert!(
                output.contains(&format!("{}:{hash:03x}|{visible}", index + 1)),
                "{output}"
            );
        }
        assert!(!output.contains("IO.puts"), "{output}");
        assert!(!output.contains("x + 123"), "{output}");
    }

    #[test]
    fn keyword_text_inside_default_string_is_not_body_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quoted.ex");
        let source = "def f(x \\\\ \", do:\"), do: x\n";
        std::fs::write(&path, source).unwrap();
        let output = tool_read(
            &serde_json::json!({"path": path, "mode": "signature"}),
            &OutlineCache::new(),
            &Session::new(),
            false,
        )
        .unwrap();

        let hash = crate::format::line_hash(source.trim_end().as_bytes());
        assert!(
            output.contains(&format!("1:{hash:03x}|def f(x \\\\ \", do:\"), do:")),
            "{output}"
        );
        assert!(!output.contains("do: x"), "{output}");
    }
}

#[cfg(test)]
mod markdown_pagination_tests {
    use super::*;

    #[test]
    fn markdown_pages_preserve_global_toc_and_mcp_navigation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guide.md");
        std::fs::write(&path, "# Root\n## Shared\na\n## Shared\nb\n## Last\nc\n").unwrap();
        let cache = OutlineCache::new();
        let session = Session::new();
        let first = tool_read(
            &serde_json::json!({"path":path,"offset":0,"limit":2}),
            &cache,
            &session,
            false,
        )
        .unwrap();
        let second = tool_read(
            &serde_json::json!({"path":path,"offset":2,"limit":2}),
            &cache,
            &session,
            false,
        )
        .unwrap();
        assert!(first.contains("Requested headings 1-2 of 4"), "{first}");
        assert!(first.contains(r#""offset":2"#), "{first}");
        assert!(second.contains("1.2") && second.contains("1.3"), "{second}");
        assert!(second.contains("End of listing."), "{second}");
        let selected = tool_read(
            &serde_json::json!({"path":path,"section":"toc:1.2"}),
            &cache,
            &session,
            false,
        )
        .unwrap();
        assert!(selected.contains("5  b"), "{selected}");
        let beyond = tool_read(
            &serde_json::json!({"path":path,"offset":usize::MAX,"limit":2}),
            &cache,
            &session,
            false,
        )
        .unwrap();
        assert!(beyond.contains("Requested headings 0 of 4"), "{beyond}");
        assert!(!beyond.contains("Next page:"), "{beyond}");
        for extra in [
            serde_json::json!({"full":true}),
            serde_json::json!({"mode":"full"}),
            serde_json::json!({"mode":"signature"}),
            serde_json::json!({"mode":"stripped"}),
            serde_json::json!({"section":"Root"}),
            serde_json::json!({"sections":["Root"]}),
        ] {
            let mut args = serde_json::json!({"path":path,"offset":0});
            args.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert!(tool_read(&args, &cache, &session, false).is_err(), "{args}");
        }
    }
}
