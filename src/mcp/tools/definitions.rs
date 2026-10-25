use serde_json::Value;

pub(in crate::mcp) fn tool_definitions(edit_mode: bool) -> Vec<Value> {
    let read_desc = if edit_mode {
        "Read a file with smart outlining. CLI equivalent: `tilth read <path>` (repeat `--section` for disjoint slices; multiple paths for batch reads). Replaces cat/head/tail and the host Read tool — \
         use this for all file reading. Output uses hashline format (line:hash|content) — \
         the line:hash anchors are required by tilth_write. Small files return full hashlined content. \
         Large files return a structural outline (no hashlines); use `section` to get hashlined \
         content for the lines you want to edit. Use `sections` to grab several disjoint slices \
         from the same file in one call. Use `full` to force complete content. \
         Use `paths` to read multiple files in one call."
    } else {
        "Read a file with smart outlining. CLI equivalent: `tilth read <path>` (repeat `--section` for disjoint slices; multiple paths for batch reads). Replaces cat/head/tail and the host Read tool — \
         use this for all file reading. Small files return full content. Large files return \
         a structural outline (functions, classes, imports) so you see the shape without \
         consuming your context window. Use `section` to read a specific line range or heading. \
         Use `sections` to grab several disjoint slices from the same file in one call. \
         Use `full` to force complete content. Use `paths` to read multiple files in one call."
    };
    let mut tools = vec![
        serde_json::json!({
            "name": "tilth_search",
            "annotations": { "readOnlyHint": true },
            "description": "Search for symbols, text, or regex patterns in code. Replaces grep/rg and the host Grep tool — use this for all code search. Symbol search returns definitions first (via tree-sitter AST), then usages, with full source code inlined for top matches. Content search finds literal text. Regex search supports full regex patterns. For cross-file tracing, pass comma-separated symbol names (max 5). Searches honor .tilthignore, .gitignore, .ignore, and Git excludes by default. Set gitignore=false for one call, or use TILTH_RESPECT_GITIGNORE=0 at startup, to bypass Git ignore rules.",
            "inputSchema": {
                "type": "object",
                "oneOf": [{"required":["query"],"not":{"required":["queries"]}}, {"required":["queries"],"not":{"required":["query"]}}],
                "properties": {
                    "queries": {"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":20,"description":"Literal queries in order; commas are not split. Mutually exclusive with query. Supports symbol/content/regex; shared budget."},
                    "select": {"type":"string","enum":["files","definitions","signatures","count"],"description":"Compact projection before paging. Definitions/signatures require symbol kind. No source expansion."},
                    "query": {
                        "type": "string",
                        "description": "Symbol name, text string, or regex pattern to search for. e.g. 'resolve_dependencies' or 'ServeHTTP,Next' for comma-separated multi-symbol lookup (max 5)."
                    },
                    "scope": {
                        "type": "string",
                        "description": "Directory, file, or glob of files/directories to search. Omit for the whole project. Relative scopes require an absolute root. Glob expansion honors ignore rules and is limited to 100,000 visited entries and 1,000 matching paths across the request. Missing scopes report up to five similar filenames (normalized Levenshtein similarity >=0.8). If none qualify, lower-confidence suggestions require similarity >=0.4, the same nonempty extension, and a shared whole filename-stem component of at least two characters (split on nonalphanumeric characters). Otherwise directory hints are offered. Hints stay within budget without choosing a replacement or widening the search. Valid scopes can still return results alongside failed scopes."
                    },
                    "scopes": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Search multiple directories, files, or globs as one combined result set for symbol, content, and regex searches. Callers pagination applies independently to each resolved scope. Do not provide both `scope` and `scopes`; relative scopes require an absolute `root`."
                    },
                    "kind": {
                        "type": "string",
                        "enum": ["symbol", "content", "regex", "callers"],
                        "default": "symbol",
                        "description": "Search type. symbol: structural definitions + usages. content: literal text. regex: regex pattern. callers: find ranked call sites. Every kind supports offset/limit pagination per target; callers additionally page each resolved scope independently."
                    },
                    "expand": {
                        "type": "number",
                        "default": 2,
                        "description": "Number of top matches to expand with full source code. Definitions show the full function/class body. Usages show ±10 context lines. With queries or select, omit expand or set it to 0; compact searches do not expand source."
                    },
                    "full": {
                        "type": "boolean",
                        "default": false,
                        "description": "Widen the match cap from 10 to 100 per query, for every kind. An explicit limit overrides this default page size. Costs tokens — use it when 10 matches cannot answer the question."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "Skip N ranked matches per target (zero-based), after combining scopes and deduplicating for symbol/content/regex. Callers apply the offset independently to each target and resolved scope. Applies to every search kind. Keep query, scope, context and filters unchanged between pages."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Ranked matches per page per target; for callers, per target and resolved scope. Applies to every search kind. Defaults to 10, or 100 with full:true; an explicit limit overrides that default. If budget truncates a page, retry the same offset with a smaller limit."
                    },
                    "context": {
                        "type": "string",
                        "description": "Path to the file the agent is currently editing. Boosts ranking of matches in the same directory or package."
                    },
                    "budget": {
                        "type": "number",
                        "description": "Max tokens in response."
                    },
                    "glob": {
                        "type": "string",
                        "description": "File pattern filter. Whitelist: \"*.rs\" (only Rust files). Exclude: \"!*.test.ts\" (skip test files). Brace expansion: \"*.{go,rs}\" (Go and Rust). Path patterns: \"src/**/*.ts\". For symbol/content/regex/callers results, narrows each resolved scope relative to its directory, or a selected file's parent; intersects with scope selections and honors ignore rules."
                    },
                    "root": {
                        "type": "string",
                        "description": "Absolute project root; anchors relative paths and scopes. Required with any relative path/scope."
                    }
                }
            }
        }),
        serde_json::json!({
            "name": "tilth_read",
            "annotations": { "readOnlyHint": true },
            "description": read_desc,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative file path to read. A relative path requires an absolute `root`; the server cannot see your shell cwd."
                    },
                    "slices": {"type":"array","minItems":1,"maxItems":20,"items":{"type":"object","required":["path","sections"],"properties":{"path":{"type":"string"},"sections":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"string","pattern":"^[1-9][0-9]*-[1-9][0-9]*$"}}}},"description":"Per-file numeric line ranges; at most 20 ranges total. Files follow first appearance; ranges sorted and overlapping/adjacent ranges merged per physical file. Shared budget. Exclusive with path/paths/symbols/sections/paging; auto/full modes only."},
                    "symbols": {"type":"array","items":{"type":"string","minLength":1},"minItems":1,"maxItems":20,"description":"Exact symbol names across path/paths; commas remain literal. Qualified names or name@line disambiguate. Returns per-name status, source once per declaration, shared budget. Supports auto/full/signature; signature conflicts with edit mode. Conflicts with sections and paging."},
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Multiple file paths to read in one call. Section selection requires path instead of paths. Each file gets independent smart handling. Saves round-trips vs multiple single reads."
                    },
                    "section": {
                        "type": "string",
                        "description": "Line range '45-89', heading title 'Architecture' (optional '##' level prefix), or TOC address 'toc:1.2'. Ambiguous titles return parent paths and copyable TOC addresses. Bypasses smart view. Use `sections` for multiple ranges."
                    },
                    "sections": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Multiple ranges from the same file in one call. Each entry is a line range, heading title (optional # prefix), or TOC address (toc:1.2). Emits each block in user-supplied order, separated by `─── lines X-Y ───` delimiters. Mutually exclusive with `section`. Capped at 20 ranges."
                    },
                    "full": {
                        "type": "boolean",
                        "default": false,
                        "description": "Legacy alias for mode='full'. Force full content output, bypass smart outlining."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["auto", "full", "signature", "stripped"],
                        "default": "auto",
                        "description": "Read view. auto: smart default. full: full content. signature: hash-prefixed declarations only. stripped: whole-file content with plain comments/debug logs/extra blanks removed."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "Skip N entries per path, including batches (zero-based). Units: directory children by name; declarations in auto/code and signature; headings in auto/Markdown; source lines in full/section; retained lines in stripped. Original source/hash/TOC addresses remain stable. Explicit code paging requests an uncapped declaration outline even for small files."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "default": 50,
                        "description": "Entries per page per path (default 50), including batches. Unit depends on mode; see offset. If budget truncates, retry the same offset with a smaller limit or a larger budget before advancing."
                    },
                    "budget": {
                        "type": "number",
                        "description": "Max tokens in response. For file outlines, an explicit budget replaces the default 100-entry large-file cap. Increase it for more outline entries."
                    },
                    "root": {
                        "type": "string",
                        "description": "Absolute project root; anchors relative paths and scopes. Required with any relative path/scope."
                    }
                }
            }
        }),
        serde_json::json!({
            "name": "tilth_list",
            "annotations": { "readOnlyHint": true },
            "description": "List files matching glob patterns as a directory tree. Replaces find/ls/tree and the host Glob tool — use this to see project structure with per-directory token-size rollups. Pass `patterns` to combine several globs into one tree.",
            "inputSchema": {
                "type": "object",
                "required": ["patterns"],
                "properties": {
                    "patterns": {
                        "type": "array",
                        "items": { "type": "string" },
                        "minItems": 1,
                        "maxItems": 20,
                        "description": "Glob patterns rendered into one tree, e.g. ['*.rs'] or ['*.rs', '*.toml']. Capped at 20."
                    },
                    "depth": {
                        "type": "number",
                        "description": "Cap directory depth (1 = top-level only)."
                    },
                    "scope": {
                        "type": "string",
                        "description": "Existing directory to root the tree at, never a file. Omit for the whole project."
                    },
                    "offset": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "Skip N matching files sorted by relative path (zero-based). Directory nodes do not count. Keep scope, patterns, depth and ignore settings unchanged between pages."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "default": 50,
                        "description": "Matching files per page (default 50). Tree counts and token rollups cover this page only. If budget truncates, retry the same offset with a smaller limit before advancing."
                    },
                    "budget": {
                        "type": "number",
                        "description": "Max tokens in response."
                    },
                    "root": {
                        "type": "string",
                        "description": "Absolute project root; anchors relative paths and scopes. Required with any relative path/scope."
                    }
                }
            }
        }),
        serde_json::json!({
              "name": "tilth_deps",
            "annotations": { "readOnlyHint": true },
            "description": "Blast-radius check before breaking changes. Shows what a file imports (local + external) and what other files call its exports, with symbol-level detail. Use ONLY when your planned edit changes a function signature, removes/renames an export, or modifies behavior that callers rely on. Do NOT use for reading files, adding new code, or internal-only changes — use tilth_read instead.",
            "inputSchema": {
                "type": "object",
                "required": ["path"],
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File to check before making breaking changes."
                    },
                    "scope": {
                        "type": "string",
                        "description": "Existing directory to search for dependents, never a file. Omit for the whole project."
                    },
                      "budget": {
                        "type": "number",
                        "description": "Max tokens. Truncates 'Used by' first."
                      },
                      "full": {
                          "type": "boolean",
                          "description": "Search all symbols and call sites and show all dependencies. Explicit budget still applies. Default: false."
                      },
                    "root": {
                        "type": "string",
                        "description": "Absolute project root; anchors relative paths and scopes. Required with any relative path/scope."
                    }
                }
            }
        }),
        serde_json::json!({
            "name": "tilth_grok",
            "annotations": { "readOnlyHint": true },
            "description": "Get everything structural about a symbol in one call — definition, body, signature, doc, callees, callers, siblings, tests. Use ONLY for 'understand this symbol' questions. Do NOT use for concept search (use tilth_search) or reading file contents (use tilth_read).",
            "inputSchema": {
                "type": "object",
                "required": ["target"],
                "properties": {
                    "target": {
                        "type": "string",
                        "description": "Symbol name, e.g. 'parse_unified_diff'. Also accepts 'src/diff/parse.rs:7' or 'Type::method'."
                    },
                    "scope": {
                        "type": "string",
                        "description": "Existing directory to narrow the search, never a file. Omit for the whole project."
                    },
                    "section": {
                        "type":"string",
                        "enum":["callers","callees","siblings","tests"],
                        "description":"Return only this relationship section plus compact target context, without expanding or recording source bodies. Omit for the existing overview."
                    },
                    "offset": {
                        "type":"integer","minimum":0,"default":0,
                        "description":"Skip N individual records in the selected section (zero-based). Requires section. Keep target/root/scope and source unchanged between pages."
                    },
                    "limit": {
                        "type":"integer","minimum":1,"default":50,
                        "description":"Records per selected-section page (default 50), after full collection/filtering/sorting. Requires section. Overrides overview/full caps. If budget truncates, retry the same offset with a smaller limit or a larger budget."
                    },
                    "budget": {
                        "type":"integer","minimum":0,
                        "description":"Maximum output tokens; section pages default to the standard response budget."
                    },
                    "full": {
                        "type": "boolean",
                        "default": false,
                        "description": "Widen overview caps: 50 callers, 30 callees, 30 siblings, 30 tests (default 5/5/8/8). Section pages collect all records regardless of full; their limit defaults to 50."
                    },
                    "root": {
                        "type": "string",
                        "description": "Absolute project root; anchors relative paths and scopes. Required with any relative path/scope."
                    }
                }
            }
        }),
        serde_json::json!({
            "name": "tilth_diff",
            "annotations": { "readOnlyHint": true },
            "description": "Structural diff showing function-level changes. Replaces git diff. Call with no args for uncommitted changes overview.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "Diff source: 'uncommitted' (default), 'staged', or a git ref (e.g. 'HEAD~1', 'main..feat'). Ignored when a, b, patch, or log is set."
                    },
                    "scope": {
                        "type": "string",
                        "description": "Restrict diff output to a specific file or directory path."
                    },
                    "a": {
                        "type": "string",
                        "description": "First file for a file-to-file diff. Must be used together with b."
                    },
                    "b": {
                        "type": "string",
                        "description": "Second file for a file-to-file diff. Must be used together with a."
                    },
                    "patch": {
                        "type": "string",
                        "description": "Path to a .patch file to parse instead of running git diff."
                    },
                    "log": {
                        "type": "string",
                        "description": "Git log range (e.g. 'HEAD~5..HEAD') — shows per-commit structural summaries."
                    },
                    "search": {
                        "type": "string",
                        "description": "Filter output to symbols or files matching this substring (case-insensitive)."
                    },
                    "blast": {
                        "type": "boolean",
                        "default": false,
                        "description": "Show blast-radius warnings for signature-changed symbols."
                    },
                    "expand": {
                        "type": "number",
                        "default": 0,
                        "description": "Number of changed symbols to expand with full source context."
                    },
                    "budget": {
                        "type": "number",
                        "description": "Max tokens in response."
                    },
                    "root": {
                        "type": "string",
                        "description": "Absolute project root; anchors relative paths and scopes. Required with any relative path/scope."
                    }
                }
            }
        }),
    ];

    if edit_mode {
        tools.push(serde_json::json!({
            "name": "tilth_write",
            "annotations": { "readOnlyHint": false },
            "description": "Batch write one or more files in one call. Replaces the host Edit and Write tools — DO NOT use those. Three per-file modes: `hash` (default — replace lines at hash anchors from tilth_read), `overwrite` (whole file; create-only by default — pass `overwrite: true` to replace an existing file), `append` (append `content`, creates if absent). overwrite/append responses echo the file's hashlines so you can chain anchored edits in the next call without re-reading. ALWAYS group writes to multiple files into a single tilth_write call — never call tilth_write twice in a row. Each file is processed independently (best-effort): a failure on one file does not block the others; results are reported per file. Partial success returns isError: false — scan the per-file `## <path>` sections for failures rather than trusting the top-level status. A parse error on one edit invalidates ALL edits for that file (none applied); retry the whole file after fixing the malformed entry. Each file path may appear at most once per call. Max 20 files per call. Example overwrite (new file): `tilth_write(files: [{path: \"src/new.rs\", mode: \"overwrite\", content: \"fn main(){}\\n\"}])`.",
            "inputSchema": {
                "type": "object",
                "required": ["files"],
                "properties": {
                    "files": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 20,
                        "description": "One entry per file. Use a single-element array for a single-file write. Each path must be unique within the call.",
                        "items": {
                            "type": "object",
                            "required": ["path"],
                            "properties": {
                                "path": {
                                    "type": "string",
                                    "description": "Absolute or relative file path. A relative path requires an absolute `root`; the server cannot see your shell cwd."
                                },
                                "mode": {
                                    "type": "string",
                                    "enum": ["hash", "h", "overwrite", "w", "append", "a"],
                                    "default": "hash",
                                    "description": "Write mode. hash (default): replace lines at hash anchors via `edits`. overwrite: write whole file from `content`; create-only by default — set `overwrite: true` to replace existing. append: append `content`, creates if absent."
                                },
                                "edits": {
                                    "type": "array",
                                    "minItems": 1,
                                    "description": "Hash-mode only: edit operations for this file, applied atomically per file.",
                                    "items": {
                                        "type": "object",
                                        "required": ["start", "content"],
                                        "properties": {
                                            "start": {
                                                "type": "string",
                                                "description": "Start anchor: 'line:hash' (e.g. '42:a3f'). Hash from tilth_read hashline output."
                                            },
                                            "end": {
                                                "type": "string",
                                                "description": "End anchor: 'line:hash'. If omitted, replaces only the start line."
                                            },
                                            "content": {
                                                "type": "string",
                                                "description": "Replacement text (can be multi-line). Empty string to delete the line(s)."
                                            }
                                        }
                                    }
                                },
                                "content": {
                                    "type": "string",
                                    "description": "overwrite / append mode only: the file contents (overwrite) or text to append (append)."
                                },
                                "overwrite": {
                                    "type": "boolean",
                                    "default": false,
                                    "description": "overwrite mode only: when true, replace an existing file. Default false fails with `AlreadyExists` so you don't clobber by accident."
                                }
                            },
                            "allOf": [
                                {
                                    "if": {"properties": {"mode": {"enum": ["hash", "h"]}}},
                                    "then": {"required": ["edits"]}
                                },
                                {
                                    "if": {
                                        "required": ["mode"],
                                        "properties": {
                                            "mode": {"enum": ["overwrite", "w", "append", "a"]}
                                        }
                                    },
                                    "then": {"required": ["content"]}
                                }
                            ]
                        }
                    },
                    "diff": {
                        "type": "boolean",
                        "default": false,
                        "description": "Set true to include a compact diff of changes in the response per file."
                    },
                    "root": {
                        "type": "string",
                        "description": "Optional absolute path. When provided, every RELATIVE file path in this call is anchored under `root` instead of the server's process cwd. Absolute file paths are used as-is. Use this when the server was launched from a different directory than the worktree you are editing."
                    }
                }
            }
        }));
    }

    for tool in &mut tools {
        let name = tool["name"].as_str().unwrap_or("").to_owned();
        if name == "tilth_deps" || name == "tilth_diff" {
            let unit = if name == "tilth_deps" {
                "dependency records across local, external, production dependents, then test dependents; paths sorted within each section. Explicit paging collects all exported symbols and callers before selecting the page"
            } else {
                "files by path in overview/directory view, symbol/hunk blocks by source order in file detail, or commits in git log order. Keep source/scope/search and the working tree unchanged between pages"
            };
            tool["inputSchema"]["properties"]["offset"] = serde_json::json!({"type":"integer","minimum":0,"default":0,
                "description":format!("Skip N {unit} (zero-based).")});
            tool["inputSchema"]["properties"]["limit"] = serde_json::json!({"type":"integer","minimum":1,"default":50,
                "description":"Entries per page (default 50). If budget truncates, retry the same offset with a smaller limit or larger budget before advancing."});
        }
        if name == "tilth_search" || name == "tilth_list" {
            tool["inputSchema"]["properties"]["gitignore"] = serde_json::json!({
                "type": "boolean",
                "description": "For this call, honor .gitignore, .ignore, and Git exclude rules when true; bypass them when false. Omit to use the current server setting (enabled by default)."
            });
        }
    }

    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_schema_exposes_pagination_for_every_search_kind() {
        let tools = tool_definitions(false);
        let search = tools
            .iter()
            .find(|tool| tool["name"] == "tilth_search")
            .unwrap();
        let properties = &search["inputSchema"]["properties"];
        for (name, minimum) in [("offset", 0), ("limit", 1)] {
            assert_eq!(properties[name]["type"], "integer");
            assert_eq!(properties[name]["minimum"], minimum);
            assert!(properties[name]["description"]
                .as_str()
                .unwrap()
                .contains("every search kind"));
        }
    }

    #[test]
    fn tilth_write_schema_requires_mode_specific_fields() {
        let tools = tool_definitions(true);
        let write = tools
            .iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some("tilth_write"))
            .expect("tilth_write tool definition present in edit mode");
        let items = &write["inputSchema"]["properties"]["files"]["items"];
        let all_of = items["allOf"]
            .as_array()
            .expect("items.allOf clauses present");
        assert_eq!(all_of.len(), 2, "expected hash-branch + content-branch");
        // Hash branch: when mode absent or in {hash, h}, require edits.
        assert_eq!(all_of[0]["then"]["required"][0], "edits");
        // Content branch: when mode in {overwrite, w, append, a}, require content.
        assert_eq!(all_of[1]["then"]["required"][0], "content");
        let content_modes = all_of[1]["if"]["properties"]["mode"]["enum"]
            .as_array()
            .expect("content-mode enum present");
        let modes: Vec<&str> = content_modes.iter().filter_map(|v| v.as_str()).collect();
        assert!(modes.contains(&"overwrite") && modes.contains(&"append"));
    }

    #[test]
    fn edit_mode_exposes_tilth_write_not_tilth_edit() {
        let tools = tool_definitions(true);
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(
            names.contains(&"tilth_write"),
            "tilth_write must be exposed"
        );
        assert!(
            !names.contains(&"tilth_edit"),
            "tilth_edit must be renamed away"
        );
    }

    #[test]
    fn tilth_search_schema_exposes_scopes_argument() {
        let tools = tool_definitions(false);
        let search = tools
            .iter()
            .find(|t| t.get("name").and_then(|v| v.as_str()) == Some("tilth_search"))
            .expect("tilth_search tool definition present");

        let scopes = &search["inputSchema"]["properties"]["scopes"];
        assert_eq!(scopes["type"], "array");
        assert_eq!(scopes["items"]["type"], "string");
        assert!(
            scopes["description"]
                .as_str()
                .is_some_and(|desc| desc.contains("scope") && desc.contains("scopes")),
            "description should explain mutual exclusion with scope: {scopes}"
        );
        assert!(
            scopes["description"]
                .as_str()
                .is_some_and(|desc| desc.contains("Callers") && desc.contains("independently")),
            "description should explain callers pagination: {scopes}"
        );
        assert!(
            search["description"]
                .as_str()
                .is_some_and(|desc| desc.contains(".tilthignore")
                    && desc.contains("TILTH_RESPECT_GITIGNORE")),
            "tool help should disclose ignore behavior: {search}"
        );
    }

    /// `tilth_files` was consolidated into `tilth_list`; the removed tool must
    /// no longer be advertised and `tilth_list` must stay present in both modes.
    #[test]
    fn tilth_files_folded_into_tilth_list() {
        for edit_mode in [false, true] {
            let defs = tool_definitions(edit_mode);
            let names: Vec<&str> = defs
                .iter()
                .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
                .collect();
            assert!(
                !names.contains(&"tilth_files"),
                "tilth_files must not be advertised (folded into tilth_list)"
            );
            assert!(
                names.contains(&"tilth_list"),
                "tilth_list must remain advertised"
            );
        }
    }

    #[test]
    fn gitignore_param_only_on_search_and_list() {
        for edit_mode in [false, true] {
            let defs = tool_definitions(edit_mode);
            for tool in &defs {
                let name = tool["name"].as_str().unwrap_or("");
                let has_gitignore = tool["inputSchema"]["properties"].get("gitignore").is_some();
                if name == "tilth_search" || name == "tilth_list" {
                    assert!(has_gitignore, "{name} must expose the gitignore parameter");
                } else {
                    assert!(
                        !has_gitignore,
                        "{name} must NOT expose the gitignore parameter"
                    );
                }
            }
        }
    }
    #[test]
    fn directory_tools_expose_integer_pagination() {
        let defs = tool_definitions(false);
        for name in ["tilth_read", "tilth_list"] {
            let schema =
                &defs.iter().find(|d| d["name"] == name).unwrap()["inputSchema"]["properties"];
            assert_eq!(schema["offset"]["type"], "integer");
            assert_eq!(schema["offset"]["minimum"], 0);
            assert_eq!(schema["limit"]["type"], "integer");
            assert_eq!(schema["limit"]["minimum"], 1);
            assert_eq!(schema["limit"]["default"], 50);
        }
    }

    /// #195: tools that refuse a file scope say so in the schema; `tilth_diff`
    /// accepts either and says that instead.
    #[test]
    fn scope_descriptions_state_what_scope_accepts() {
        let defs = tool_definitions(true);
        let scope_desc = |name: &str| -> String {
            defs.iter()
                .find(|t| t.get("name").and_then(Value::as_str) == Some(name))
                .and_then(|t| t.pointer("/inputSchema/properties/scope/description"))
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{name} must advertise scope"))
                .to_string()
        };
        for name in ["tilth_list", "tilth_deps", "tilth_grok"] {
            let desc = scope_desc(name);
            assert!(
                desc.contains("directory") && desc.contains("never a file"),
                "{name} scope must be directory-only: {desc}"
            );
        }
        assert!(
            scope_desc("tilth_search").contains("glob"),
            "tilth_search must advertise file globs"
        );
        assert!(
            scope_desc("tilth_diff").contains("file or directory"),
            "tilth_diff scope accepts a file or a directory"
        );
    }

    /// tilth publishes no token-saving figures, so the tool that reported them
    /// stays out of the tool list in both modes.
    #[test]
    fn tilth_savings_is_not_advertised() {
        for edit_mode in [false, true] {
            let defs = tool_definitions(edit_mode);
            let names: Vec<&str> = defs
                .iter()
                .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
                .collect();
            assert!(
                !names.contains(&"tilth_savings"),
                "tilth_savings must not be advertised (edit_mode={edit_mode})"
            );
        }
    }
}
