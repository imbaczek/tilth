//! Project fingerprint for MCP initialization.
//! Gives agents instant orientation without a tool call.

use std::collections::HashMap;
use std::fmt::Write;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::lang::detect_file_type;
use crate::read::imports::is_import_line;
use crate::types::{FileType, Lang};

/// Compute a project fingerprint for MCP initialization.
/// Must be fast (<250ms) — runs synchronously in the initialize handler.
/// Returns empty string on any failure (no error propagation).
#[must_use]
pub fn fingerprint(root: &Path) -> String {
    fingerprint_with_recursion(root, true)
}

/// Compute a fingerprint with either a recursive or depth-two source walk.
#[must_use]
pub fn fingerprint_with_recursion(root: &Path, recursive: bool) -> String {
    fingerprint_page(root, recursive, 0, 10)
}

/// Compute a fingerprint with a depth-ordered directory page.
/// Offset and limit apply to directories, independently for each project root.
#[must_use]
pub fn fingerprint_page(root: &Path, recursive: bool, offset: usize, limit: usize) -> String {
    let start = Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fingerprint_inner(root, recursive, offset, limit)
    }));
    let elapsed = start.elapsed();
    if elapsed.as_millis() > 250 {
        crate::diagnostic!(
            "[tilth] fingerprint took {}ms (>250ms budget)",
            elapsed.as_millis()
        );
    }
    result.unwrap_or_default()
}

fn fingerprint_inner(root: &Path, recursive: bool, offset: usize, limit: usize) -> String {
    let mut lines: Vec<String> = Vec::new();
    let walk = walk_files(root, recursive);

    // Determine primary language
    let primary_lang = walk
        .lang_counts
        .iter()
        .max_by(|(left_lang, left_count), (right_lang, right_count)| {
            left_count
                .cmp(right_count)
                .then_with(|| lang_display_name(**left_lang).cmp(lang_display_name(**right_lang)))
        })
        .map(|(lang, _)| *lang);

    let lang_name = primary_lang.map_or("Unknown", lang_display_name);
    let total_files = primary_lang
        .and_then(|l| walk.lang_counts.get(&l))
        .copied()
        .unwrap_or_else(|| walk.lang_counts.values().sum::<usize>());

    let mut directories: Vec<_> = walk.directory_lang_counts.keys().collect();
    directories.sort_by_key(|dir| (directory_depth(dir), dir.as_str()));
    let directory_count = directories.len();
    let page: Vec<_> = directories.into_iter().skip(offset).take(limit).collect();

    lines.push(format!(
        "[tilth] {lang_name} project — {total_files} source files, {directory_count} directories"
    ));
    if !page.is_empty() {
        let display: Vec<_> = page.iter().map(|dir| format!("{dir}/")).collect();
        lines.push(format!("  dirs: {}", display.join(" ")));
    }
    if offset > 0 || page.len() < directory_count {
        lines.push(format!(
            "  directory page: {} shown of {directory_count} (offset {offset}, limit {limit})",
            page.len()
        ));
        let next_offset = offset.saturating_add(page.len());
        if !page.is_empty() && next_offset < directory_count {
            lines.push(format!(
                "  next page: --offset {next_offset} --limit {limit}"
            ));
        }
    }
    append_secondary_languages(&mut lines, &walk, primary_lang);
    if walk.truncated {
        lines.push("  source walk capped; counts and directories are partial".into());
    }
    // Manifest — name, version, deps
    if let Some(manifest) = find_manifest(root) {
        if let Some(info) = parse_manifest(root, &manifest) {
            // Deps line
            if !info.deps.is_empty() {
                let dep_str = info.deps.join(", ");
                lines.push(format!("  deps: {dep_str}"));
            }

            // Hot files (only for projects with local imports)
            if let Some(hot) = hot_files(root, &walk, primary_lang) {
                lines.push(format!("  hot (× = importers): {hot}"));
            }

            // Git context
            if let Some(git) = git_context(root) {
                lines.push(format!("  git: {git}"));
            }

            // Test style
            if let Some(tests) = test_style(root, &walk, primary_lang) {
                lines.push(format!("  tests: {tests}"));
            }

            // Manifest line
            let mut manifest_line = format!("  manifest: {manifest}");
            if let Some(name) = &info.name {
                write!(manifest_line, " ({name}").unwrap();
                if let Some(version) = &info.version {
                    write!(manifest_line, " v{version}").unwrap();
                }
                manifest_line.push(')');
            }
            lines.push(manifest_line);
        }
    } else {
        // No manifest — still show hot, git, tests
        if let Some(hot) = hot_files(root, &walk, primary_lang) {
            lines.push(format!("  hot (× = importers): {hot}"));
        }
        if let Some(git) = git_context(root) {
            lines.push(format!("  git: {git}"));
        }
        if let Some(tests) = test_style(root, &walk, primary_lang) {
            lines.push(format!("  tests: {tests}"));
        }
    }

    lines.join("\n")
}

fn append_secondary_languages(
    lines: &mut Vec<String>,
    walk: &WalkResult,
    primary_lang: Option<Lang>,
) {
    // Language totals and their common source roots remain stable across directory pages.
    let mut secondary: Vec<_> = walk
        .lang_counts
        .iter()
        .filter(|(lang, _)| Some(**lang) != primary_lang)
        .collect();
    secondary.sort_by_key(|(lang, _)| lang_display_name(**lang));
    for (lang, count) in secondary {
        let directories: Vec<_> = walk
            .directory_lang_counts
            .iter()
            .filter(|(_, counts)| counts.contains_key(lang))
            .map(|(dir, _)| dir.as_str())
            .collect();
        let root = common_source_root(&directories);
        let location = root.map_or_else(String::new, |root| format!(" (under {root}/)"));
        lines.push(format!(
            "  {}: {count} source files{location}",
            lang_display_name(*lang)
        ));
    }
}

fn common_source_root(directories: &[&str]) -> Option<String> {
    let (first, rest) = directories.split_first()?;
    if *first == "." {
        return None;
    }
    let mut prefix: Vec<_> = first.split('/').collect();
    for directory in rest {
        let shared = prefix
            .iter()
            .zip(directory.split('/'))
            .take_while(|(left, right)| **left == *right)
            .count();
        prefix.truncate(shared);
        if prefix.is_empty() {
            return None;
        }
    }
    Some(prefix.join("/"))
}

fn directory_depth(dir: &str) -> usize {
    if dir == "." {
        0
    } else {
        dir.split('/').count()
    }
}

// ---------------------------------------------------------------------------
// Language display
// ---------------------------------------------------------------------------

fn lang_display_name(lang: Lang) -> &'static str {
    crate::lang::spec::spec(lang).display
}

// ---------------------------------------------------------------------------
// Source walk
// ---------------------------------------------------------------------------

struct WalkResult {
    lang_counts: HashMap<Lang, usize>,
    /// Source parent directories → per-language file counts
    directory_lang_counts: HashMap<String, HashMap<Lang, usize>>,
    /// Code files found: (path relative to root, size in bytes)
    code_files: Vec<(String, u64)>,
    /// Whether specific test dirs exist
    has_tests_dir: bool,
    has_test_dir: bool,
    has_dunder_tests: bool,
    has_spec_dir: bool,
    truncated: bool,
}

fn walk_files(root: &Path, recursive: bool) -> WalkResult {
    let mut result = WalkResult {
        lang_counts: HashMap::new(),
        directory_lang_counts: HashMap::new(),
        code_files: Vec::new(),
        has_tests_dir: false,
        has_test_dir: false,
        has_dunder_tests: false,
        has_spec_dir: false,
        truncated: false,
    };
    let start = Instant::now();
    let mut builder = crate::search::base_walk_builder(root);
    builder.follow_links(false);
    if !recursive {
        builder.max_depth(Some(3));
    }
    for (visited, entry) in builder.build().enumerate() {
        if visited >= 100_000 || start.elapsed() >= Duration::from_millis(150) {
            result.truncated = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            match entry.file_name().to_str() {
                Some("tests") => result.has_tests_dir = true,
                Some("test") => result.has_test_dir = true,
                Some("__tests__") => result.has_dunder_tests = true,
                Some("spec") => result.has_spec_dir = true,
                _ => {}
            }
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        if let FileType::Code(lang) = detect_file_type(path) {
            *result.lang_counts.entry(lang).or_insert(0) += 1;
            if let Ok(rel) = path.strip_prefix(root) {
                result.code_files.push((
                    rel.to_string_lossy().into_owned(),
                    entry.metadata().map_or(0, |m| m.len()),
                ));
                let parent = rel.parent().unwrap_or_else(|| Path::new(""));
                let key = if parent.as_os_str().is_empty() {
                    ".".to_owned()
                } else {
                    parent
                        .to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/")
                };
                *result
                    .directory_lang_counts
                    .entry(key)
                    .or_default()
                    .entry(lang)
                    .or_insert(0) += 1;
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Manifest parsing
// ---------------------------------------------------------------------------

fn find_manifest(root: &Path) -> Option<String> {
    const MANIFESTS: &[&str] = &["Cargo.toml", "package.json", "go.mod", "pyproject.toml"];
    for m in MANIFESTS {
        if root.join(m).exists() {
            return Some((*m).to_string());
        }
    }
    None
}

struct ManifestInfo {
    name: Option<String>,
    version: Option<String>,
    deps: Vec<String>,
}

fn parse_manifest(root: &Path, manifest: &str) -> Option<ManifestInfo> {
    match manifest {
        "Cargo.toml" => parse_cargo_toml(root),
        "package.json" => parse_package_json(root),
        "go.mod" => parse_go_mod(root),
        "pyproject.toml" => parse_pyproject_toml(root),
        _ => None,
    }
}

fn parse_cargo_toml(root: &Path) -> Option<ManifestInfo> {
    #[derive(Deserialize)]
    struct CargoToml {
        package: Option<Package>,
        dependencies: Option<toml::Table>,
    }
    #[derive(Deserialize)]
    struct Package {
        name: Option<String>,
        version: Option<String>,
    }

    let content = fs::read_to_string(root.join("Cargo.toml")).ok()?;
    let parsed: CargoToml = toml::from_str(&content).ok()?;
    let (name, version) = parsed.package.map_or((None, None), |p| (p.name, p.version));
    let mut deps: Vec<String> = parsed
        .dependencies
        .map(|d| d.into_iter().map(|(k, _)| k).collect())
        .unwrap_or_default();
    deps.sort();
    deps.truncate(10);
    Some(ManifestInfo {
        name,
        version,
        deps,
    })
}

fn parse_package_json(root: &Path) -> Option<ManifestInfo> {
    #[derive(Deserialize)]
    struct PackageJson {
        name: Option<String>,
        version: Option<String>,
        dependencies: Option<serde_json::Map<String, serde_json::Value>>,
    }

    let content = fs::read_to_string(root.join("package.json")).ok()?;
    let parsed: PackageJson = serde_json::from_str(&content).ok()?;
    let mut deps: Vec<String> = parsed
        .dependencies
        .map(|d| d.into_iter().map(|(k, _)| k).collect())
        .unwrap_or_default();
    deps.sort();
    deps.truncate(10);
    Some(ManifestInfo {
        name: parsed.name,
        version: parsed.version,
        deps,
    })
}

fn parse_go_mod(root: &Path) -> Option<ManifestInfo> {
    let content = fs::read_to_string(root.join("go.mod")).ok()?;
    let mut name = None;
    let mut deps: Vec<String> = Vec::new();
    let mut in_require = false;

    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("module ") {
            name = Some(rest.trim().to_string());
        }
        if trimmed == "require (" {
            in_require = true;
            continue;
        }
        if trimmed == ")" {
            in_require = false;
            continue;
        }
        if in_require {
            // e.g. "github.com/gin-gonic/gin v1.9.0"
            if let Some(dep) = trimmed.split_whitespace().next() {
                if !dep.starts_with("//") {
                    // Use short name (last segment of module path)
                    let short = dep.rsplit('/').next().unwrap_or(dep);
                    deps.push(short.to_string());
                }
            }
        }
    }

    deps.sort();
    deps.truncate(10);

    Some(ManifestInfo {
        name,
        version: None,
        deps,
    })
}

fn parse_pyproject_toml(root: &Path) -> Option<ManifestInfo> {
    #[derive(Default, Deserialize)]
    struct PyProject {
        project: Option<Project>,
    }
    #[derive(Default, Deserialize)]
    struct Project {
        name: Option<String>,
        version: Option<String>,
        dependencies: Option<Vec<String>>,
    }

    let content = fs::read_to_string(root.join("pyproject.toml")).ok()?;
    let parsed: PyProject = toml::from_str(&content).ok()?;
    let project = parsed.project.unwrap_or_default();
    let mut deps: Vec<String> = project
        .dependencies
        .unwrap_or_default()
        .into_iter()
        .filter_map(|spec| {
            let bare = spec
                .split(&['>', '<', '=', '~', '!', ';', '[', ' '][..])
                .next()?
                .trim();
            (!bare.is_empty()).then(|| bare.to_string())
        })
        .collect();
    deps.sort();
    deps.truncate(10);
    Some(ManifestInfo {
        name: project.name,
        version: project.version,
        deps,
    })
}

// ---------------------------------------------------------------------------
// Git context
// ---------------------------------------------------------------------------

/// Run a git command with a 200ms timeout. Returns None if it fails or times out.
/// Best-effort: every git failure (spawn error, non-zero exit, timeout, I/O error)
/// is intentionally swallowed into None — git context is a cosmetic fingerprint
/// and must never break the primary read/search path.
fn git_output(root: &Path, args: &[&str]) -> Option<String> {
    let mut child = Command::new("git")
        .args(args)
        .current_dir(root)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + Duration::from_millis(200);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let out = child.stdout.take()?;
                let s = std::io::read_to_string(out).ok()?;
                let trimmed = s.trim().to_string();
                return if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(_) => return None,
        }
    }
}

fn git_context(root: &Path) -> Option<String> {
    let branch = git_output(root, &["branch", "--show-current"])
        .or_else(|| git_output(root, &["rev-parse", "--short", "HEAD"]))?;

    let dirty_count = git_output(root, &["status", "--porcelain"]).map_or(0, |s| s.lines().count());

    let dirty_str = if dirty_count == 0 {
        "clean".to_string()
    } else {
        format!("{dirty_count} uncommitted files")
    };

    Some(format!("branch {branch}, {dirty_str}"))
}

// ---------------------------------------------------------------------------
// Test style detection
// ---------------------------------------------------------------------------

fn test_style(root: &Path, walk: &WalkResult, primary_lang: Option<Lang>) -> Option<String> {
    let mut styles: Vec<String> = Vec::new();

    // Directory-based test detection
    if walk.has_tests_dir {
        styles.push("tests/".to_string());
    }
    if walk.has_test_dir {
        styles.push("test/".to_string());
    }
    if walk.has_dunder_tests {
        styles.push("__tests__/".to_string());
    }
    if walk.has_spec_dir {
        styles.push("spec/".to_string());
    }

    // File pattern detection
    let has_test_files = walk
        .code_files
        .iter()
        .any(|(path, _)| path.contains(".test.") || path.contains(".spec."));
    let has_go_tests = walk
        .code_files
        .iter()
        .any(|(path, _)| path.ends_with("_test.go"));
    let has_py_tests = walk
        .code_files
        .iter()
        .any(|(path, _)| path.starts_with("test_") || path.contains("/test_"));

    if has_test_files && !walk.has_dunder_tests {
        styles.push("*.test/spec files".to_string());
    }
    if has_go_tests {
        styles.push("_test.go".to_string());
    }
    if has_py_tests {
        styles.push("test_*.py".to_string());
    }

    // Rust in-source test detection
    if primary_lang == Some(Lang::Rust) {
        let has_cfg_test = walk
            .code_files
            .iter()
            .filter(|(path, _)| {
                Path::new(path)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("rs"))
            })
            .take(5)
            .any(|(path, _)| {
                let full = root.join(path);
                fs::read_to_string(&full).is_ok_and(|content| content.contains("#[cfg(test)]"))
            });
        if has_cfg_test {
            styles.push("in-source #[cfg(test)]".to_string());
        }
    }

    if styles.is_empty() {
        None
    } else {
        Some(styles.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Hot files — most imported local files
// ---------------------------------------------------------------------------

fn hot_files(root: &Path, walk: &WalkResult, primary_lang: Option<Lang>) -> Option<String> {
    let lang = primary_lang?; // require a detected language
    let start = Instant::now();

    // Sort by size (smallest first) and take first 100
    let mut files: Vec<&(String, u64)> = walk.code_files.iter().collect();
    files.sort_by_key(|(_, size)| *size);
    files.truncate(100);

    // Use resolve_related_files to get real file paths for imports.
    // Count how many files import each target path.
    let mut path_counts: HashMap<std::path::PathBuf, usize> = HashMap::new();
    // Also collect all import source lines for symbol extraction later
    let mut all_import_sources: Vec<String> = Vec::new();

    for (rel_path, _) in &files {
        if start.elapsed().as_millis() > 100 {
            break;
        }
        let full = root.join(rel_path);
        let Ok(content) = fs::read_to_string(&full) else {
            continue;
        };

        // Resolve imports to actual file paths using the proven import resolver
        let resolved = crate::read::imports::resolve_related_files_with_content(&full, &content);
        for target_path in resolved {
            *path_counts.entry(target_path).or_insert(0) += 1;
        }

        // Collect import source strings for symbol extraction
        for line in content.lines() {
            if is_import_line(line, lang) {
                let source = crate::lang::outline::extract_import_source(line, Some(lang));
                if !source.is_empty() && !crate::read::imports::is_external(&source, lang) {
                    all_import_sources.push(source);
                }
            }
        }
    }

    if path_counts.is_empty() {
        return None;
    }

    // Sort by import count descending, take top 5
    let mut sorted: Vec<(std::path::PathBuf, usize)> = path_counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    sorted.truncate(5);

    if sorted[0].1 < 2 {
        return None;
    }

    // For each hot file, find the most commonly imported symbol by scanning
    // import sources that reference this file's module name.
    let parts: Vec<String> = sorted
        .iter()
        .filter(|(_, count)| *count >= 2)
        .map(|(path, count)| {
            let rel = path.strip_prefix(root).unwrap_or(path);
            let rel_str = rel.display().to_string();

            // Derive the module name from the file path
            // src/types.rs → "types", src/lang/mod.rs → "lang", src/error.rs → "error"
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            let module_name = if stem == "mod" || stem == "index" || stem == "__init__" {
                path.parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or(stem)
            } else {
                stem
            };

            // Count symbols imported from this module across all import sources
            let mut symbol_counts: HashMap<String, usize> = HashMap::new();
            for source in &all_import_sources {
                // Match imports that reference this module
                // e.g., for module "types": "crate::types::OutlineEntry" matches
                let segments: Vec<&str> = source.split("::").collect();
                if let Some(mod_pos) = segments.iter().position(|s| *s == module_name) {
                    // Everything after the module name is a symbol path
                    for &sym in segments.iter().skip(mod_pos + 1) {
                        if !sym.is_empty()
                            && !sym.contains('*')
                            && !sym.contains('{')
                            && sym != "self"
                        {
                            *symbol_counts.entry(sym.to_string()).or_insert(0) += 1;
                        }
                    }
                }
            }

            // Pick the most frequently imported symbol (break ties alphabetically for determinism)
            let top_sym = symbol_counts
                .into_iter()
                .max_by(|(a_sym, a_c), (b_sym, b_c)| a_c.cmp(b_c).then(b_sym.cmp(a_sym)))
                .map(|(sym, _)| sym);

            if let Some(sym) = top_sym {
                format!("{rel_str}({sym}) ×{count}")
            } else {
                format!("{rel_str} ×{count}")
            }
        })
        .collect();

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fingerprint_on_tilth() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let output = fingerprint(root);

        assert!(!output.is_empty(), "fingerprint should not be empty");
        assert!(
            output.contains("Rust"),
            "should detect Rust as primary language"
        );
        assert!(output.contains("Cargo.toml"), "should detect manifest");
        assert!(output.contains("tilth"), "should find project name");

        // Token budget: output should be compact
        let estimated_tokens = output.len() / 4;
        assert!(
            estimated_tokens < 300,
            "fingerprint should be <300 tokens, got {estimated_tokens}"
        );
    }

    #[test]
    fn test_fingerprint_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let output = fingerprint(tmp.path());

        // Empty dir: should produce minimal output or empty
        // With 0 files and 0 modules, the header will say "0 source files"
        // but that's fine — it's still useful context
        assert!(
            output.is_empty() || output.contains("0 source files"),
            "empty dir should produce empty or minimal output, got: {output}"
        );
    }

    #[test]
    fn test_manifest_parsing() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let info = parse_cargo_toml(root).expect("should parse Cargo.toml");

        assert_eq!(info.name.as_deref(), Some("tilth"));
        assert!(info.version.is_some(), "should have a version");
        assert!(
            info.deps.iter().any(|d| d == "clap"),
            "deps should include clap: {:?}",
            info.deps
        );
        assert!(
            info.deps.iter().any(|d| d == "dashmap"),
            "deps should include dashmap: {:?}",
            info.deps
        );
    }
}
