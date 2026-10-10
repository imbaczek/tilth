pub mod imports;
pub mod outline;

use std::fmt::Write;
use std::fs;
use std::path::Path;

use memmap2::Mmap;

use crate::cache::OutlineCache;
use crate::error::TilthError;
use crate::format;
use crate::lang::detect_file_type;
use crate::lang::outline::{heading_level, heading_text, parse_markdown};
use crate::types::{estimate_tokens, FileType, ViewMode};

pub(crate) const TOKEN_THRESHOLD: u64 = 6_000;
/// Minimum compression ratio required to prefer the outline over full content.
/// If `outline_tokens >= full_tokens * OUTLINE_MIN_COMPRESSION / 100`, the
/// outline saves too few tokens to be worth the information loss, so the full
/// file is returned instead. Integer arithmetic: 80 means 80%.
const OUTLINE_MIN_COMPRESSION: u64 = 80;
const FILE_SIZE_CAP: u64 = 500_000; // 500KB

/// Max file size for `full=true` reads. Files above this threshold get a
/// warning header + outline instead of raw content, preventing multi-megabyte
/// responses that cause MCP client timeouts. Grok shares this cap to bound
/// its lossy decode + outline parse of code files.
/// Override with `TILTH_FULL_SIZE_CAP` env var (bytes). Default: 2MB.
pub(crate) fn full_read_size_cap() -> u64 {
    std::env::var("TILTH_FULL_SIZE_CAP")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2_000_000)
}

/// Main entry point for read mode. Routes through the decision tree.
pub fn read_file(
    path: &Path,
    section: Option<&str>,
    full: bool,
    cache: &OutlineCache,
    edit_mode: bool,
) -> Result<String, TilthError> {
    read_file_with_budget(path, section, full, cache, edit_mode, None)
}

/// Select the automatic view before applying the caller's output budget.
pub(crate) fn read_file_with_budget(
    path: &Path,
    section: Option<&str>,
    full: bool,
    cache: &OutlineCache,
    edit_mode: bool,
    budget: Option<u64>,
) -> Result<String, TilthError> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(TilthError::NotFound {
                path: path.to_path_buf(),
                suggestion: suggest_similar(path),
            });
        }
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(TilthError::PermissionDenied {
                path: path.to_path_buf(),
            });
        }
        Err(e) => {
            return Err(TilthError::IoError {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };

    // Directory → list contents
    if meta.is_dir() {
        return list_directory(
            path,
            crate::listing::Page::new(0, crate::listing::DEFAULT_LIMIT).expect("positive default"),
            false,
        );
    }

    let byte_len = meta.len();

    // Empty check before mmap — mmap on 0-byte file may fail on some platforms
    if byte_len == 0 {
        return Ok(format::file_header(path, 0, 0, ViewMode::Empty));
    }

    // Section param → return those lines verbatim, any size
    if let Some(range) = section {
        return read_ranges(path, &[range], edit_mode);
    }

    // Binary detection
    let file = fs::File::open(path).map_err(|e| TilthError::IoError {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mmap = unsafe { Mmap::map(&file) }.map_err(|e| TilthError::IoError {
        path: path.to_path_buf(),
        source: e,
    })?;
    let buf = &mmap[..];

    if crate::lang::detection::is_binary(buf) {
        let mime = mime_from_ext(path);
        return Ok(format::binary_header(path, byte_len, mime));
    }

    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

    // Generated
    if crate::lang::detection::is_generated_by_name(name)
        || crate::lang::detection::is_generated_by_content(buf)
    {
        let line_count = memchr::memchr_iter(b'\n', buf).count() as u32 + 1;
        return Ok(format::file_header(
            path,
            byte_len,
            line_count,
            ViewMode::Generated,
        ));
    }

    // Minified — filename convention or, for big files, newline-density heuristic.
    if crate::lang::detection::is_minified_by_name(name)
        || (byte_len >= crate::lang::detection::MINIFIED_CHECK_THRESHOLD
            && crate::lang::detection::is_minified_by_content(buf))
    {
        let line_count = memchr::memchr_iter(b'\n', buf).count() as u32 + 1;
        return Ok(format::file_header(
            path,
            byte_len,
            line_count,
            ViewMode::Minified,
        ));
    }

    let tokens = estimate_tokens(byte_len);
    let content = String::from_utf8_lossy(buf);
    let line_count = memchr::memchr_iter(b'\n', buf).count() as u32 + 1;

    // Guard: full=true on very large files. Return outline + warning instead of
    // dumping megabytes that would blow up the MCP client's timeout/memory.
    let cap = full_read_size_cap();
    if full && byte_len > cap {
        let file_type = detect_file_type(path);
        let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        #[allow(clippy::cast_precision_loss)] // cap and file sizes fit in f64 mantissa for display
        let cap_mb = cap as f64 / 1_000_000.0;
        #[allow(clippy::cast_precision_loss)]
        let file_mb = byte_len as f64 / 1_000_000.0;

        let outline = cache.get_or_compute_with_cap(path, mtime, true, || {
            outline::generate(path, file_type, &content, buf, true)
        });

        let header = format::file_header(path, byte_len, line_count, ViewMode::Outline);
        return Ok(format!(
            "{header}\n\n> **full=true skipped**: file is {file_mb:.1}MB (cap: {cap_mb:.1}MB). \
             Use `section` to read specific ranges, or set TILTH_FULL_SIZE_CAP={byte_len} to override.\n\n{outline}"
        ));
    }

    // Canonical full-content view — shared by the small-file gate and OGATE below.
    let full_view = || {
        let header = format::file_header(path, byte_len, line_count, ViewMode::Full);
        if edit_mode {
            let numbered = format::hashlines(&content, 1);
            format!("{header}\n\n{numbered}")
        } else {
            format!("{header}\n\n{content}")
        }
    };

    // Full mode or small file → return full content (skip smart view)
    if full
        || (tokens <= TOKEN_THRESHOLD
            && budget.is_none_or(|limit| estimate_tokens(full_view().len() as u64) <= limit))
    {
        return Ok(full_view());
    }

    // Large file → smart view by file type
    let file_type = detect_file_type(path);
    let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);

    let capped = byte_len > FILE_SIZE_CAP;

    let outline = cache.get_or_compute_with_cap(path, mtime, capped, || {
        outline::generate(path, file_type, &content, buf, capped)
    });

    // OGATE: if the outline is not meaningfully smaller than the full file,
    // return the full file — same token cost with less information is strictly
    // worse and forces the caller to do a second read.
    let outline_tokens = estimate_tokens(outline.len() as u64);
    if outline_tokens >= tokens * OUTLINE_MIN_COMPRESSION / 100 {
        return Ok(full_view());
    }

    let mode = match file_type {
        FileType::StructuredData => ViewMode::Keys,
        _ => ViewMode::Outline,
    };
    let header = format::file_header(path, byte_len, line_count, mode);
    Ok(format!("{header}\n\n{outline}"))
}

/// Would this file produce an outline (rather than full content) in default read mode?
/// Used by the MCP layer to decide whether to append related-file hints.
pub fn would_outline(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| !m.is_dir() && estimate_tokens(m.len()) > TOKEN_THRESHOLD)
}

/// An ATX heading and its complete section, with a copyable TOC address.
struct HeadingSection {
    level: u8,
    title: String,
    start: usize,
    end: usize,
    address: String,
    ancestry: String,
}

impl HeadingSection {
    fn describe(&self) -> String {
        format!(
            "{}  {} (lines {}-{})",
            self.address, self.ancestry, self.start, self.end
        )
    }
}

/// Use the parser's section spans so nested headings belong to their parent,
/// and headings inside code or HTML blocks never become selectable sections.
/// Setext headings remain unsupported, as in the markdown outline.
fn heading_sections(buf: &[u8]) -> Vec<HeadingSection> {
    let Ok(content) = std::str::from_utf8(buf) else {
        return Vec::new();
    };
    let Some(tree) = parse_markdown(content) else {
        return Vec::new();
    };
    let lines: Vec<&str> = content.lines().collect();
    heading_sections_from_tree(tree.root_node(), &lines, usize::MAX)
}

/// Shared by section selection and Markdown outlines. Limiting the collected
/// prefix does not change its addresses; later headings cannot affect ancestry.
fn heading_sections_from_tree(
    node: tree_sitter::Node,
    lines: &[&str],
    max_sections: usize,
) -> Vec<HeadingSection> {
    let mut sections = Vec::new();
    collect_sections(node, lines, &mut sections, max_sections);

    // Number actual siblings using section containment, independent of ATX
    // level gaps. Container headings (quotes/lists) can have the same level
    // as their enclosing heading, so levels alone cannot determine parents.
    let mut stack: Vec<usize> = Vec::new();
    let mut children = vec![0; sections.len()];
    let mut roots = 0;
    for i in 0..sections.len() {
        while stack.last().is_some_and(|&j| {
            sections[j].end < sections[i].start || sections[j].end < sections[i].end
        }) {
            stack.pop();
        }
        let (address, ancestry) = if let Some(&parent) = stack.last() {
            children[parent] += 1;
            (
                format!("{}.{}", sections[parent].address, children[parent]),
                format!("{} > {}", sections[parent].ancestry, sections[i].title),
            )
        } else {
            roots += 1;
            (format!("toc:{roots}"), sections[i].title.clone())
        };
        sections[i].address = address;
        sections[i].ancestry = ancestry;
        stack.push(i);
    }
    sections
}

fn collect_sections(
    node: tree_sitter::Node,
    lines: &[&str],
    out: &mut Vec<HeadingSection>,
    max_sections: usize,
) {
    if out.len() >= max_sections {
        return;
    }
    if node.kind() == "section" {
        let mut cursor = node.walk();
        let heading = node
            .children(&mut cursor)
            .find(|c| c.kind() == "atx_heading");
        if let Some(heading) = heading {
            if let Some(level) = heading_level(heading) {
                out.push(HeadingSection {
                    level,
                    title: heading_text(heading, lines),
                    start: heading.start_position().row + 1,
                    end: section_end_line(node),
                    address: String::new(),
                    ancestry: String::new(),
                });
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if out.len() >= max_sections {
            break;
        }
        if !matches!(
            child.kind(),
            "fenced_code_block" | "indented_code_block" | "html_block"
        ) {
            collect_sections(child, lines, out, max_sections);
        }
    }
}

/// 1-indexed inclusive last line of a tree-sitter section (end is exclusive).
fn section_end_line(section: tree_sitter::Node) -> usize {
    let end = section.end_position();
    if end.column == 0 {
        end.row
    } else {
        end.row + 1
    }
}

/// Plain titles match every level; an optional ATX prefix constrains the level.
fn heading_query(query: &str) -> Option<(Option<u8>, &str)> {
    let query = query.trim();
    let level = query.bytes().take_while(|&c| c == b'#').count();
    if level == 0 {
        return (!query.is_empty()).then_some((None, query));
    }
    if level > 6 {
        return None;
    }
    let title = query[level..].trim().trim_end_matches('#').trim();
    #[allow(clippy::cast_possible_truncation)]
    (!title.is_empty()).then_some((Some(level as u8), title))
}

/// Rank miss suggestions while retaining unique addresses and parent context.
fn closest_headings(sections: &[HeadingSection], query: &str, top_n: usize) -> Vec<String> {
    let Some((_, title)) = heading_query(query) else {
        return Vec::new();
    };
    let q_lower = title.to_ascii_lowercase();
    let mut scored: Vec<_> = sections
        .iter()
        .filter_map(|section| {
            let clean = section
                .title
                .split('{')
                .next()
                .unwrap_or(&section.title)
                .trim();
            (!clean.is_empty()).then(|| {
                (
                    edit_distance(&q_lower, &clean.to_ascii_lowercase()),
                    section,
                )
            })
        })
        .collect();
    scored.sort_by_key(|(distance, _)| *distance);
    scored
        .into_iter()
        .take(top_n)
        .map(|(_, section)| section.describe())
        .collect()
}

#[cfg(test)]
fn suggest_headings(buf: &[u8], query: &str, top_n: usize) -> Vec<String> {
    closest_headings(&heading_sections(buf), query, top_n)
}

#[cfg(test)]
fn resolve_heading(buf: &[u8], heading: &str) -> Option<(usize, usize)> {
    resolve_range(buf, heading).ok()
}

/// Line ranges take precedence; `toc:` reserves a separate numeric grammar.
/// Every heading lookup must identify exactly one section.
fn resolve_range(buf: &[u8], range: &str) -> Result<(usize, usize), TilthError> {
    let invalid = |reason: String| TilthError::InvalidQuery {
        query: range.to_string(),
        reason,
    };
    let query = range.trim();
    if let Some(lines) = parse_range(query) {
        return Ok(lines);
    }
    let sections = heading_sections(buf);
    if let Some(address) = query.strip_prefix("toc:") {
        if address.split('.').any(|part| {
            part.is_empty()
                || !part.bytes().all(|b| b.is_ascii_digit())
                || part.starts_with('0')
                || part.parse::<usize>().is_err()
        }) {
            return Err(invalid("invalid TOC address: expected toc:1 or toc:1.2 (positive sibling indices, no leading zeros)".into()));
        }
        return sections
            .iter()
            .find(|s| s.address == query)
            .map(|s| (s.start, s.end))
            .ok_or_else(|| invalid("TOC address not found in file".into()));
    }
    let Some((level, title)) = heading_query(query) else {
        return Err(invalid("expected a line range (45-89), heading title (Architecture or ## Architecture), or TOC address (toc:1.2)".into()));
    };
    let matches: Vec<_> = sections
        .iter()
        .filter(|s| level.is_none_or(|level| level == s.level) && s.title == title)
        .collect();
    match matches.as_slice() {
        [section] => Ok((section.start, section.end)),
        [] => {
            let suggestions = closest_headings(&sections, query, 5);
            let reason = if suggestions.is_empty() {
                "heading not found in file".to_string()
            } else {
                format!(
                    "heading not found in file. Closest matches:\n  {}",
                    suggestions.join("\n  ")
                )
            };
            Err(invalid(reason))
        }
        _ => Err(invalid(format!(
            "ambiguous heading; select a TOC address with --section (or section in MCP):\n  {}",
            matches
                .iter()
                .map(|s| s.describe())
                .collect::<Vec<_>>()
                .join("\n  ")
        ))),
    }
}

/// One resolved range, ready to format.
struct Block {
    start: usize, // 1-indexed inclusive
    end: usize,   // 1-indexed inclusive (clamped to file length)
    text: String,
}

/// Read one or more line ranges from a file. Each range is "start-end"
/// (e.g. "45-89"), a heading title (optional ATX level prefix), or a TOC
/// address (e.g. "toc:1.2") for markdown files. Mmaps the file once and emits a single `[section]`
/// header followed by the formatted blocks; when more than one range is
/// requested, each block is preceded by a `─── lines X-Y ───` delimiter.
///
/// Ranges are emitted in the order supplied — overlapping or out-of-order
/// ranges are honored verbatim, not coalesced or sorted. Any invalid
/// range fails the whole call.
pub fn read_ranges(path: &Path, ranges: &[&str], edit_mode: bool) -> Result<String, TilthError> {
    let (blocks, total_bytes, total_lines) = collect_blocks(path, ranges, edit_mode)?;
    let header = format::file_header(path, total_bytes, total_lines, ViewMode::Section);

    if blocks.len() == 1 {
        let b = &blocks[0];
        return Ok(format!("{header}\n\n{}", b.text));
    }

    let mut out = String::with_capacity(header.len() + total_bytes as usize + blocks.len() * 32);
    out.push_str(&header);
    for b in &blocks {
        let _ = write!(out, "\n\n─── lines {}-{} ───\n", b.start, b.end);
        out.push_str(&b.text);
    }
    Ok(out)
}

/// Like [`read_ranges`], but with a strict shared token budget across all
/// sections.
///
/// Blocks are emitted in user order. Once the remaining budget cannot hold a
/// later block, that block is replaced with an omission marker while the marker
/// itself still fits. If the first block alone exceeds the budget, the standard
/// truncator cuts that first block and all later blocks are omitted.
pub fn read_ranges_with_budget(
    path: &Path,
    ranges: &[&str],
    edit_mode: bool,
    budget: u64,
) -> Result<String, TilthError> {
    let (blocks, total_bytes, total_lines) = collect_blocks(path, ranges, edit_mode)?;
    let header = format::file_header(path, total_bytes, total_lines, ViewMode::Section);

    if blocks.len() == 1 {
        let single = format!("{header}\n\n{}", blocks[0].text);
        return Ok(crate::budget::apply(&single, budget));
    }

    let mut out = String::with_capacity(header.len() + total_bytes as usize + blocks.len() * 32);
    out.push_str(&header);
    let mut remaining = budget.saturating_sub(estimate_tokens(header.len() as u64));

    for (i, b) in blocks.iter().enumerate() {
        let chunk = format!("\n\n─── lines {}-{} ───\n{}", b.start, b.end, b.text);
        let tokens = estimate_tokens(chunk.len() as u64);
        if tokens <= remaining {
            out.push_str(&chunk);
            remaining -= tokens;
            continue;
        }
        if i == 0 {
            let single = format!("{header}{chunk}");
            return Ok(crate::budget::apply(&single, budget));
        }
        let marker = format!(
            "\n\n─── lines {}-{} ───\n... section omitted (budget exhausted) ...",
            b.start, b.end
        );
        let marker_tokens = estimate_tokens(marker.len() as u64);
        if marker_tokens > remaining {
            break;
        }
        out.push_str(&marker);
        remaining -= marker_tokens;
    }
    Ok(out)
}

fn collect_blocks(
    path: &Path,
    ranges: &[&str],
    edit_mode: bool,
) -> Result<(Vec<Block>, u64, u32), TilthError> {
    if ranges.is_empty() {
        return Err(TilthError::InvalidQuery {
            query: String::new(),
            reason: "at least one range is required".into(),
        });
    }

    let file = fs::File::open(path).map_err(|e| TilthError::IoError {
        path: path.to_path_buf(),
        source: e,
    })?;
    let mmap = unsafe { Mmap::map(&file) }.map_err(|e| TilthError::IoError {
        path: path.to_path_buf(),
        source: e,
    })?;
    let buf = &mmap[..];

    let mut line_offsets: Vec<usize> = vec![0];
    for pos in memchr::memchr_iter(b'\n', buf) {
        line_offsets.push(pos + 1);
    }
    let total = line_offsets.len();

    let mut blocks: Vec<Block> = Vec::with_capacity(ranges.len());
    let mut total_bytes: u64 = 0;
    let mut total_lines: u32 = 0;

    for range in ranges {
        let (start, end) = resolve_range(buf, range)?;
        let s = start.saturating_sub(1).min(total);
        let e = end.min(total);
        if s >= e {
            return Err(TilthError::InvalidQuery {
                query: (*range).to_string(),
                reason: format!("range out of bounds (file has {total} lines)"),
            });
        }
        let start_byte = line_offsets[s];
        let end_byte = if e < line_offsets.len() {
            line_offsets[e]
        } else {
            buf.len()
        };
        let selected = String::from_utf8_lossy(&buf[start_byte..end_byte]);
        total_bytes += selected.len() as u64;
        total_lines += (e - s) as u32;
        let formatted = if edit_mode {
            format::hashlines(&selected, start as u32)
        } else {
            format::number_lines(&selected, start as u32)
        };
        blocks.push(Block {
            start,
            end: e,
            text: formatted,
        });
    }

    Ok((blocks, total_bytes, total_lines))
}

/// Parse "45-89" into (45, 89). 1-indexed.
fn parse_range(s: &str) -> Option<(usize, usize)> {
    let (a, b) = s.split_once('-')?;
    let start: usize = a.trim().parse().ok()?;
    let end: usize = b.trim().parse().ok()?;
    if start == 0 || end < start {
        return None;
    }
    Some((start, end))
}

/// List directory contents — treat as glob on dir/*.
pub(crate) fn list_directory(
    path: &Path,
    page: crate::listing::Page,
    mcp: bool,
) -> Result<String, TilthError> {
    let mut entries: Vec<String> = Vec::new();
    let read_dir = fs::read_dir(path).map_err(|e| TilthError::IoError {
        path: path.to_path_buf(),
        source: e,
    })?;

    let mut items: Vec<_> = read_dir.filter_map(std::result::Result::ok).collect();
    items.sort_by_key(std::fs::DirEntry::file_name);

    let (start, end) = page.bounds(items.len());
    for entry in &items[start..end] {
        let ft = entry.file_type().ok();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let meta = entry.metadata().ok();

        let suffix = match ft {
            Some(t) if t.is_dir() => "/".to_string(),
            Some(t) if t.is_symlink() => " →".to_string(),
            _ => match meta {
                Some(m) => {
                    let tokens = estimate_tokens(m.len());
                    format!("  ({tokens} tokens)")
                }
                None => String::new(),
            },
        };
        entries.push(format!("  {name}{suffix}"));
    }

    let header = format!("# {} ({} items)", path.display(), items.len());
    let mut output = format!(
        "{header}\n\n{}\n{}",
        page.summary(items.len(), "items"),
        entries.join("\n")
    );
    page.append_navigation(&mut output, items.len(), mcp);
    Ok(output)
}

/// Public entry point for did-you-mean on path-like fallthrough queries.
/// Resolves the query relative to scope and checks the parent directory.
pub fn suggest_similar_file(scope: &Path, query: &str) -> Option<String> {
    let resolved = scope.join(query);
    suggest_similar(&resolved)
}

/// Suggest a similar file name from the parent directory (edit distance).
fn suggest_similar(path: &Path) -> Option<String> {
    let parent = path.parent()?;
    let name = path.file_name()?.to_str()?;
    let entries = fs::read_dir(parent).ok()?;

    let mut best: Option<(usize, String)> = None;
    for entry in entries.flatten() {
        let candidate = entry.file_name();
        let candidate = candidate.to_string_lossy();
        let dist = edit_distance(name, &candidate);
        if dist <= 3 {
            match &best {
                Some((d, _)) if dist < *d => best = Some((dist, candidate.into_owned())),
                None => best = Some((dist, candidate.into_owned())),
                _ => {}
            }
        }
    }
    best.map(|(_, name)| name)
}

/// Levenshtein distance over Unicode scalar values.
///
/// Wraps `strsim::levenshtein`, which iterates `.chars()` so a single CJK
/// or emoji glyph counts as one edit unit (not 3-4 bytes). Used by both
/// filename suggestion (`suggest_similar`) and heading suggestion
/// (`suggest_headings`).
fn edit_distance(a: &str, b: &str) -> usize {
    strsim::levenshtein(a, b)
}

/// Guess MIME type from extension for binary file headers.
pub(crate) fn mime_from_ext(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("pdf") => "application/pdf",
        Some("zip") => "application/zip",
        Some("gz" | "tgz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("wasm") => "application/wasm",
        Some("woff" | "woff2") => "font/woff2",
        Some("ttf" | "otf") => "font/ttf",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_titles_and_level_constraints() {
        let input = b"# Title\n## Shared\na\n### Shared\nb\n## Other ##\nc\n";
        assert_eq!(resolve_range(input, "Title").unwrap(), (1, 7));
        assert_eq!(resolve_range(input, "  Other  ").unwrap(), (6, 7));
        assert_eq!(resolve_range(input, "## Shared").unwrap(), (2, 5));
        assert_eq!(resolve_range(input, "### Shared").unwrap(), (4, 5));
        assert!(resolve_range(input, "Shared")
            .unwrap_err()
            .to_string()
            .contains("ambiguous heading"));
    }

    #[test]
    fn ambiguous_titles_offer_parent_paths_and_round_trip_addresses() {
        let input = b"# One\n### Shared\na\n### Shared\nb\n# Two\n## Shared\nc\n";
        for query in ["Shared", "### Shared"] {
            let err = resolve_range(input, query).unwrap_err().to_string();
            assert!(err.contains("toc:1.1  One > Shared (lines 2-3)"), "{err}");
            assert!(err.contains("toc:1.2  One > Shared (lines 4-5)"), "{err}");
        }
        let err = resolve_range(input, "Shared").unwrap_err().to_string();
        assert!(err.contains("toc:2.1  Two > Shared (lines 7-8)"), "{err}");
        assert_eq!(resolve_range(input, "toc:1.1").unwrap(), (2, 3));
        assert_eq!(resolve_range(input, "toc:1.2").unwrap(), (4, 5));
        assert_eq!(resolve_range(input, "toc:2.1").unwrap(), (7, 8));
        assert_eq!(resolve_range(input, "toc:1").unwrap(), (1, 5));
    }

    #[test]
    fn toc_addresses_skip_level_gaps_and_code_blocks() {
        let input = b"## Root\n~~~md\n# Fake\n~~~\n###### Child\nx\n### Sibling\ny\n## Next\nz";
        assert_eq!(resolve_range(input, "toc:1.1").unwrap(), (5, 6));
        assert_eq!(resolve_range(input, "toc:1.2").unwrap(), (7, 8));
        assert_eq!(resolve_range(input, "toc:2").unwrap(), (9, 10));
        assert!(resolve_range(input, "Fake").is_err());
    }

    #[test]
    fn toc_ancestry_respects_blockquote_and_list_section_boundaries() {
        for container in [
            "> # Nested\n> ## Shared\n> inside\n",
            "- # Nested\n  ## Shared\n  inside\n",
        ] {
            let input = format!("# Outer\n\n{container}\n## Shared\noutside\n");
            let sections = heading_sections(input.as_bytes());
            let shared: Vec<_> = sections.iter().filter(|s| s.title == "Shared").collect();
            assert_eq!(shared.len(), 2, "{input}");
            assert_eq!(shared[0].address, "toc:1.1.1", "{input}");
            assert_eq!(shared[0].ancestry, "Outer > Nested > Shared", "{input}");
            assert_eq!(shared[1].address, "toc:1.2", "{input}");
            assert_eq!(shared[1].ancestry, "Outer > Shared", "{input}");
            assert_eq!(resolve_range(input.as_bytes(), "toc:1.2").unwrap(), (7, 8));
            let err = resolve_range(input.as_bytes(), "Shared")
                .unwrap_err()
                .to_string();
            assert!(err.contains("toc:1.2  Outer > Shared (lines 7-8)"), "{err}");
        }
    }

    #[test]
    fn toc_grammar_does_not_conflict_with_lines_or_numeric_titles() {
        let input = b"# 1.2\n## 1-2\n## toc:1\nbody\n";
        assert_eq!(resolve_range(input, "1.2").unwrap(), (1, 4));
        assert_eq!(resolve_range(input, "1-2").unwrap(), (1, 2));
        assert_eq!(resolve_range(input, "## 1-2").unwrap(), (2, 2));
        assert_eq!(resolve_range(input, "## toc:1").unwrap(), (3, 4));
        for query in [
            "toc:",
            "toc:0",
            "toc:1.0",
            "toc:01",
            "toc:1.",
            "toc:1..2",
            "toc:1-2",
            "toc:1.foo",
            "toc:999999999999999999999999999999",
        ] {
            assert!(
                resolve_range(input, query)
                    .unwrap_err()
                    .to_string()
                    .contains("invalid TOC address"),
                "{query}"
            );
        }
        assert!(resolve_range(input, "toc:2")
            .unwrap_err()
            .to_string()
            .contains("not found"));
    }

    #[test]
    fn heading_found() {
        let input = b"# Title\nSome content\n## Section\nSection content\n";
        let result = resolve_heading(input, "## Section");

        assert_eq!(result, Some((3, 4)));
    }

    #[test]
    fn heading_not_found() {
        let input = b"# Title\nContent\n";
        let result = resolve_heading(input, "## Missing");

        assert_eq!(result, None);
    }

    #[test]
    fn heading_in_code_block() {
        let input = b"# Real\n```\n## Fake\n```\n";
        let result = resolve_heading(input, "## Fake");

        // Heading inside code block should be skipped
        assert_eq!(result, None);
    }

    #[test]
    fn duplicate_headings() {
        let input = b"## First\ntext\n## First\ntext\n";
        let result = resolve_heading(input, "## First");

        // Duplicate headings require an explicit TOC address.
        assert_eq!(result, None);
    }

    #[test]
    fn last_heading_to_eof() {
        let input = b"# Start\ntext\n## End\nfinal line\n";
        let result = resolve_heading(input, "## End");

        // Last heading should extend to total_lines (4)
        assert_eq!(result, Some((3, 4)));
    }

    #[test]
    fn nested_sections() {
        let input = b"## A\ncontent\n### B\nmore\n## C\ntext\n";
        let result = resolve_heading(input, "## A");

        // ## A should include ### B, ending when ## C starts (line 5)
        // So range is [1, 4]
        assert_eq!(result, Some((1, 4)));
    }

    #[test]
    fn no_hashes() {
        let input = b"# Heading\ntext\n";

        // Empty string
        assert_eq!(resolve_heading(input, ""), None);

        // String without hashes
        assert_eq!(resolve_heading(input, "hello"), None);
    }

    #[test]
    fn full_true_size_cap_returns_outline() {
        use std::io::Write;

        // Create a temp file larger than our small cap (100 bytes)
        let path = std::env::temp_dir().join("tilth_test_large.rs");
        let mut f = std::fs::File::create(&path).unwrap();
        // Write enough to exceed the cap — 200 bytes of Rust code
        for i in 0..20 {
            writeln!(f, "pub fn func_{i}() {{ println!(\"hello\"); }}").unwrap();
        }
        drop(f);

        // Set a tiny cap so the guard triggers
        std::env::set_var("TILTH_FULL_SIZE_CAP", "100");

        let cache = OutlineCache::new();
        let result = read_file(&path, None, true, &cache, false).unwrap();

        // Should contain the warning, not the full file content
        assert!(
            result.contains("full=true skipped"),
            "expected size cap warning, got: {result}"
        );
        assert!(
            result.contains("func_0"),
            "expected outline content in output"
        );

        std::env::remove_var("TILTH_FULL_SIZE_CAP");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn suggest_headings_returns_close_matches() {
        let input = b"# Architecture\nfoo\n## Getting Started\nbar\n## Configuration\nbaz\n";
        let suggestions = suggest_headings(input, "## Get Started", 5);
        assert!(
            suggestions.iter().any(|h| h.contains("Getting Started")),
            "expected 'Getting Started' in suggestions, got: {suggestions:?}"
        );
    }

    #[test]
    fn suggest_headings_top_n_orders_by_distance() {
        let input = b"# A\nfoo\n## Configuration\nbar\n## Authentication\nbaz\n## Settings\nqux\n";
        // Whole-word typo of "Configuration" — Levenshtein favours close-length
        // candidates here, so "Configuration" must rank ahead of the others.
        let suggestions = suggest_headings(input, "## Configurashun", 5);
        assert!(
            suggestions[0].contains("Configuration"),
            "expected 'Configuration' first, got: {suggestions:?}"
        );
    }

    #[test]
    fn suggest_headings_skips_code_blocks() {
        let input = b"## Real Heading\nfoo\n```md\n## Inside Code\n```\n";
        let suggestions = suggest_headings(input, "## Heading", 5);
        // Heading inside code block must NOT appear
        assert!(
            !suggestions.iter().any(|h| h.contains("Inside Code")),
            "fenced heading leaked into suggestions: {suggestions:?}"
        );
        assert!(
            suggestions.iter().any(|h| h.contains("Real Heading")),
            "expected real heading in suggestions: {suggestions:?}"
        );
    }

    #[test]
    fn suggest_headings_empty_query_returns_empty() {
        let input = b"# A\n## B\n";
        assert!(suggest_headings(input, "", 5).is_empty());
        assert!(suggest_headings(input, "###", 5).is_empty());
    }

    /// `CommonMark` allows `~~~` as a fence delimiter. Headings inside
    /// must not be treated as suggestable.
    #[test]
    fn suggest_headings_skips_tilde_fenced_blocks() {
        let input = b"## Real Heading\nfoo\n~~~md\n## Inside Tilde Fence\n~~~\n";
        let suggestions = suggest_headings(input, "## Heading", 5);
        assert!(
            !suggestions.iter().any(|h| h.contains("Inside Tilde Fence")),
            "tilde-fenced heading leaked: {suggestions:?}"
        );
        assert!(
            suggestions.iter().any(|h| h.contains("Real Heading")),
            "real heading missing: {suggestions:?}"
        );
    }

    /// `CommonMark` §4.6.1 limits ATX headings to 1–6 `#`s. Lines with 7+
    /// hashes are not headings, even with a space after.
    #[test]
    fn suggest_headings_rejects_seven_or_more_hashes() {
        let input = b"## Real Heading\nfoo\n####### Not a Heading\n";
        let suggestions = suggest_headings(input, "## Heading", 5);
        assert!(
            !suggestions.iter().any(|h| h.contains("Not a Heading")),
            "7-hash line leaked as heading: {suggestions:?}"
        );
    }

    /// `CommonMark` §4.6.1: hashes must be followed by a space (or EOL).
    /// `##foo` (no space) is not a heading.
    #[test]
    fn suggest_headings_rejects_hashes_without_space() {
        let input = b"## Real Heading\nfoo\n##NoSpace\n";
        let suggestions = suggest_headings(input, "## Heading", 5);
        assert!(
            !suggestions.iter().any(|h| h.contains("NoSpace")),
            "##NoSpace leaked as heading: {suggestions:?}"
        );
    }

    /// Filename and heading suggestion rely on Unicode-scalar-level edit
    /// distance, not byte-level — locks in the contract `strsim::levenshtein`
    /// provides via its char-iterating wrapper. If `strsim` ever switches
    /// to a byte-level distance, this test fails loudly.
    #[test]
    fn edit_distance_is_unicode_aware() {
        // 设置 (Settings) and 設定 (Configuration) — different chars,
        // each one Unicode scalar. Distance should be 2, not 6.
        assert_eq!(edit_distance("设置", "設定"), 2);
        // emoji single-scalar: 🦀 vs 🐙 = distance 1.
        assert_eq!(edit_distance("🦀", "🐙"), 1);
        // ASCII baseline still works.
        assert_eq!(edit_distance("kitten", "sitting"), 3);
    }

    fn write_temp(name: &str, content: &str) -> std::path::PathBuf {
        use std::io::Write;
        let path = std::env::temp_dir().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        path
    }

    #[test]
    fn read_ranges_single_matches_legacy_section() {
        // One range produces no `─── lines X-Y ───` delimiter — output is
        // identical in shape to the pre-multi-section read.
        let path = write_temp(
            "tilth_test_ranges_single.txt",
            "alpha\nbeta\ngamma\ndelta\nepsilon\n",
        );
        let out = read_ranges(&path, &["2-3"], false).unwrap();
        assert!(out.contains("[section]"), "expected section header: {out}");
        assert!(
            !out.contains("─── lines"),
            "single range must not emit delimiter: {out}"
        );
        assert!(out.contains("2  beta"), "expected line 2: {out}");
        assert!(out.contains("3  gamma"), "expected line 3: {out}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_ranges_disjoint_two_blocks() {
        let path = write_temp(
            "tilth_test_ranges_disjoint.txt",
            "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\n",
        );
        let out = read_ranges(&path, &["1-2", "6-7"], false).unwrap();
        assert!(out.contains("─── lines 1-2 ───"), "first delimiter: {out}");
        assert!(out.contains("─── lines 6-7 ───"), "second delimiter: {out}");
        assert!(
            out.contains("1  l1") && out.contains("7  l7"),
            "content: {out}"
        );
        // Header reports summed lines — 2 + 2 = 4
        assert!(out.contains("(4 lines"), "summed line_count: {out}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_ranges_preserves_user_order() {
        // Out-of-order ranges are NOT sorted — emit verbatim.
        let path = write_temp("tilth_test_ranges_order.txt", "a\nb\nc\nd\ne\nf\n");
        let out = read_ranges(&path, &["5-6", "1-2"], false).unwrap();
        let later = out.find("─── lines 5-6 ───").unwrap();
        let earlier = out.find("─── lines 1-2 ───").unwrap();
        assert!(
            later < earlier,
            "5-6 must appear before 1-2 (user order): {out}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_ranges_overlap_is_emitted_verbatim() {
        // Overlap is honored — duplicated content, no coalescing.
        let path = write_temp("tilth_test_ranges_overlap.txt", "x1\nx2\nx3\nx4\nx5\n");
        let out = read_ranges(&path, &["1-3", "2-4"], false).unwrap();
        assert!(out.contains("─── lines 1-3 ───"), "first block: {out}");
        assert!(out.contains("─── lines 2-4 ───"), "second block: {out}");
        // line 2 ("x2") appears in both blocks
        let occurrences = out.matches("  x2\n").count();
        assert_eq!(
            occurrences, 2,
            "expected x2 to appear twice (overlap): {out}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_ranges_mixed_line_and_heading() {
        let path = write_temp(
            "tilth_test_ranges_mixed.md",
            "# Top\nintro line\n## Foo\nfoo body\n## Bar\nbar body\n",
        );
        let out = read_ranges(&path, &["1-2", "## Bar"], false).unwrap();
        assert!(
            out.contains("─── lines 1-2 ───"),
            "line-range delimiter: {out}"
        );
        // "## Bar" lives at lines 5-6 in this fixture
        assert!(
            out.contains("─── lines 5-6 ───"),
            "heading-resolved delimiter: {out}"
        );
        assert!(
            out.contains("intro line") && out.contains("bar body"),
            "content: {out}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_ranges_invalid_second_range_fails_whole_call() {
        let path = write_temp("tilth_test_ranges_invalid.txt", "one\ntwo\nthree\n");
        let err = read_ranges(&path, &["1-2", "not-a-range"], false).unwrap_err();
        assert!(
            matches!(err, TilthError::InvalidQuery { .. }),
            "expected InvalidQuery, got: {err:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_ranges_empty_input_errors() {
        let path = write_temp("tilth_test_ranges_empty.txt", "a\nb\n");
        let err = read_ranges(&path, &[], false).unwrap_err();
        assert!(
            matches!(err, TilthError::InvalidQuery { .. }),
            "expected InvalidQuery for empty input, got: {err:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_ranges_edit_mode_emits_hashlines_per_block() {
        // Edit-mode output must be hashlined inside every block, not just the first.
        // Hashline format is `<line>:<3hex>|<content>`.
        let path = write_temp(
            "tilth_test_ranges_edit_mode.txt",
            "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\n",
        );
        let out = read_ranges(&path, &["1-2", "5-6"], true).unwrap();
        // Both delimiters present
        assert!(out.contains("─── lines 1-2 ───"), "first delimiter: {out}");
        assert!(out.contains("─── lines 5-6 ───"), "second delimiter: {out}");
        // Hashline anchors present for at least one line in each block (the
        // exact hash depends on FNV-1a so just check the `<n>:<hex>|` shape).
        let line_one_match = out
            .lines()
            .any(|l| l.starts_with("1:") && l.contains("|alpha"));
        let line_six_match = out
            .lines()
            .any(|l| l.starts_with("6:") && l.contains("|zeta"));
        assert!(line_one_match, "expected hashline for line 1: {out}");
        assert!(line_six_match, "expected hashline for line 6: {out}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn budgeted_passes_through_when_budget_ample() {
        let path = write_temp(
            "tilth_test_budgeted_passthrough.txt",
            "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\n",
        );
        let plain = read_ranges(&path, &["1-2", "5-6"], false).unwrap();
        let budgeted = read_ranges_with_budget(&path, &["1-2", "5-6"], false, 100_000).unwrap();
        assert_eq!(plain, budgeted);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn budgeted_omits_later_sections_when_exhausted() {
        use std::fmt::Write as _;
        let big_line = "x".repeat(200);
        let body = (0..30).fold(String::new(), |mut s, _| {
            let _ = writeln!(s, "{big_line}");
            s
        });
        let path = write_temp("tilth_test_budgeted_omit.txt", &body);
        let budget = 240u64;
        let out =
            read_ranges_with_budget(&path, &["1-3", "10-12", "20-22"], false, budget).unwrap();

        assert!(out.contains("─── lines 1-3 ───"), "first marker: {out}");
        assert!(
            out.contains("─── lines 10-12 ───\n... section omitted (budget exhausted) ..."),
            "second omitted: {out}"
        );
        assert!(
            out.contains("─── lines 20-22 ───\n... section omitted (budget exhausted) ..."),
            "third omitted: {out}"
        );
        assert!(
            estimate_tokens(out.len() as u64) <= budget,
            "output {} tokens > budget {budget}: {out}",
            estimate_tokens(out.len() as u64)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn budgeted_truncates_first_section_when_too_large() {
        use std::fmt::Write as _;
        let body = (0..200).fold(String::new(), |mut s, i| {
            let _ = writeln!(s, "line-{i}-padding-padding-padding");
            s
        });
        let path = write_temp("tilth_test_budgeted_first_too_large.txt", &body);
        let budget = 100u64;
        let out = read_ranges_with_budget(&path, &["1-150"], false, budget).unwrap();
        assert!(
            out.contains("... truncated"),
            "expected truncation sentinel: {out}"
        );
        assert!(
            estimate_tokens(out.len() as u64) <= budget,
            "output {} tokens > budget {budget}: {out}",
            estimate_tokens(out.len() as u64)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn budgeted_preserves_user_order_until_exhaustion() {
        use std::fmt::Write as _;
        let big_line = "y".repeat(200);
        let body = (0..30).fold(String::new(), |mut s, _| {
            let _ = writeln!(s, "{big_line}");
            s
        });
        let path = write_temp("tilth_test_budgeted_order.txt", &body);
        let budget = 240u64;
        let out =
            read_ranges_with_budget(&path, &["20-22", "1-3", "10-12"], false, budget).unwrap();
        let pos_first = out.find("─── lines 20-22 ───").expect("first marker");
        let pos_second = out.find("─── lines 1-3 ───").expect("second marker");
        let pos_third = out.find("─── lines 10-12 ───").expect("third marker");
        assert!(
            pos_first < pos_second && pos_second < pos_third,
            "order: {out}"
        );
        assert!(
            out.contains("─── lines 10-12 ───\n... section omitted (budget exhausted) ..."),
            "third must be omitted: {out}"
        );
        assert!(
            estimate_tokens(out.len() as u64) <= budget,
            "output {} tokens > budget {budget}: {out}",
            estimate_tokens(out.len() as u64)
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn budgeted_never_exceeds_budget_even_when_markers_do_not_fit() {
        use std::fmt::Write as _;
        let big_line = "z".repeat(50);
        let body = (0..30).fold(String::new(), |mut s, _| {
            let _ = writeln!(s, "{big_line}");
            s
        });
        let path = write_temp("tilth_test_budgeted_strict_cap.txt", &body);
        let budget = 80u64;
        let out =
            read_ranges_with_budget(&path, &["1-3", "10-12", "20-22"], false, budget).unwrap();
        assert!(
            estimate_tokens(out.len() as u64) <= budget,
            "output {} tokens > budget {budget}: {out}",
            estimate_tokens(out.len() as u64)
        );
        let _ = std::fs::remove_file(&path);
    }

    // OGATE: "never-worse outline gate" — if the outline barely compresses
    // (outline_tokens >= 80% of full-file tokens), return full content instead.
    #[test]
    fn ogate_flat_file_returns_full_when_outline_barely_compresses() {
        use std::fmt::Write as _;
        // A file of empty-body one-liner fns. The signature IS essentially the
        // whole source line (only ` {}` is dropped), so any signature-bearing
        // outline is ≥80% of the full-file tokens and OGATE must fire. This holds
        // regardless of the outline's per-entry formatting overhead, so the test
        // does not silently break if that format is ever tightened. Enough lines
        // to clear TOKEN_THRESHOLD (6k) — otherwise the small-file gate returns
        // full before OGATE is even consulted, passing for the wrong reason.
        let mut src = String::new();
        for i in 0..2000 {
            writeln!(src, "pub fn flat_{i}() {{}}").unwrap();
        }
        // ~2000 * ~21 bytes ≈ 43 KB ≈ 10.7k tokens (> TOKEN_THRESHOLD = 6k).
        // Assert it so the test provably exercises OGATE rather than the
        // small-file gate (which would also return `[full]`).
        assert!(
            estimate_tokens(src.len() as u64) > TOKEN_THRESHOLD,
            "fixture must exceed TOKEN_THRESHOLD so OGATE, not the small-file gate, returns full"
        );
        let path = write_temp("tilth_ogate_flat.rs", &src);

        let cache = OutlineCache::new();
        let result = read_file(&path, None, false, &cache, false).unwrap();

        // Gate fired → full view. The header tag records the decision directly
        // (an outline would read `[outline]`), and the trailing fn's body braces
        // `() {}` only appear in verbatim full content — an outline renders the
        // signature without the body.
        assert!(
            result.contains("[full]"),
            "OGATE must return the full view; header was: {}",
            result.lines().next().unwrap_or("")
        );
        assert!(
            result.contains("flat_1999() {}"),
            "expected verbatim full content (last fn body present)"
        );

        let _ = std::fs::remove_file(&path);
    }

    /// Contrasting test: a large file with big function bodies compresses well
    /// and must still return the OUTLINE (gate must NOT fire).
    #[test]
    fn ogate_large_compressible_file_returns_outline() {
        use std::fmt::Write as _;
        // Each function has a large body (many comment lines) so the outline
        // (which only shows the `fn` signature) is a tiny fraction of the
        // full-file token count.
        let mut src = String::new();
        for i in 0..40 {
            writeln!(src, "pub fn heavy_{i}() {{").unwrap();
            // 200 lines of body per function → ~200 * 30 = 6 000 bytes each
            for j in 0..200 {
                writeln!(
                    src,
                    "    let _x_{j}: u64 = {j}_{i}_padding_value_that_fills_space;"
                )
                .unwrap();
            }
            writeln!(src, "}}").unwrap();
        }
        // Total ≈ 40 * 202 lines * ~50 bytes = ~400 KB → well over TOKEN_THRESHOLD
        let path = write_temp("tilth_ogate_heavy.rs", &src);

        let cache = OutlineCache::new();
        let result = read_file(&path, None, false, &cache, false).unwrap();

        // Gate must NOT have fired: the output should be an outline, not full content.
        // An outline shows signatures but omits the body.
        assert!(
            result.contains("heavy_0"),
            "expected outline to mention function name heavy_0: {}",
            &result[..result.len().min(400)]
        );
        // Body variable lines are only in the full file, not in an outline
        assert!(
            !result.contains("_x_0: u64"),
            "outline must not contain body variables; got full content instead"
        );

        let _ = std::fs::remove_file(&path);
    }
}
