pub mod format;
pub mod matching;
pub mod overlay;
pub mod parse;

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use rayon::prelude::*;

use crate::types::OutlineKind;

#[derive(Debug)]
pub enum DiffSource {
    GitUncommitted,
    GitStaged,
    GitRef(String),
    Files(PathBuf, PathBuf),
    Patch(PathBuf),
    Log(String),
}

#[derive(Debug)]
pub struct FileDiff {
    pub path: PathBuf,
    pub old_path: Option<PathBuf>,
    pub status: FileStatus,
    pub hunks: Vec<Hunk>,
    pub is_generated: bool,
    pub is_binary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug)]
pub struct Hunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug)]
pub struct DiffLine {
    /// Actual source line for rendered attribution (old side for removals).
    pub line: Option<u32>,
    pub kind: DiffLineKind,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug)]
pub struct DiffSymbol {
    pub entry: crate::types::OutlineEntry,
    pub identity: SymbolIdentity,
    pub content_hash: u64,
    pub structural_hash: u64,
    pub signature_hash: u64,
    pub source_text: String,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct SymbolIdentity {
    pub kind: OutlineKind,
    pub parent_path: String,
    pub name: String,
}

#[derive(Debug)]
pub struct SymbolChange {
    pub name: String,
    pub kind: OutlineKind,
    pub change: ChangeType,
    pub match_confidence: MatchConfidence,
    pub line: u32,
    pub old_line: Option<u32>,
    pub structural_hash: u64,
    pub old_sig: Option<String>,
    pub new_sig: Option<String>,
    pub size_delta: Option<(u32, u32)>,
}

/// Internal attribution key: overloads and same-named methods stay distinct.
pub(crate) fn symbol_key(change: &SymbolChange) -> String {
    format!("{}@{}", change.name, change.line)
}

#[derive(Debug, Clone)]
pub enum ChangeType {
    Added,
    Deleted,
    BodyChanged,
    SignatureChanged,
    Renamed { old_name: String },
    Moved { old_path: PathBuf },
    RenamedAndMoved { old_name: String, old_path: PathBuf },
    Unchanged,
}

#[derive(Debug, Clone)]
pub enum MatchConfidence {
    Exact,
    Structural,
    Fuzzy(f32),
    Ambiguous(u32),
}

#[derive(Debug)]
pub struct FileOverlay {
    pub path: PathBuf,
    pub symbol_changes: Vec<SymbolChange>,
    pub attributed_hunks: Vec<(String, Vec<DiffLine>)>,
    pub conflicts: Vec<Conflict>,
    pub new_content: Option<String>,
}

#[derive(Debug)]
pub struct Conflict {
    pub line: u32,
    pub ours: String,
    pub theirs: String,
    pub enclosing_fn: Option<String>,
}

#[derive(Debug)]
pub struct CommitSummary {
    pub hash: String,
    pub timestamp: i64,
    pub message: String,
    pub author: String,
    pub overlays: Vec<FileOverlay>,
}

/// Resolve the diff source from CLI/MCP parameters.
///
/// Priority: patch > log > a+b > source > default (uncommitted).
/// Returns an error if only one of `a` or `b` is provided.
pub fn resolve_source(
    source: Option<&str>,
    a: Option<&str>,
    b: Option<&str>,
    patch: Option<&str>,
    log: Option<&str>,
) -> Result<DiffSource, String> {
    if let Some(p) = patch {
        return Ok(DiffSource::Patch(PathBuf::from(p)));
    }
    if let Some(l) = log {
        return Ok(DiffSource::Log(l.to_string()));
    }
    match (a, b) {
        (Some(fa), Some(fb)) => return Ok(DiffSource::Files(PathBuf::from(fa), PathBuf::from(fb))),
        (Some(_), None) | (None, Some(_)) => {
            return Err("both --a and --b must be provided together".to_string());
        }
        (None, None) => {}
    }
    if let Some(s) = source {
        let ds = match s {
            "staged" => DiffSource::GitStaged,
            "uncommitted" | "working" => DiffSource::GitUncommitted,
            r => DiffSource::GitRef(r.to_string()),
        };
        return Ok(ds);
    }
    Ok(DiffSource::GitUncommitted)
}

/// Execute a git diff command and return raw unified diff output.
///
/// Git runs inside `repo` when one is provided (the caller's checkout);
/// otherwise in the process cwd, exactly as before `repo` existed.
fn run_git_diff_scoped(
    source: &DiffSource,
    repo: Option<&Path>,
    scope: Option<&str>,
) -> Result<String, String> {
    run_git_diff_query(source, repo, scope, &[], &[])
}

fn run_git_diff_query(
    source: &DiffSource,
    repo: Option<&Path>,
    scope: Option<&str>,
    extra: &[String],
    paths: &[PathBuf],
) -> Result<String, String> {
    use std::process::Command;

    match source {
        DiffSource::Log(_) => {
            return Err("log mode should not call run_git_diff directly".to_string());
        }
        DiffSource::Patch(path) => {
            let content = std::fs::read_to_string(path)
                .map_err(|e| format!("failed to read patch file: {e}"))?;
            return Ok(content);
        }
        _ => {}
    }

    let mut cmd = Command::new("git");
    if let Some(dir) = repo {
        cmd.current_dir(dir);
    }
    // Pin the output shape. User git config can otherwise swap in an external
    // diff tool, colour the patch, drop or rename the `a/` `b/` prefixes,
    // make paths cwd-relative, or blank out empty context lines — every one
    // of which the parser misreads as "no changes" or a wrong path (#208).
    cmd.args(["-c", "core.quotePath=false"]);
    cmd.args(["-c", "diff.suppressBlankEmpty=false"]);
    cmd.arg("diff");
    cmd.args([
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
    ]);
    cmd.args(extra);

    match source {
        DiffSource::GitUncommitted => {
            // working tree vs HEAD (unstaged + staged)
            cmd.arg("HEAD");
        }
        DiffSource::GitStaged => {
            cmd.arg("--staged");
        }
        DiffSource::GitRef(r) => {
            cmd.arg(r);
        }
        DiffSource::Files(fa, fb) => {
            cmd.arg("--no-index").arg("--").arg(fa).arg(fb);
        }
        // Patch and Log are handled above
        DiffSource::Patch(_) | DiffSource::Log(_) => unreachable!(),
    }

    if !matches!(source, DiffSource::Files(..)) {
        if !paths.is_empty() {
            cmd.arg("--");
            for path in paths {
                cmd.arg(format!(":(literal){}", path.display()));
            }
        } else if let Some(scope) = scope {
            let paths = scoped_git_paths(source, repo, scope)?;
            if paths.is_empty() {
                return Ok(String::new());
            }
            cmd.arg("--");
            for path in paths {
                cmd.arg(format!(":(literal){}", path.display()));
            }
        }
    }

    let output = cmd
        .output()
        .map_err(|e| format!("failed to run git diff: {e}"))?;

    // `git diff --no-index` exits 1 when the files differ; every other variant
    // exits 0 on success. Anything else is a git error, and parsing its empty
    // stdout would report "No changes." for a diff that was never produced.
    let ok = match source {
        DiffSource::Files(..) => matches!(output.status.code(), Some(0 | 1)),
        _ => output.status.success(),
    };
    if !ok {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "git diff failed ({}): {}",
            output.status,
            stderr.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Select changed paths with a cheap name/status query before requesting patches.
/// Include both sides of renames so old-path scopes keep the rename.
fn scoped_git_paths(
    source: &DiffSource,
    repo: Option<&Path>,
    scope: &str,
) -> Result<Vec<PathBuf>, String> {
    let mut cmd = Command::new("git");
    if let Some(dir) = repo {
        cmd.current_dir(dir);
    }
    cmd.args([
        "-c",
        "core.quotePath=false",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--no-relative",
        "--name-status",
        "-z",
    ]);
    match source {
        DiffSource::GitUncommitted => {
            cmd.arg("HEAD");
        }
        DiffSource::GitStaged => {
            cmd.arg("--staged");
        }
        DiffSource::GitRef(reference) => {
            cmd.arg(reference);
        }
        _ => unreachable!(),
    }
    let output = cmd
        .output()
        .map_err(|e| format!("failed to run git diff: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "git diff failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    let fields: Vec<&[u8]> = output.stdout.split(|byte| *byte == 0).collect();
    let mut changes = Vec::new();
    let mut i = 0;
    while i + 1 < fields.len() {
        let status = fields[i];
        i += 1;
        let old = PathBuf::from(String::from_utf8_lossy(fields[i]).into_owned());
        i += 1;
        let new = if status
            .first()
            .is_some_and(|kind| matches!(kind, b'R' | b'C'))
            && i < fields.len()
        {
            let path = PathBuf::from(String::from_utf8_lossy(fields[i]).into_owned());
            i += 1;
            Some(path)
        } else {
            None
        };
        changes.push((old, new));
    }

    let has_changes = !changes.is_empty();
    let directory = directory_scope_path(scope, repo).and_then(|(path, exists)| {
        (exists
            || scope.ends_with('/')
            || changes.iter().any(|(old, new)| {
                is_path_descendant(old, &path)
                    || new
                        .as_deref()
                        .is_some_and(|new| is_path_descendant(new, &path))
            }))
        .then_some(path)
    });
    let file = scope.split_once(':').map_or(scope, |(file, _)| file);
    let mut paths = Vec::new();
    for (old, new) in changes {
        let matches = |path: &Path| {
            if let Some(directory) = &directory {
                path.starts_with(directory)
            } else {
                let path = path.to_string_lossy();
                path == file || path.ends_with(file)
            }
        };
        if matches(&old) || new.as_deref().is_some_and(matches) {
            paths.push(old);
            if let Some(new) = new {
                paths.push(new);
            }
        }
    }
    if paths.is_empty() && has_changes && directory.is_none() {
        return Err(format!("file '{file}' not found in diff"));
    }
    paths.sort_unstable();
    paths.dedup();
    Ok(paths)
}

/// Full diff orchestrator — parse → overlay → format pipeline.
///
/// `repo` anchors every git command and working-tree read to the caller's
/// checkout. `None` keeps the process-cwd behavior (CLI, or an MCP call with
/// no `root` arg).
pub fn diff(
    source: &DiffSource,
    repo: Option<&Path>,
    scope: Option<&str>,
    search: Option<&str>,
    blast: bool,
    _expand: usize,
    budget: Option<u64>,
) -> Result<String, String> {
    // Git emits repository-relative paths even when invoked in a subdirectory.
    // Anchor both blob and working-tree reads to the same repository root.
    let git_root = if matches!(
        source,
        DiffSource::GitUncommitted
            | DiffSource::GitStaged
            | DiffSource::GitRef(_)
            | DiffSource::Log(_)
    ) {
        let mut cmd = Command::new("git");
        if let Some(repo) = repo {
            cmd.current_dir(repo);
        }
        let output = cmd
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .map_err(|e| format!("failed to locate repository: {e}"))?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        Some(PathBuf::from(
            String::from_utf8_lossy(&output.stdout).trim(),
        ))
    } else {
        None
    };
    let repo = git_root.as_deref().or(repo);
    let normalized_scope = scope.map(|scope| {
        let (path, symbol) = scope
            .split_once(':')
            .map_or((scope, None), |(path, symbol)| (path, Some(symbol)));
        let path = Path::new(path);
        let path = repo
            .and_then(|root| path.strip_prefix(root).ok())
            .unwrap_or(path);
        let path: PathBuf = path
            .components()
            .filter(|c| !matches!(c, std::path::Component::CurDir))
            .collect();
        let path = if path.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            path
        };
        let path = path.to_string_lossy();
        symbol.map_or_else(|| path.to_string(), |symbol| format!("{path}:{symbol}"))
    });
    let scope = normalized_scope.as_deref();
    // Log mode has its own pipeline.
    if let DiffSource::Log(range) = source {
        return diff_log(range, repo, scope, budget);
    }

    let raw = run_git_diff_scoped(source, repo, scope)?;
    if raw.is_empty() {
        if let Some(scope) = scope {
            let path = scope.split_once(':').map_or(scope, |(path, _)| path);
            let disk = repo.map_or_else(|| PathBuf::from(path), |repo| repo.join(path));
            if !disk.exists() && !scope.ends_with('/') {
                return Err(format!("file '{path}' not found in diff"));
            }
        }
        return Ok("No changes.".to_string());
    }

    // 1. Parse raw unified diff.
    let mut file_diffs = parse::parse_unified_diff(&raw);
    let directory_scope = scope
        .and_then(|scope| directory_scope_path(scope, repo))
        .and_then(|(directory, exists)| {
            (exists
                || scope.is_some_and(|scope| scope.ends_with('/'))
                || file_diffs_have_descendant_paths(&file_diffs, &directory))
            .then_some(directory)
        });
    if let Some(scope) = scope {
        let path = scope.split_once(':').map_or(scope, |(path, _)| path);
        file_diffs.retain(|fd| {
            let matches = |file: &Path| {
                directory_scope.as_ref().map_or_else(
                    || file.to_string_lossy().ends_with(path),
                    |directory| file.starts_with(directory),
                )
            };
            matches(&fd.path) || fd.old_path.as_deref().is_some_and(matches)
        });
    }
    if file_diffs.is_empty() {
        return Ok("No changes.".to_string());
    }

    // 2. Build structural overlays in parallel — each FileDiff is independent
    // and `compute_overlay` constructs its own tree-sitter parser per call
    // (see `lang::outline::get_outline_entries`), so no shared mutable state
    // crosses worker boundaries.
    let mut overlays: Vec<FileOverlay> = file_diffs
        .par_iter()
        .map(|fd| overlay::compute_overlay(fd, source, repo))
        .collect();

    // 3. Discover a bounded set of move candidates by changed-line names.
    // Git returns only paths here; never fetch an unscoped patch as fallback.
    let move_warning = if scope.is_some() {
        extend_move_candidates(&mut overlays, source, repo)?
    } else {
        None
    };
    overlay::cross_file_matching(&mut overlays);
    let selected_paths: HashSet<_> = file_diffs.iter().map(|fd| fd.path.clone()).collect();
    overlays.retain(|overlay| selected_paths.contains(&overlay.path));
    if let Some(directory) = &directory_scope {
        overlays.retain(|overlay| overlay_matches_directory_scope(overlay, &file_diffs, directory));
    }

    // 4. Signature warnings.
    let mut warnings = overlay::signature_warnings(&overlays);
    if let Some(warning) = move_warning {
        warnings.push(warning);
    }

    // 5. Search filter.
    if let Some(term) = search {
        filter_by_search(&mut overlays, term);
        if overlays.is_empty() {
            return Ok(format!("No changes matching '{term}'."));
        }
    }

    // 6. Blast radius.
    if blast {
        let mut blast_warnings = compute_blast(&overlays, repo);
        warnings.append(&mut blast_warnings);
    }

    // 7. Build file_meta parallel to overlays.
    let file_meta: Vec<(&Path, bool, bool)> = overlays
        .iter()
        .map(|o| {
            // Find the original FileDiff for this overlay to get is_generated/is_binary.
            let fd = file_diffs.iter().find(|fd| fd.path == o.path);
            let (is_generated, is_binary) =
                fd.map_or((false, false), |f| (f.is_generated, f.is_binary));
            (o.path.as_path(), is_generated, is_binary)
        })
        .collect();

    // 8. Format based on scope.
    let label = source_label(source);
    let mut output = match scope {
        None => format::format_overview(&overlays, &file_meta, &warnings, &label, budget),
        Some(_) if directory_scope.is_some() => {
            format::format_overview(&overlays, &file_meta, &warnings, &label, budget)
        }
        Some(s) if s.contains(':') => {
            // file:function scope
            let (file_part, fn_name) = s.split_once(':').unwrap();
            match overlays.iter().find(|o| {
                let p = o.path.to_string_lossy();
                p == file_part
                    || p.ends_with(file_part)
                    || file_diffs.iter().any(|fd| {
                        fd.path == o.path
                            && fd
                                .old_path
                                .as_ref()
                                .is_some_and(|old| old.to_string_lossy().ends_with(file_part))
                    })
            }) {
                Some(o) => format::format_function_detail(o, fn_name),
                None => return Err(format!("file '{file_part}' not found in diff")),
            }
        }
        Some(file) => {
            match overlays.iter().find(|o| {
                let p = o.path.to_string_lossy();
                p == file
                    || p.ends_with(file)
                    || file_diffs.iter().any(|fd| {
                        fd.path == o.path
                            && fd
                                .old_path
                                .as_ref()
                                .is_some_and(|old| old.to_string_lossy().ends_with(file))
                    })
            }) {
                Some(o) => format::format_file_detail(o, budget),
                None if file == "." || overlays.iter().any(|o| o.path.starts_with(file)) => {
                    format::format_overview(&overlays, &file_meta, &warnings, &label, budget)
                }
                None => return Err(format!("file '{file}' not found in diff")),
            }
        }
    };

    if scope.is_some_and(|scope| {
        overlays
            .iter()
            .any(|o| o.path == Path::new(scope.split_once(':').map_or(scope, |(path, _)| path)))
    }) {
        for warning in &warnings {
            output.push('\n');
            output.push_str(warning);
            output.push('\n');
        }
    }

    // 9. Conflict detection for uncommitted diffs.
    if matches!(source, DiffSource::GitUncommitted) {
        let mut all_conflicts = Vec::new();
        for overlay in &overlays {
            let conflicts = overlay::detect_conflicts(&overlay.path, repo);
            if !conflicts.is_empty() {
                all_conflicts.push((&overlay.path, conflicts));
            }
        }
        if !all_conflicts.is_empty() {
            for (path, conflicts) in &all_conflicts {
                output.push('\n');
                output.push_str(&format::format_conflicts(conflicts, path));
            }
            if let Some(b) = budget {
                output = crate::budget::apply(&output, b);
            }
        }
    }

    Ok(output)
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Resolve a directory scope to a path relative to its repository root.
fn directory_scope_path(scope: &str, repo: Option<&Path>) -> Option<(PathBuf, bool)> {
    let scope_path = Path::new(scope);
    if scope.contains(':') && !scope_path.is_absolute() {
        return None;
    }

    let root = if let Some(repo) = repo {
        std::fs::canonicalize(repo).ok()?
    } else {
        let cwd = std::env::current_dir().ok()?;
        let git_root = Command::new("git")
            .current_dir(&cwd)
            .args(["rev-parse", "--show-toplevel"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| {
                let root = String::from_utf8_lossy(&output.stdout);
                std::fs::canonicalize(root.trim()).ok()
            });
        git_root.unwrap_or(cwd)
    };

    let absolute_scope = if scope_path.is_absolute() {
        scope_path.to_path_buf()
    } else {
        root.join(scope_path)
    };

    match std::fs::canonicalize(&absolute_scope) {
        Ok(absolute_scope) => {
            if !absolute_scope.is_dir() {
                return None;
            }
            let relative_scope = absolute_scope.strip_prefix(&root).ok()?.to_path_buf();
            Some((relative_scope, true))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let absolute_scope = normalize_absolute_scope(&absolute_scope)?;
            let mut ancestor = absolute_scope.as_path();
            let canonical_ancestor = loop {
                match std::fs::canonicalize(ancestor) {
                    Ok(path) => break path,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        ancestor = ancestor.parent()?;
                    }
                    Err(_) => return None,
                }
            };
            if !canonical_ancestor.is_dir() {
                return None;
            }

            let missing = absolute_scope.strip_prefix(ancestor).ok()?;
            let mut resolved_scope = canonical_ancestor;
            resolved_scope.push(missing);
            let relative_scope = normalize_absolute_scope(&resolved_scope)?
                .strip_prefix(&root)
                .ok()?
                .to_path_buf();
            Some((relative_scope, false))
        }
        Err(_) => None,
    }
}

fn normalize_absolute_scope(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                } else {
                    return None;
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Some(normalized)
}

fn file_diffs_have_descendant_paths(file_diffs: &[FileDiff], directory: &Path) -> bool {
    file_diffs.iter().any(|file_diff| {
        is_path_descendant(&file_diff.path, directory)
            || file_diff
                .old_path
                .as_deref()
                .is_some_and(|old_path| is_path_descendant(old_path, directory))
    })
}

fn is_path_descendant(path: &Path, directory: &Path) -> bool {
    path != directory && path.starts_with(directory)
}

fn overlay_matches_directory_scope(
    overlay: &FileOverlay,
    file_diffs: &[FileDiff],
    directory: &Path,
) -> bool {
    overlay.path.starts_with(directory)
        || file_diffs.iter().any(|file_diff| {
            file_diff.path == overlay.path
                && file_diff
                    .old_path
                    .as_deref()
                    .is_some_and(|old_path| old_path.starts_with(directory))
        })
}

/// Extend scoped overlays with a bounded set of possible move counterparts.
fn extend_move_candidates(
    overlays: &mut Vec<FileOverlay>,
    source: &DiffSource,
    repo: Option<&Path>,
) -> Result<Option<String>, String> {
    const CANDIDATE_LIMIT: usize = 64;
    if !matches!(
        source,
        DiffSource::GitUncommitted | DiffSource::GitStaged | DiffSource::GitRef(_)
    ) {
        return Ok(None);
    }
    let mut names: Vec<_> = overlays
        .iter()
        .flat_map(|o| &o.symbol_changes)
        .filter(|c| matches!(c.change, ChangeType::Added | ChangeType::Deleted))
        .filter(|c| !matches!(c.kind, OutlineKind::Import | OutlineKind::Export))
        .map(|c| c.name.rsplit("::").next().unwrap_or(&c.name).to_string())
        .collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return Ok(None);
    }
    let limited = || {
        Some(format!("warning: cross-scope move detection skipped (more than {CANDIDATE_LIMIT} candidate names or files)"))
    };
    if names.len() > CANDIDATE_LIMIT {
        return Ok(limited());
    }
    let escaped: Vec<String> = names
        .iter()
        .map(|name| {
            name.chars().fold(String::new(), |mut out, ch| {
                if ".[](){}?*+^$|\\".contains(ch) {
                    out.push('\\');
                }
                out.push(ch);
                out
            })
        })
        .collect();
    let pattern = format!(
        "(^|[^[:alnum:]_$])({})($|[^[:alnum:]_$])",
        escaped.join("|")
    );
    let raw = run_git_diff_query(
        source,
        repo,
        None,
        &["--name-only".into(), "-z".into(), format!("-G{pattern}")],
        &[],
    )?;
    let paths: Vec<_> = raw
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .filter(|path| !overlays.iter().any(|overlay| &overlay.path == path))
        .collect();
    if paths.len() > CANDIDATE_LIMIT {
        return Ok(limited());
    }
    if !paths.is_empty() {
        let raw = run_git_diff_query(source, repo, None, &[], &paths)?;
        let mut files = parse::parse_unified_diff(&raw);
        files.retain(|file| !overlays.iter().any(|overlay| overlay.path == file.path));
        overlays.extend(
            files
                .par_iter()
                .map(|file| overlay::compute_overlay(file, source, repo))
                .collect::<Vec<_>>(),
        );
    }
    Ok(None)
}

/// Human-readable label for a diff source.
fn source_label(source: &DiffSource) -> String {
    match source {
        DiffSource::GitUncommitted => "uncommitted".to_string(),
        DiffSource::GitStaged => "staged".to_string(),
        DiffSource::GitRef(r) => r.clone(),
        DiffSource::Files(a, b) => format!("{} vs {}", a.display(), b.display()),
        DiffSource::Patch(p) => format!("patch: {}", p.display()),
        DiffSource::Log(r) => format!("log: {r}"),
    }
}

/// Filter overlays to only symbols whose diff lines contain the search term
/// (case-insensitive substring match). Removes files with no matches.
fn filter_by_search(overlays: &mut Vec<FileOverlay>, term: &str) {
    let lower_term = term.to_lowercase();

    overlays.retain_mut(|overlay| {
        // Keep symbol changes that have matching diff lines.
        let matching_symbols: HashSet<String> = overlay
            .attributed_hunks
            .iter()
            .filter(|(_, lines)| {
                lines
                    .iter()
                    .any(|l| l.content.to_lowercase().contains(&lower_term))
            })
            .map(|(name, _)| name.clone())
            .collect();

        // Also match on symbol names themselves.
        let matching_names: HashSet<String> = overlay
            .symbol_changes
            .iter()
            .filter(|c| c.name.to_lowercase().contains(&lower_term))
            .map(symbol_key)
            .collect();

        let all_matching: HashSet<String> =
            matching_symbols.union(&matching_names).cloned().collect();

        if all_matching.is_empty() {
            return false;
        }

        overlay
            .symbol_changes
            .retain(|c| all_matching.contains(&symbol_key(c)));
        overlay
            .attributed_hunks
            .retain(|(name, _)| all_matching.contains(name));

        true
    });
}

/// Find callers of signature-changed symbols and return warnings.
///
/// The caller search runs over `repo` when provided — searching the process
/// cwd for a diff anchored elsewhere would count the wrong checkout's callers.
fn compute_blast(overlays: &[FileOverlay], repo: Option<&Path>) -> Vec<String> {
    let sig_changed: HashSet<String> = overlays
        .iter()
        .flat_map(|o| o.symbol_changes.iter())
        .filter(|c| matches!(c.change, ChangeType::SignatureChanged))
        .map(|c| c.name.rsplit("::").next().unwrap_or(&c.name).to_string())
        .collect();

    if sig_changed.is_empty() {
        return Vec::new();
    }

    let scope = match repo {
        Some(r) => r.to_path_buf(),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let bloom = crate::index::bloom::BloomFilterCache::new();

    match crate::search::callers::find_callers_batch(&sig_changed, &scope, &bloom, None, usize::MAX)
    {
        Ok(matches) => {
            let mut counts: std::collections::HashMap<String, usize> =
                std::collections::HashMap::new();
            for (target, _) in &matches {
                *counts.entry(target.clone()).or_default() += 1;
            }
            let mut warnings: Vec<_> = counts
                .into_iter()
                .map(|(name, count)| {
                    format!(
                        "blast: `{name}` signature changed — {count} caller{} may need updating",
                        if count == 1 { "" } else { "s" }
                    )
                })
                .collect();
            warnings.sort();
            warnings
        }
        Err(_) => Vec::new(),
    }
}

/// Log mode pipeline: run per-commit diffs and format as commit summaries.
fn diff_log(
    range: &str,
    repo: Option<&Path>,
    scope: Option<&str>,
    budget: Option<u64>,
) -> Result<String, String> {
    // Get commit list.
    let mut cmd = Command::new("git");
    if let Some(dir) = repo {
        cmd.current_dir(dir);
    }
    let output = cmd
        .args(["log", "--format=%H %at %s%x00%an", range])
        .output()
        .map_err(|e| format!("failed to run git log: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git log failed: {stderr}"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut summaries: Vec<CommitSummary> = Vec::new();
    let scope_path = scope.and_then(|scope| directory_scope_path(scope, repo));

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Format: "<hash> <timestamp> <subject>\0<author>"
        let Some((rest, author)) = line.split_once('\0') else {
            continue;
        };

        let mut parts = rest.splitn(3, ' ');
        let Some(hash) = parts.next() else {
            continue;
        };
        let timestamp: i64 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let message = parts.next().unwrap_or("").to_string();

        // Run diff for this commit.
        let ref_str = format!("{hash}^..{hash}");
        let commit_source = DiffSource::GitRef(ref_str);
        let raw = match run_git_diff_scoped(&commit_source, repo, scope) {
            Ok(raw) => raw,
            Err(error) if scope.is_some() && error.ends_with("not found in diff") => continue,
            Err(error) => return Err(error),
        };
        let file_diffs = parse::parse_unified_diff(&raw);

        let mut overlays: Vec<FileOverlay> = file_diffs
            .iter()
            .map(|fd| overlay::compute_overlay(fd, &commit_source, repo))
            .collect();
        overlay::cross_file_matching(&mut overlays);
        let directory_scope = scope_path.as_ref().and_then(|(directory, exists)| {
            if *exists || file_diffs_have_descendant_paths(&file_diffs, directory) {
                Some(directory.as_path())
            } else {
                None
            }
        });
        if let Some(directory) = directory_scope {
            overlays
                .retain(|overlay| overlay_matches_directory_scope(overlay, &file_diffs, directory));
        } else if let Some(file_scope) = scope {
            overlays.retain(|overlay| {
                let path = overlay.path.to_string_lossy();
                path == file_scope
                    || path.ends_with(file_scope)
                    || file_diffs.iter().any(|fd| {
                        fd.path == overlay.path
                            && fd
                                .old_path
                                .as_ref()
                                .is_some_and(|old| old.to_string_lossy().ends_with(file_scope))
                    })
            });
        }

        summaries.push(CommitSummary {
            hash: hash.to_string(),
            timestamp,
            message,
            author: author.to_string(),
            overlays,
        });
    }

    if scope.is_some() {
        summaries.retain(|s| !s.overlays.is_empty());
    }

    if summaries.is_empty() {
        return Ok("No commits found.".to_string());
    }

    Ok(format::format_log(&summaries, range, budget))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Serializes tests that mutate the process-global cwd. Crate-wide on purpose:
/// two module-local locks cannot protect against each other, and the MCP tool
/// tests (`mcp::tools::diff`) pin cwd too.
#[cfg(test)]
pub(crate) static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    /// Create a test git repo with an initial commit containing a Rust file.
    fn setup_test_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("failed to create tempdir");
        let p = dir.path();

        git(p, &["init"]);
        git(p, &["config", "user.email", "test@test.com"]);
        git(p, &["config", "user.name", "Test"]);

        let src = p.join("src");
        fs::create_dir_all(&src).unwrap();

        let main_rs = src.join("main.rs");
        fs::write(
            &main_rs,
            "fn hello() {\n    println!(\"hello\");\n}\n\nfn goodbye() {\n    println!(\"bye\");\n}\n\nfn main() {\n    hello();\n    goodbye();\n}\n",
        )
        .unwrap();

        git(p, &["add", "-A"]);
        git(p, &["commit", "-m", "initial"]);

        dir
    }

    /// Run a git command in the given directory.
    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@test.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@test.com")
            .output()
            .expect("failed to run git");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// Run `diff()` from within the test repo directory, serialized via `CWD_LOCK`.
    /// Passes `repo: None` on purpose — these tests pin the default cwd flow.
    fn run_diff_in(
        dir: &Path,
        source: &DiffSource,
        scope: Option<&str>,
        search: Option<&str>,
        blast: bool,
        budget: Option<u64>,
    ) -> Result<String, String> {
        let _lock = CWD_LOCK.lock().unwrap();
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(dir).unwrap();
        let result = diff(source, None, scope, search, blast, 0, budget);
        std::env::set_current_dir(&prev).unwrap();
        result
    }

    // 1. test_empty_diff
    #[test]
    fn test_empty_diff() {
        let dir = setup_test_repo();
        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert_eq!(result, "No changes.");
    }

    #[test]
    fn scoped_typescript_detail_uses_both_sides_once() {
        let dir = setup_test_repo();
        let path = dir.path().join("src/service.ts");
        let before = "export class A {\n  run() {\n    return 1;\n  }\n}\nexport class B {\n  run() {\n    return 10;\n  }\n}\n";
        fs::write(&path, before).unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-m", "baseline typescript"]);
        fs::write(
            &path,
            before
                .replace("return 1;", "const value = 2;\n    return value;")
                .replace("return 10;", "return 20;"),
        )
        .unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-m", "change both methods"]);
        // Range overlays must come from the committed blobs, not this file.
        fs::write(&path, "export class WrongWorktree {}\n").unwrap();
        let source = DiffSource::GitRef("HEAD~1..HEAD".to_string());
        let output = diff(
            &source,
            Some(dir.path()),
            Some("src/service.ts"),
            None,
            false,
            0,
            None,
        )
        .unwrap();
        assert!(output.contains("+3/−2 lines"), "{output}");
        assert_eq!(output.matches("const value = 2;").count(), 1, "{output}");
        assert_eq!(output.matches("return 1;").count(), 1, "{output}");
        assert_eq!(output.matches("return 20;").count(), 1, "{output}");
        assert!(
            output.contains("A::run") && output.contains("B::run"),
            "{output}"
        );
        assert!(!output.contains("WrongWorktree"), "{output}");
        for _ in 0..5 {
            assert_eq!(
                output,
                diff(
                    &source,
                    Some(dir.path()),
                    Some("./src/service.ts"),
                    None,
                    false,
                    0,
                    None
                )
                .unwrap()
            );
        }
        assert_eq!(
            output,
            diff(
                &source,
                Some(&dir.path().join("src")),
                Some(path.to_str().unwrap()),
                None,
                false,
                0,
                None
            )
            .unwrap()
        );
        let directory = diff(&source, Some(dir.path()), Some("src"), None, false, 0, None).unwrap();
        assert!(
            directory.contains("A::run") && directory.contains("B::run"),
            "{directory}"
        );
        let method = diff(
            &source,
            Some(dir.path()),
            Some("src/service.ts:A::run"),
            None,
            false,
            0,
            None,
        )
        .unwrap();
        assert!(
            method.contains("return 1;") && method.contains("const value = 2;"),
            "{method}"
        );
    }

    #[test]
    fn file_detail_preserves_non_code_added_and_deleted_totals() {
        let dir = setup_test_repo();
        fs::write(dir.path().join("notes.txt"), "one\ntwo\n").unwrap();
        git(dir.path(), &["add", "notes.txt"]);
        let added = diff(
            &DiffSource::GitStaged,
            Some(dir.path()),
            Some("notes.txt"),
            None,
            false,
            0,
            None,
        )
        .unwrap();
        assert!(
            added.contains("+2/−0 lines") && added.contains("one"),
            "{added}"
        );
        git(dir.path(), &["commit", "-m", "notes"]);
        fs::write(dir.path().join("notes.txt"), "one\nthree\n").unwrap();
        let modified = diff(
            &DiffSource::GitUncommitted,
            Some(dir.path()),
            Some("notes.txt"),
            None,
            false,
            0,
            None,
        )
        .unwrap();
        assert!(
            modified.contains("+1/−1 lines") && modified.contains("two"),
            "{modified}"
        );
        fs::remove_file(dir.path().join("notes.txt")).unwrap();
        let deleted = diff(
            &DiffSource::GitUncommitted,
            Some(dir.path()),
            Some("notes.txt"),
            None,
            false,
            0,
            None,
        )
        .unwrap();
        assert!(
            deleted.contains("+0/−2 lines") && deleted.contains("two"),
            "{deleted}"
        );
    }

    #[test]
    fn git_scope_is_literal_and_limits_the_patch() {
        let dir = setup_test_repo();
        fs::write(dir.path().join("src/[literal].rs"), "fn target() { 1 }\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-m", "literal path"]);
        fs::write(dir.path().join("src/[literal].rs"), "fn target() { 2 }\n").unwrap();
        fs::write(dir.path().join("src/main.rs"), "fn unrelated() {}\n").unwrap();
        let raw = run_git_diff_scoped(
            &DiffSource::GitUncommitted,
            Some(dir.path()),
            Some("src/[literal].rs:target"),
        )
        .unwrap();
        assert!(raw.contains("[literal].rs"));
        assert!(!raw.contains("main.rs") && !raw.contains("unrelated"));
    }

    #[test]
    fn scoped_move_detection_finds_external_file_without_leaking_its_output() {
        let dir = setup_test_repo();
        fs::write(
            dir.path().join("src/old.rs"),
            "pub fn moved() { println!(\"moved\"); }\n",
        )
        .unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-m", "source"]);
        fs::write(dir.path().join("src/old.rs"), "// moved elsewhere\n").unwrap();
        fs::create_dir(dir.path().join("destination")).unwrap();
        fs::write(
            dir.path().join("destination/new.rs"),
            "pub fn moved() { println!(\"moved\"); }\n",
        )
        .unwrap();
        git(dir.path(), &["add", "-A"]);
        let output = diff(
            &DiffSource::GitStaged,
            Some(dir.path()),
            Some("destination"),
            None,
            false,
            0,
            None,
        )
        .unwrap();
        assert!(
            output.contains("moved") && output.contains("from src/old.rs"),
            "{output}"
        );
        assert!(!output.contains("## src/old.rs"), "{output}");
    }

    #[test]
    fn scoped_move_candidate_limit_is_explicit() {
        let dir = setup_test_repo();
        fs::create_dir(dir.path().join("destination")).unwrap();
        fs::write(
            dir.path().join("destination/new.rs"),
            "pub fn common() { 1 }\n",
        )
        .unwrap();
        for i in 0..65 {
            fs::write(dir.path().join(format!("candidate{i}.txt")), "common\n").unwrap();
        }
        git(dir.path(), &["add", "-A"]);
        let output = diff(
            &DiffSource::GitStaged,
            Some(dir.path()),
            Some("destination/new.rs"),
            None,
            false,
            0,
            None,
        )
        .unwrap();
        assert!(
            output.contains("cross-scope move detection skipped"),
            "{output}"
        );
        assert!(!output.contains("candidate0.txt"), "{output}");
    }

    #[test]
    fn cross_scope_move_can_change_parent_but_requires_the_same_structure() {
        let dir = setup_test_repo();
        let old = dir.path().join("src/old.ts");
        fs::write(
            &old,
            "class Old {\n  run() { return 1; }\n}\nfunction unrelated() { return 10; }\n",
        )
        .unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-m", "old parent"]);
        fs::write(&old, "// definitions removed\n").unwrap();
        fs::create_dir(dir.path().join("destination")).unwrap();
        fs::write(
            dir.path().join("destination/new.ts"),
            "class New {\n  run() { return 1; }\n}\nfunction unrelated() { return 99; }\n",
        )
        .unwrap();
        git(dir.path(), &["add", "-A"]);
        let source = DiffSource::GitStaged;
        let mut overlays = parse::parse_unified_diff(
            &run_git_diff_scoped(&source, Some(dir.path()), Some("destination")).unwrap(),
        )
        .iter()
        .map(|file| overlay::compute_overlay(file, &source, Some(dir.path())))
        .collect::<Vec<_>>();
        assert!(
            extend_move_candidates(&mut overlays, &source, Some(dir.path()))
                .unwrap()
                .is_none()
        );
        overlay::cross_file_matching(&mut overlays);
        let new = overlays
            .iter()
            .find(|o| o.path == Path::new("destination/new.ts"))
            .unwrap();
        let method = new
            .symbol_changes
            .iter()
            .find(|change| change.name == "New::run")
            .unwrap();
        assert!(
            matches!(method.change, ChangeType::Moved { .. }),
            "{method:?}"
        );
        let unrelated = new
            .symbol_changes
            .iter()
            .find(|change| change.name == "unrelated")
            .unwrap();
        assert!(
            matches!(unrelated.change, ChangeType::Added),
            "{unrelated:?}"
        );
    }

    // 2. test_overview_modified
    #[test]
    fn test_overview_modified() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        let content = fs::read_to_string(&main_rs).unwrap();
        fs::write(
            &main_rs,
            content.replace("println!(\"hello\")", "println!(\"hi there\")"),
        )
        .unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert!(result.contains("[~]"), "expected [~] marker in:\n{result}");
    }

    // 3. test_overview_added
    #[test]
    fn test_overview_added() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        let mut content = fs::read_to_string(&main_rs).unwrap();
        content.push_str("\nfn new_function() {\n    println!(\"new\");\n}\n");
        fs::write(&main_rs, content).unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert!(result.contains("[+]"), "expected [+] marker in:\n{result}");
    }

    // 4. test_overview_deleted
    #[test]
    fn test_overview_deleted() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        // Remove the goodbye function entirely.
        fs::write(
            &main_rs,
            "fn hello() {\n    println!(\"hello\");\n}\n\nfn main() {\n    hello();\n}\n",
        )
        .unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert!(result.contains("[-]"), "expected [-] marker in:\n{result}");
    }

    // 5. test_overview_signature_changed
    #[test]
    fn test_overview_signature_changed() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        let content = fs::read_to_string(&main_rs).unwrap();
        // Change hello() to hello(name: &str)
        let new_content = content
            .replace("fn hello() {", "fn hello(name: &str) {")
            .replace("println!(\"hello\")", "println!(\"hello {}\", name)")
            .replace("hello();", "hello(\"world\");");
        fs::write(&main_rs, new_content).unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert!(
            result.contains("[~:sig]"),
            "expected [~:sig] marker in:\n{result}"
        );
    }

    // 6. test_file_detail_scope
    #[test]
    fn test_file_detail_scope() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        let content = fs::read_to_string(&main_rs).unwrap();
        fs::write(
            &main_rs,
            content.replace("println!(\"hello\")", "println!(\"hi\")"),
        )
        .unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            Some("src/main.rs"),
            None,
            false,
            None,
        )
        .unwrap();
        assert!(
            result.contains("# Diff: src/main.rs"),
            "expected file detail header in:\n{result}"
        );
        assert!(
            result.contains("symbols touched"),
            "expected symbols touched in:\n{result}"
        );
    }

    // 7. test_function_detail_scope
    #[test]
    fn test_function_detail_scope() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        let content = fs::read_to_string(&main_rs).unwrap();
        fs::write(
            &main_rs,
            content.replace("println!(\"hello\")", "println!(\"hi\")"),
        )
        .unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            Some("src/main.rs:hello"),
            None,
            false,
            None,
        )
        .unwrap();
        assert!(
            result.contains("hello"),
            "expected hello function in:\n{result}"
        );
    }

    // 8. test_staged_diff
    #[test]
    fn test_staged_diff() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        let content = fs::read_to_string(&main_rs).unwrap();
        fs::write(
            &main_rs,
            content.replace("println!(\"hello\")", "println!(\"staged\")"),
        )
        .unwrap();
        git(dir.path(), &["add", "src/main.rs"]);

        let result =
            run_diff_in(dir.path(), &DiffSource::GitStaged, None, None, false, None).unwrap();
        assert!(
            result.contains("main.rs") || result.contains("[~]"),
            "expected staged changes in:\n{result}"
        );
    }

    // 9. test_ref_diff
    #[test]
    fn test_ref_diff() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        let content = fs::read_to_string(&main_rs).unwrap();
        fs::write(
            &main_rs,
            content.replace("println!(\"hello\")", "println!(\"ref\")"),
        )
        .unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-m", "change hello"]);

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitRef("HEAD~1..HEAD".to_string()),
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert!(
            result.contains("main.rs"),
            "expected main.rs in ref diff:\n{result}"
        );
    }

    // 10. test_generated_file
    #[test]
    fn test_generated_file() {
        let dir = setup_test_repo();
        let lock = dir.path().join("package-lock.json");
        fs::write(&lock, "{}").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-m", "add lock"]);

        fs::write(&lock, "{ \"version\": 2 }").unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert!(
            result.contains("generated"),
            "expected 'generated' in:\n{result}"
        );
    }

    // 11. test_multiple_files
    #[test]
    fn test_multiple_files() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        let content = fs::read_to_string(&main_rs).unwrap();
        fs::write(
            &main_rs,
            content.replace("println!(\"hello\")", "println!(\"hi\")"),
        )
        .unwrap();

        let lib_rs = dir.path().join("src/lib.rs");
        fs::write(&lib_rs, "pub fn lib_fn() {\n    42\n}\n").unwrap();
        git(dir.path(), &["add", "src/lib.rs"]);
        git(dir.path(), &["commit", "-m", "add lib"]);
        fs::write(&lib_rs, "pub fn lib_fn() {\n    99\n}\n").unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert!(result.contains("main.rs"), "expected main.rs in:\n{result}");
        assert!(result.contains("lib.rs"), "expected lib.rs in:\n{result}");
        assert!(
            result.contains("2 files"),
            "expected '2 files' in:\n{result}"
        );
    }

    // 12. test_search_filter
    #[test]
    fn test_search_filter() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        let content = fs::read_to_string(&main_rs).unwrap();
        // Modify both functions.
        let new_content = content
            .replace("println!(\"hello\")", "println!(\"UNIQUE_MARKER\")")
            .replace("println!(\"bye\")", "println!(\"other change\")");
        fs::write(&main_rs, new_content).unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            None,
            Some("UNIQUE_MARKER"),
            false,
            None,
        )
        .unwrap();
        assert!(
            result.contains("hello"),
            "expected hello (matching) in:\n{result}"
        );
    }

    // 13. test_search_no_matches
    #[test]
    fn test_search_no_matches() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        let content = fs::read_to_string(&main_rs).unwrap();
        fs::write(
            &main_rs,
            content.replace("println!(\"hello\")", "println!(\"hi\")"),
        )
        .unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            None,
            Some("NONEXISTENT_TERM_XYZ"),
            false,
            None,
        )
        .unwrap();
        assert!(
            result.contains("No changes matching"),
            "expected no-match message in:\n{result}"
        );
    }

    // 14. test_file_scope_not_found
    #[test]
    fn scoped_git_paths_only_selects_the_requested_file() {
        let dir = setup_test_repo();
        fs::write(
            dir.path().join("src/main.rs"),
            "fn main() { println!(\"changed\"); }\n",
        )
        .unwrap();
        fs::write(dir.path().join("src/other.rs"), "fn other() {}\n").unwrap();
        git(dir.path(), &["add", "src/main.rs", "src/other.rs"]);

        let paths =
            scoped_git_paths(&DiffSource::GitStaged, Some(dir.path()), "src/main.rs").unwrap();
        assert_eq!(paths, vec![PathBuf::from("src/main.rs")]);
    }

    #[test]
    fn test_file_scope_not_found() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");
        let content = fs::read_to_string(&main_rs).unwrap();
        fs::write(
            &main_rs,
            content.replace("println!(\"hello\")", "println!(\"hi\")"),
        )
        .unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::GitUncommitted,
            Some("nonexistent.rs"),
            None,
            false,
            None,
        );
        assert!(result.is_err(), "expected error for missing file scope");
        assert!(
            result.unwrap_err().contains("not found"),
            "expected 'not found' in error"
        );
    }

    // 15. test_patch_file
    #[test]
    fn test_patch_file() {
        let dir = setup_test_repo();
        let patch = dir.path().join("test.patch");
        let patch_content = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,3 +1,3 @@
 fn hello() {
-    println!(\"hello\");
+    println!(\"patched\");
 }
";
        fs::write(&patch, patch_content).unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::Patch(patch.clone()),
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert!(
            result.contains("main.rs"),
            "expected main.rs in patch result:\n{result}"
        );
    }

    // 16. test_file_to_file
    #[test]
    fn test_file_to_file() {
        let dir = setup_test_repo();
        let file_a = dir.path().join("a.txt");
        let file_b = dir.path().join("b.txt");
        fs::write(&file_a, "line one\nline two\n").unwrap();
        fs::write(&file_b, "line one\nline three\n").unwrap();

        let result = run_diff_in(
            dir.path(),
            &DiffSource::Files(file_a, file_b),
            None,
            None,
            false,
            None,
        )
        .unwrap();
        // The diff should contain something — the files differ.
        assert!(
            !result.contains("No changes"),
            "expected changes between files:\n{result}"
        );
    }

    // 17. test_log_mode
    #[test]
    fn test_log_mode() {
        let dir = setup_test_repo();
        let main_rs = dir.path().join("src/main.rs");

        // Make a second commit.
        let content = fs::read_to_string(&main_rs).unwrap();
        fs::write(
            &main_rs,
            content.replace("println!(\"hello\")", "println!(\"log test\")"),
        )
        .unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-m", "second commit"]);

        let result = run_diff_in(
            dir.path(),
            &DiffSource::Log("HEAD~1..HEAD".to_string()),
            None,
            None,
            false,
            None,
        )
        .unwrap();
        assert!(
            result.contains("# Log:"),
            "expected log header in:\n{result}"
        );
        assert!(
            result.contains("second commit"),
            "expected commit message in:\n{result}"
        );
    }

    // 18. test_resolve_source_variants
    #[test]
    fn test_resolve_source_variants() {
        // Default → uncommitted.
        assert!(matches!(
            resolve_source(None, None, None, None, None).unwrap(),
            DiffSource::GitUncommitted
        ));

        // Staged.
        assert!(matches!(
            resolve_source(Some("staged"), None, None, None, None).unwrap(),
            DiffSource::GitStaged
        ));

        // Working.
        assert!(matches!(
            resolve_source(Some("working"), None, None, None, None).unwrap(),
            DiffSource::GitUncommitted
        ));

        // Ref.
        match resolve_source(Some("HEAD~3..HEAD"), None, None, None, None).unwrap() {
            DiffSource::GitRef(r) => assert_eq!(r, "HEAD~3..HEAD"),
            other => panic!("expected GitRef, got {other:?}"),
        }

        // Files.
        match resolve_source(None, Some("a.rs"), Some("b.rs"), None, None).unwrap() {
            DiffSource::Files(a, b) => {
                assert_eq!(a, PathBuf::from("a.rs"));
                assert_eq!(b, PathBuf::from("b.rs"));
            }
            other => panic!("expected Files, got {other:?}"),
        }

        // Error: only one of a/b.
        assert!(resolve_source(None, Some("a.rs"), None, None, None).is_err());

        // Patch.
        match resolve_source(None, None, None, Some("test.patch"), None).unwrap() {
            DiffSource::Patch(p) => assert_eq!(p, PathBuf::from("test.patch")),
            other => panic!("expected Patch, got {other:?}"),
        }

        // Log.
        match resolve_source(None, None, None, None, Some("HEAD~5..HEAD")).unwrap() {
            DiffSource::Log(r) => assert_eq!(r, "HEAD~5..HEAD"),
            other => panic!("expected Log, got {other:?}"),
        }

        // Patch takes priority over source.
        assert!(matches!(
            resolve_source(Some("staged"), None, None, Some("x.patch"), None).unwrap(),
            DiffSource::Patch(_)
        ));
    }

    // ── git config must not change the shape of what we parse (#208) ─────────

    /// Stage one change inside `goodbye`, so the patch has a symbol to
    /// attribute the hunk to and a blank context line (the gap between
    /// `hello` and `goodbye`) three lines above it.
    fn stage_goodbye_change(dir: &Path) {
        let main_rs = dir.join("src/main.rs");
        let content = fs::read_to_string(&main_rs).unwrap();
        fs::write(
            &main_rs,
            content.replace("println!(\"bye\")", "println!(\"farewell\")"),
        )
        .unwrap();
        git(dir, &["add", "-A"]);
    }

    fn staged_diff(dir: &Path) -> String {
        run_diff_in(dir, &DiffSource::GitStaged, None, None, false, None).unwrap()
    }

    /// `diff.external` replaces the unified patch with whatever the tool
    /// prints. With `true` that is nothing, which used to read as "No changes.".
    #[test]
    fn staged_diff_bypasses_diff_external() {
        let dir = setup_test_repo();
        git(dir.path(), &["config", "diff.external", "true"]);
        stage_goodbye_change(dir.path());
        let result = staged_diff(dir.path());
        assert!(
            result.contains("goodbye"),
            "external diff tool must be bypassed:\n{result}"
        );
    }

    /// `color.ui=always` wraps the patch in escape codes the parser cannot read.
    #[test]
    fn staged_diff_ignores_color_ui_always() {
        let dir = setup_test_repo();
        git(dir.path(), &["config", "color.ui", "always"]);
        stage_goodbye_change(dir.path());
        let result = staged_diff(dir.path());
        assert!(
            result.contains("goodbye"),
            "colour codes must not reach the parser:\n{result}"
        );
    }

    /// `diff.noprefix` and `diff.mnemonicPrefix` change the `a/` `b/` prefixes
    /// the parser keys on; the file came out as `src/main.rs src/main.rs` or
    /// `c/src/main.rs i/src/main.rs`.
    #[test]
    fn staged_diff_pins_path_prefixes() {
        for (key, value) in [("diff.noprefix", "true"), ("diff.mnemonicPrefix", "true")] {
            let dir = setup_test_repo();
            git(dir.path(), &["config", key, value]);
            stage_goodbye_change(dir.path());
            let result = staged_diff(dir.path());
            assert!(
                result.contains("## src/main.rs")
                    && !result.contains("src/main.rs src/main.rs")
                    && !result.contains("i/src/main.rs"),
                "{key}={value}: path must be unprefixed and repo-relative:\n{result}"
            );
        }
    }

    /// `diff.suppressBlankEmpty` emits a blank context line as "" instead of
    /// " ". The parser drops such lines, shifting every line after them.
    #[test]
    fn run_git_diff_keeps_blank_context_lines() {
        let dir = setup_test_repo();
        git(dir.path(), &["config", "diff.suppressBlankEmpty", "true"]);
        stage_goodbye_change(dir.path());
        let raw = run_git_diff_scoped(&DiffSource::GitStaged, Some(dir.path()), None).unwrap();
        assert!(
            raw.lines().any(|l| l == " "),
            "blank context line must survive as a single space:\n{raw}"
        );
    }

    /// `diff.relative` rewrites paths relative to the cwd; the overlay reads
    /// files by repo-root path.
    #[test]
    fn run_git_diff_keeps_repo_relative_paths() {
        let dir = setup_test_repo();
        git(dir.path(), &["config", "diff.relative", "true"]);
        stage_goodbye_change(dir.path());
        let raw = run_git_diff_scoped(&DiffSource::GitStaged, Some(&dir.path().join("src")), None)
            .unwrap();
        assert!(
            raw.contains("diff --git a/src/main.rs b/src/main.rs"),
            "paths must stay repo-relative when git runs in a subdirectory:\n{raw}"
        );
    }

    /// A git failure used to parse as an empty patch and report "No changes.".
    #[test]
    fn bad_ref_is_an_error_not_no_changes() {
        let dir = setup_test_repo();
        let err = run_diff_in(
            dir.path(),
            &DiffSource::GitRef("no-such-ref".to_string()),
            None,
            None,
            false,
            None,
        )
        .unwrap_err();
        assert!(
            err.contains("no-such-ref"),
            "error must carry git's own message: {err}"
        );
    }
}
// test

#[cfg(test)]
mod directory_scope_path_tests {
    use super::directory_scope_path;
    use std::path::PathBuf;

    #[test]
    fn absolute_directory_scope_resolves_to_repo_relative_path() {
        let repo = tempfile::tempdir().unwrap();
        let absolute_scope = repo.path().join("src/nested");
        std::fs::create_dir_all(&absolute_scope).unwrap();

        assert_eq!(
            directory_scope_path(absolute_scope.to_str().unwrap(), Some(repo.path())),
            Some((PathBuf::from("src/nested"), true))
        );
    }
}
