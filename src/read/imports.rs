//! Resolve import statements to local file paths.
//! Used by the MCP layer to hint related files after an outlined read.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::SystemTime;

use crate::lang::detect_file_type;
use crate::types::{FileType, Lang};

const MAX_SUGGESTIONS: usize = 8;

/// Extract import sources from a code file and resolve them to existing local file paths.
/// Returns empty Vec for non-code files, files with no imports, or when all imports are external.
pub fn resolve_related_files(file_path: &Path) -> Vec<PathBuf> {
    let Ok(content) = fs::read_to_string(file_path) else {
        return Vec::new();
    };
    resolve_related_files_with_content(file_path, &content)
}

/// Same as `resolve_related_files` but takes pre-read content to avoid a redundant file read.
pub fn resolve_related_files_with_content(file_path: &Path, content: &str) -> Vec<PathBuf> {
    let FileType::Code(lang) = detect_file_type(file_path) else {
        return Vec::new();
    };

    let Some(_dir) = file_path.parent() else {
        return Vec::new();
    };

    let mut results = Vec::new();
    for line in content.lines() {
        if results.len() >= MAX_SUGGESTIONS {
            break;
        }
        if !is_import_line(line, lang) {
            continue;
        }
        let source = crate::lang::outline::extract_import_source(line, Some(lang));
        if source.is_empty() {
            continue;
        }
        if let Some(path) = resolve_import_source(file_path, &source, lang) {
            if !results.contains(&path) {
                results.push(path);
            }
        }
    }
    results
}

/// Resolve one import to a local file, including TypeScript `compilerOptions.paths`.
/// The closest tsconfig is used, as TypeScript projects in a monorepo can have
/// different aliases. Unresolvable package imports remain external.
pub(crate) fn resolve_import_source(file: &Path, source: &str, lang: Lang) -> Option<PathBuf> {
    let dir = file.parent()?;
    if !is_external(source, lang) {
        if let Some(path) = resolve(dir, source, lang) {
            return Some(path);
        }
    }
    if source.starts_with('.') || source.starts_with('/') {
        return None;
    }
    if !matches!(lang, Lang::TypeScript | Lang::Tsx | Lang::JavaScript) {
        return None;
    }
    let config = dir
        .ancestors()
        .map(|ancestor| ancestor.join("tsconfig.json"))
        .find(|candidate| candidate.is_file())?;
    let chain = tsconfig_chain(&config);
    let base_url = chain.iter().find_map(|(path, json)| {
        json.pointer("/compilerOptions/baseUrl")
            .and_then(|value| value.as_str())
            .and_then(|relative| path.parent().map(|dir| dir.join(relative)))
    });
    if let Some((path, paths)) = chain.iter().find_map(|(path, json)| {
        json.pointer("/compilerOptions/paths")
            .and_then(|value| value.as_object())
            .map(|paths| (path, paths))
    }) {
        let base = base_url
            .clone()
            .unwrap_or_else(|| path.parent().unwrap_or(dir).to_path_buf());
        let best = paths
            .iter()
            .filter_map(|(pattern, replacements)| {
                match_alias(pattern, source).map(|capture| (pattern, replacements, capture))
            })
            .max_by_key(|(pattern, _, _)| {
                // TypeScript prefers exact matches, then the longest prefix
                // preceding a wildcard. Never fall back to a weaker pattern.
                let (prefix, exact) = pattern
                    .split_once('*')
                    .map_or((pattern.as_str(), true), |(prefix, _)| (prefix, false));
                (exact, prefix.len(), pattern.len())
            });
        if let Some((_, replacements, capture)) = best {
            return replacements
                .as_array()?
                .iter()
                .filter_map(|value| value.as_str())
                .find_map(|replacement| {
                    resolve_js(&base, &replacement.replace('*', capture))
                        .map(|found| normalize_path(&found))
                });
        }
    }
    base_url.and_then(|base| resolve_js(&base, source).map(|found| normalize_path(&found)))
}

fn tsconfig_chain(start: &Path) -> Vec<(PathBuf, Arc<serde_json::Value>)> {
    let mut result = Vec::new();
    let mut seen = std::collections::HashSet::new();
    append_tsconfig(start, &mut result, &mut seen, 0);
    result
}

fn append_tsconfig(
    config: &Path,
    result: &mut Vec<(PathBuf, Arc<serde_json::Value>)>,
    seen: &mut std::collections::HashSet<PathBuf>,
    depth: usize,
) {
    if depth >= 16 {
        return;
    }
    let canonical = config
        .canonicalize()
        .unwrap_or_else(|_| config.to_path_buf());
    if !seen.insert(canonical) {
        return;
    }
    let Some(json) = tsconfig_json(config) else {
        return;
    };
    let parents: Vec<_> = match json.get("extends") {
        Some(serde_json::Value::String(name)) => vec![name.clone()],
        Some(serde_json::Value::Array(names)) => names
            .iter()
            .filter_map(|name| name.as_str().map(str::to_owned))
            .rev()
            .collect(),
        _ => Vec::new(),
    };
    result.push((config.to_path_buf(), json));
    for parent in parents {
        if let Some(path) = resolve_tsconfig_extends(config, &parent) {
            append_tsconfig(&path, result, seen, depth + 1);
        }
    }
}

fn resolve_tsconfig_extends(config: &Path, name: &str) -> Option<PathBuf> {
    let dir = config.parent()?;
    if name.starts_with('.') || name.starts_with('/') {
        return tsconfig_candidate(&dir.join(name));
    }
    for ancestor in dir.ancestors() {
        if let Some(found) = tsconfig_candidate(&ancestor.join("node_modules").join(name)) {
            return Some(found);
        }
    }
    None
}

fn tsconfig_candidate(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if path.extension().is_none() {
        let json = path.with_extension("json");
        if json.is_file() {
            return Some(json);
        }
    }
    if !path.is_dir() {
        return None;
    }
    if let Ok(package) = fs::read_to_string(path.join("package.json")) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&package) {
            if let Some(name) = json.get("tsconfig").and_then(|value| value.as_str()) {
                if let Some(found) = tsconfig_candidate(&path.join(name)) {
                    return Some(found);
                }
            }
        }
    }
    path.join("tsconfig.json")
        .is_file()
        .then(|| path.join("tsconfig.json"))
}

/// Import specifiers from JS/TS syntax. Unlike line-based extraction this
/// includes multiline imports and re-exports.
/// The second field records whether the statement re-exports another module.
/// Return (local binding, imported name) for a static import statement.
pub(crate) fn js_import_bindings(text: &str) -> Vec<(String, String)> {
    let clause = text
        .trim_start()
        .strip_prefix("import")
        .unwrap_or("")
        .trim_start();
    let clause = clause.strip_prefix("type ").unwrap_or(clause);
    let mut bindings = Vec::new();
    if let Some(start) = clause.find('{') {
        if let Some(end) = clause[start + 1..].find('}') {
            for item in clause[start + 1..start + 1 + end].split(',') {
                let item = item.trim().strip_prefix("type ").unwrap_or(item.trim());
                let mut words = item.split_whitespace();
                if let Some(imported) = words.next() {
                    let local = if words.next() == Some("as") {
                        words.next().unwrap_or(imported)
                    } else {
                        imported
                    };
                    bindings.push((local.to_string(), imported.to_string()));
                }
            }
        }
    }
    if let Some(namespace) = clause.strip_prefix("* as ") {
        if let Some(local) = namespace.split_whitespace().next() {
            bindings.push((local.to_string(), "*".to_string()));
        }
    } else if !clause.starts_with(['{', '\'', '"']) {
        if let Some(local) = clause
            .split(|c: char| c.is_whitespace() || c == ',')
            .next()
            .filter(|name| !name.is_empty())
        {
            bindings.push((local.to_string(), "default".to_string()));
        }
    }
    bindings
}

/// Return (local binding, exported name) for a brace export list.
pub(crate) fn js_export_bindings(text: &str) -> Vec<(String, String)> {
    let clause = text
        .trim()
        .strip_prefix("export")
        .unwrap_or("")
        .trim_start();
    if let Some(value) = clause.strip_prefix("default") {
        let value = value.trim().trim_end_matches(';').trim();
        if !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, '_' | '$' | '.'))
        {
            return vec![(value.to_string(), "default".to_string())];
        }
        return Vec::new();
    }
    if !clause
        .strip_prefix("type ")
        .unwrap_or(clause)
        .trim_start()
        .starts_with('{')
    {
        return Vec::new();
    }
    let (Some(start), Some(end)) = (text.find('{'), text.find('}')) else {
        return Vec::new();
    };
    text[start + 1..end]
        .split(',')
        .filter_map(|item| {
            let item = item.trim().strip_prefix("type ").unwrap_or(item.trim());
            let mut words = item.split_whitespace();
            let local = words.next()?;
            let exported = if words.next() == Some("as") {
                words.next().unwrap_or(local)
            } else {
                local
            };
            Some((local.to_string(), exported.to_string()))
        })
        .collect()
}

pub(crate) fn js_module_sources(content: &str, lang: Lang) -> Vec<(String, bool)> {
    let Some(grammar) = crate::lang::outline::outline_language(lang) else {
        return Vec::new();
    };
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&grammar).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(content, None) else {
        return Vec::new();
    };
    let mut locally_exported = std::collections::HashSet::new();
    let mut export_cursor = tree.root_node().walk();
    for statement in tree.root_node().named_children(&mut export_cursor) {
        if statement.kind() == "export_statement"
            && statement.child_by_field_name("source").is_none()
        {
            if let Ok(text) = statement.utf8_text(content.as_bytes()) {
                locally_exported
                    .extend(js_export_bindings(text).into_iter().map(|(local, _)| local));
            }
        }
    }
    let mut result = Vec::new();
    let mut cursor = tree.root_node().walk();
    for statement in tree.root_node().named_children(&mut cursor) {
        if !matches!(statement.kind(), "import_statement" | "export_statement") {
            continue;
        }
        let Some(source) = statement.child_by_field_name("source") else {
            continue;
        };
        let Ok(raw) = source.utf8_text(content.as_bytes()) else {
            continue;
        };
        let value = raw.trim_matches(['\'', '"']);
        if !value.is_empty() {
            let reexport = statement.kind() == "export_statement"
                || statement
                    .utf8_text(content.as_bytes())
                    .ok()
                    .is_some_and(|text| {
                        js_import_bindings(text)
                            .iter()
                            .any(|(local, _)| locally_exported.contains(local))
                    });
            result.push((value.to_string(), reexport));
        }
    }
    result
}

type CachedImports = (SystemTime, u64, Arc<Vec<(String, bool)>>);
static JS_IMPORT_CACHE: OnceLock<dashmap::DashMap<PathBuf, CachedImports>> = OnceLock::new();

/// Avoid reparsing unchanged files on repeated dependency and grok queries.
pub(crate) fn js_module_sources_from_file(path: &Path, lang: Lang) -> Vec<(String, bool)> {
    let Ok(metadata) = fs::metadata(path) else {
        return Vec::new();
    };
    let Ok(modified) = metadata.modified() else {
        return Vec::new();
    };
    let cache = JS_IMPORT_CACHE.get_or_init(dashmap::DashMap::new);
    if let Some(cached) = cache.get(path) {
        if cached.0 == modified && cached.1 == metadata.len() {
            return cached.2.as_ref().clone();
        }
    }
    let Ok(content) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let sources = if content.contains("import") || content.contains("export") {
        js_module_sources(&content, lang)
    } else {
        Vec::new()
    };
    cache.insert(
        path.to_path_buf(),
        (modified, metadata.len(), Arc::new(sources.clone())),
    );
    sources
}

type CachedConfig = (SystemTime, u64, Arc<serde_json::Value>);
static TSCONFIG_CACHE: OnceLock<dashmap::DashMap<PathBuf, CachedConfig>> = OnceLock::new();

fn tsconfig_json(path: &Path) -> Option<Arc<serde_json::Value>> {
    let metadata = fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let cache = TSCONFIG_CACHE.get_or_init(dashmap::DashMap::new);
    if let Some(cached) = cache.get(path) {
        if cached.0 == modified && cached.1 == metadata.len() {
            return Some(Arc::clone(&cached.2));
        }
    }
    let raw = fs::read_to_string(path).ok()?;
    let parsed = Arc::new(serde_json::from_str(&strip_json_comments(&raw)).ok()?);
    cache.insert(
        path.to_path_buf(),
        (modified, metadata.len(), Arc::clone(&parsed)),
    );
    Some(parsed)
}

fn match_alias<'a>(pattern: &str, source: &'a str) -> Option<&'a str> {
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        source.strip_prefix(prefix)?.strip_suffix(suffix)
    } else if pattern == source {
        Some("")
    } else {
        None
    }
}

fn strip_json_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut quoted = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if quoted {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else if ch == '"' {
            quoted = true;
            out.push(ch);
        } else if ch == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                    break;
                }
            }
        } else if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut previous = '\0';
            for next in chars.by_ref() {
                if next == '\n' {
                    out.push('\n');
                }
                if previous == '*' && next == '/' {
                    break;
                }
                previous = next;
            }
        } else {
            out.push(ch);
        }
    }
    // JSONC permits trailing commas, which are common in tsconfig files.
    let mut cleaned = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    let mut quoted = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if quoted {
            cleaned.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                quoted = false;
            }
        } else if ch == '"' {
            quoted = true;
            cleaned.push(ch);
        } else if ch == ',' {
            if !matches!(chars.clone().find(|c| !c.is_whitespace()), Some('}' | ']')) {
                cleaned.push(ch);
            }
        } else {
            cleaned.push(ch);
        }
    }
    cleaned
}

pub(crate) fn is_import_line(line: &str, lang: Lang) -> bool {
    let trimmed = line.trim_start();
    match lang {
        Lang::Rust => trimmed.starts_with("use "),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            trimmed.starts_with("import ") || trimmed.starts_with("import{")
        }
        Lang::Python => trimmed.starts_with("import ") || trimmed.starts_with("from "),
        Lang::Go | Lang::Java | Lang::Scala | Lang::Kotlin => trimmed.starts_with("import "),
        Lang::C | Lang::Cpp => trimmed.starts_with("#include"),
        Lang::Elixir => {
            trimmed.starts_with("alias ")
                || trimmed.starts_with("import ")
                || trimmed.starts_with("use ")
                || trimmed.starts_with("require ")
        }
        Lang::Bash => trimmed
            .strip_prefix("source")
            .or_else(|| trimmed.strip_prefix('.'))
            .is_some_and(|rest| rest.starts_with(char::is_whitespace)),
        _ => false,
    }
}

pub(crate) fn is_external(source: &str, lang: Lang) -> bool {
    match lang {
        Lang::Rust => {
            !(source.starts_with("crate::")
                || source.starts_with("self::")
                || source.starts_with("super::"))
        }
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => {
            !(source.starts_with('.') || source.starts_with("@/") || source.starts_with("~/"))
        }
        // Bash: dot-relative paths are local; anything else (bare name, /abs/path) is external.
        Lang::Python | Lang::Bash => !source.starts_with('.'),
        Lang::C | Lang::Cpp => !source.starts_with('"'),
        // Elixir, Go, Java, Scala, Kotlin — can't resolve without build system knowledge.
        _ => true,
    }
}

fn resolve(dir: &Path, source: &str, lang: Lang) -> Option<PathBuf> {
    let raw = match lang {
        Lang::Rust => resolve_rust(dir, source),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => resolve_js(dir, source),
        Lang::Python => resolve_python(dir, source),
        Lang::C | Lang::Cpp => resolve_c_include(dir, source),
        Lang::Bash => resolve_bash(dir, source),
        // Elixir, Go, Java, etc. — module-to-file mapping requires build system conventions.
        _ => None,
    };
    raw.map(|p| normalize_path(&p))
}

/// Lexically collapse `.` and `..` components without touching the filesystem.
/// `dir.join("../foo")` returns a `PathBuf` containing literal `..`; without this,
/// distinct spellings of the same target file produce distinct `PathBuf`s and
/// downstream callers (dedup loops, `HashMap` keys) treat them as different files.
fn normalize_path(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                let pop_ok = matches!(
                    out.components().next_back(),
                    Some(Component::Normal(_) | Component::Prefix(_))
                );
                if pop_ok {
                    out.pop();
                } else {
                    out.push(comp);
                }
            }
            _ => out.push(comp),
        }
    }
    out
}

// --- Rust ---

fn resolve_rust(dir: &Path, source: &str) -> Option<PathBuf> {
    if let Some(rest) = source.strip_prefix("crate::") {
        let src_dir = find_src_ancestor(dir)?;
        try_rust_path(src_dir, rest)
    } else if let Some(rest) = source.strip_prefix("self::") {
        try_rust_path(dir, rest)
    } else if let Some(rest) = source.strip_prefix("super::") {
        try_rust_path(dir.parent()?, rest)
    } else {
        None
    }
}

/// Try progressively shorter paths until one resolves.
/// `cache::OutlineCache` → try cache/OutlineCache.rs (no) → cache.rs (yes).
/// `read::imports` → try read/imports.rs (yes) → stop.
fn try_rust_path(base: &Path, rest: &str) -> Option<PathBuf> {
    let segments: Vec<&str> = rest.split("::").collect();
    for len in (1..=segments.len()).rev() {
        let rel: PathBuf = segments[..len].iter().collect();
        if let Some(found) = try_rust_module(&base.join(&rel)) {
            return Some(found);
        }
    }
    None
}

fn try_rust_module(base: &Path) -> Option<PathBuf> {
    let with_rs = base.with_extension("rs");
    if with_rs.exists() {
        return Some(with_rs);
    }
    let mod_rs = base.join("mod.rs");
    if mod_rs.exists() {
        return Some(mod_rs);
    }
    None
}

fn find_src_ancestor(start: &Path) -> Option<&Path> {
    let mut current = start;
    loop {
        if current.file_name().and_then(|n| n.to_str()) == Some("src") {
            return Some(current);
        }
        current = current.parent()?;
    }
}

// --- JS/TS ---

fn resolve_js(dir: &Path, source: &str) -> Option<PathBuf> {
    let base = dir.join(source);
    // An explicit runtime extension can point to a TypeScript source file.
    let substitute: &[&str] = match base.extension().and_then(|ext| ext.to_str()) {
        Some("js") => &["ts", "tsx", "d.ts"],
        Some("jsx") => &["tsx", "d.ts"],
        Some("mjs") => &["mts", "d.mts"],
        Some("cjs") => &["cts", "d.cts"],
        _ => &[],
    };
    for ext in substitute {
        let candidate = base.with_extension(ext);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // Try extensions for extensionless imports.
    for ext in &[".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs"] {
        let candidate = PathBuf::from(format!("{}{ext}", base.display()));
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // Already has extension
    if base.exists() && base.is_file() {
        return Some(base);
    }
    // Index files
    for name in &[
        "index.ts",
        "index.tsx",
        "index.mts",
        "index.cts",
        "index.js",
        "index.jsx",
        "index.mjs",
        "index.cjs",
    ] {
        let candidate = base.join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

// --- Python ---

fn resolve_python(dir: &Path, source: &str) -> Option<PathBuf> {
    let dots = source.bytes().take_while(|&b| b == b'.').count();
    if dots == 0 {
        return None;
    }
    // Each dot beyond the first goes up one directory.
    let mut base = dir.to_path_buf();
    for _ in 1..dots {
        base = base.parent()?.to_path_buf();
    }
    let module_part = &source[dots..];
    if module_part.is_empty() {
        // Bare `from . import X`
        let init = base.join("__init__.py");
        return if init.exists() { Some(init) } else { None };
    }
    let rel = module_part.replace('.', "/");
    let as_file = base.join(format!("{rel}.py"));
    if as_file.exists() {
        return Some(as_file);
    }
    let as_pkg = base.join(&rel).join("__init__.py");
    if as_pkg.exists() {
        return Some(as_pkg);
    }
    None
}

// --- C/C++ ---

fn resolve_c_include(dir: &Path, source: &str) -> Option<PathBuf> {
    let clean = source.trim_matches('"');
    let candidate = dir.join(clean);
    if candidate.exists() {
        Some(candidate)
    } else {
        None
    }
}

// --- Bash ---

fn resolve_bash(dir: &Path, source: &str) -> Option<PathBuf> {
    // Only resolve literal relative paths — no extension inference. A single
    // metadata() stat avoids the exists()+is_file() two-call TOCTOU; resolution
    // is best-effort, so a stale result only ever costs a related-file hint.
    let candidate = dir.join(source);
    std::fs::metadata(&candidate)
        .is_ok_and(|m| m.is_file())
        .then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn tsconfig_prefers_specific_path_pattern() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("wrong/@svc")).unwrap();
        fs::write(
            root.join("tsconfig.json"),
            r#"{"compilerOptions":{"paths":{
            "*": ["wrong/*"], "@svc/*": ["src/*"]
        }}}"#,
        )
        .unwrap();
        fs::write(root.join("src/service.ts"), "").unwrap();
        fs::write(root.join("wrong/@svc/service.ts"), "").unwrap();
        assert_eq!(
            resolve_import_source(&root.join("client.ts"), "@svc/service", Lang::TypeScript),
            Some(root.join("src/service.ts"))
        );
    }

    #[test]
    fn tsconfig_package_extends_resolves_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("node_modules/@org/config")).unwrap();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("node_modules/@org/config/package.json"),
            r#"{"tsconfig":"base.json"}"#,
        )
        .unwrap();
        fs::write(
            root.join("node_modules/@org/config/base.json"),
            r#"{"compilerOptions":{"paths":{"@lib/*":["../../../src/*"]}}}"#,
        )
        .unwrap();
        fs::write(root.join("tsconfig.json"), r#"{"extends":"@org/config"}"#).unwrap();
        fs::write(root.join("src/util.ts"), "").unwrap();
        assert_eq!(
            resolve_import_source(&root.join("client.ts"), "@lib/util", Lang::TypeScript),
            Some(root.join("src/util.ts"))
        );
    }

    #[test]
    fn tsconfig_array_extends_uses_later_base_first() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("first")).unwrap();
        fs::create_dir_all(root.join("second")).unwrap();
        fs::write(
            root.join("base-a.json"),
            r#"{"compilerOptions":{"paths":{"@lib/*":["first/*"]}}}"#,
        )
        .unwrap();
        fs::write(
            root.join("base-b.json"),
            r#"{"compilerOptions":{"paths":{"@lib/*":["second/*"]}}}"#,
        )
        .unwrap();
        fs::write(
            root.join("tsconfig.json"),
            r#"{"extends":["./base-a.json","./base-b.json"]}"#,
        )
        .unwrap();
        fs::write(root.join("first/util.ts"), "").unwrap();
        fs::write(root.join("second/util.ts"), "").unwrap();
        assert_eq!(
            resolve_import_source(&root.join("client.ts"), "@lib/util", Lang::TypeScript),
            Some(root.join("second/util.ts"))
        );
    }

    #[test]
    fn js_runtime_extensions_resolve_typescript_source() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("service.ts"), "").unwrap();
        fs::write(root.join("module.mts"), "").unwrap();
        fs::write(root.join("common.cts"), "").unwrap();
        assert_eq!(
            resolve_js(root, "./service.js"),
            Some(root.join("service.ts"))
        );
        assert_eq!(
            resolve_js(root, "./module.mjs"),
            Some(root.join("module.mts"))
        );
        assert_eq!(
            resolve_js(root, "./common.cjs"),
            Some(root.join("common.cts"))
        );
    }

    #[test]
    fn import_cache_refreshes_after_file_change() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("client.ts");
        fs::write(&path, "import './one';\n").unwrap();
        assert_eq!(
            js_module_sources_from_file(&path, Lang::TypeScript)[0].0,
            "./one"
        );
        fs::write(&path, "import './two-longer';\n").unwrap();
        assert_eq!(
            js_module_sources_from_file(&path, Lang::TypeScript)[0].0,
            "./two-longer"
        );
    }

    #[test]
    fn tsconfig_alias_in_extended_config_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("service/src")).unwrap();
        fs::create_dir_all(root.join("shared")).unwrap();
        fs::write(
            root.join("tsconfig.base.json"),
            r#"{"compilerOptions":{"baseUrl":".","paths":{"@shared/*":["shared/*",]}}}"#,
        )
        .unwrap();
        fs::write(
            root.join("service/tsconfig.json"),
            r#"{"extends":"../tsconfig.base.json"}"#,
        )
        .unwrap();
        let target = root.join("shared/util.ts");
        fs::write(&target, "export const util = 1;").unwrap();
        let importer = root.join("service/src/main.ts");
        assert_eq!(
            resolve_import_source(&importer, "@shared/util", Lang::TypeScript),
            Some(target)
        );
    }

    #[test]
    fn js_module_sources_handles_multiline_and_reexports() {
        let source =
            "import {\n  Service,\n} from '@svc/service';\nexport { Other } from './other';\n";
        assert_eq!(
            js_module_sources(source, Lang::TypeScript),
            vec![
                ("@svc/service".to_string(), false),
                ("./other".to_string(), true)
            ]
        );
    }

    #[test]
    fn normalize_collapses_dot_and_parent_components() {
        let p = Path::new("temporal/workflows/../utils/activityProxies.ts");
        assert_eq!(
            normalize_path(p),
            PathBuf::from("temporal/utils/activityProxies.ts")
        );
        let p = Path::new("app/db/./db.ts");
        assert_eq!(normalize_path(p), PathBuf::from("app/db/db.ts"));
    }

    #[test]
    fn normalize_preserves_leading_parent_when_unresolvable() {
        // No prior Normal component to pop, so ".." is kept.
        let p = Path::new("../outside.ts");
        assert_eq!(normalize_path(p), PathBuf::from("../outside.ts"));
    }

    #[test]
    fn js_resolve_returns_normalized_path_for_parent_import() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("temporal/workflows")).unwrap();
        fs::create_dir_all(root.join("temporal/utils")).unwrap();
        fs::write(root.join("temporal/utils/activityProxies.ts"), "").unwrap();

        let resolved = resolve_js(&root.join("temporal/workflows"), "../utils/activityProxies")
            .expect("should resolve");
        let normalized = normalize_path(&resolved);
        assert_eq!(normalized, root.join("temporal/utils/activityProxies.ts"));
        // No "../" component should survive normalization.
        assert!(
            !normalized
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "normalized path still contains '..': {normalized:?}"
        );
    }

    #[test]
    fn js_resolve_dedups_different_spellings_of_same_file() {
        // Two importers of the same file via different relative paths must
        // produce the same PathBuf so that hot-file counting aggregates them.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("a/c")).unwrap();
        fs::write(root.join("a/b/target.ts"), "").unwrap();

        let from_sibling =
            resolve(&root.join("a/b"), "./target", Lang::TypeScript).expect("sibling");
        let from_cousin =
            resolve(&root.join("a/c"), "../b/target", Lang::TypeScript).expect("cousin");

        assert_eq!(
            from_sibling, from_cousin,
            "different spellings should normalize to the same PathBuf"
        );
    }

    #[test]
    fn bash_is_import_line_tab_separated() {
        // Tab between `source` and path is valid bash and must be detected.
        assert!(
            is_import_line("source\t./lib.sh", Lang::Bash),
            "source<TAB>./lib.sh should be detected as an import line"
        );
        // False positives: `sourcefile=1` looks like it starts with `source` but
        // has no whitespace separator.
        assert!(
            !is_import_line("sourcefile=1", Lang::Bash),
            "sourcefile=1 must not be detected as an import line"
        );
        // `./script.sh` is a script execution, not a source directive.
        assert!(
            !is_import_line("./script.sh", Lang::Bash),
            "./script.sh must not be detected as an import line"
        );
        // `.bashrc` — dot followed by non-whitespace, not a source directive.
        assert!(
            !is_import_line(".bashrc", Lang::Bash),
            ".bashrc must not be detected as an import line"
        );
    }
}
