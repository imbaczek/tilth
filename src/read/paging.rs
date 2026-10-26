use crate::cache::OutlineCache;
use crate::error::TilthError;
use crate::listing::Page;
use crate::types::{FileType, ViewMode};
use std::path::Path;

pub(crate) fn read_file_page(
    path: &Path,
    full: bool,
    _cache: &OutlineCache,
    edit: bool,
    page: Page,
    mcp: bool,
) -> Result<String, TilthError> {
    let bytes = super::source::bytes(path).map_err(|source| match source.kind() {
        std::io::ErrorKind::NotFound => TilthError::NotFound {
            path: path.into(),
            suggestion: None,
        },
        std::io::ErrorKind::PermissionDenied => TilthError::PermissionDenied { path: path.into() },
        _ => TilthError::IoError {
            path: path.into(),
            source,
        },
    })?;
    if crate::lang::detection::is_binary(&bytes) {
        return Ok(page.render(
            &crate::format::binary_header(path, bytes.len() as u64, super::mime_from_ext(path)),
            &[],
            "entries",
            mcp,
        ));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let skipped = if crate::lang::detection::is_generated_by_name(name)
        || crate::lang::detection::is_generated_by_content(&bytes)
    {
        Some(ViewMode::Generated)
    } else if crate::lang::detection::is_minified_by_name(name)
        || (bytes.len() as u64 >= crate::lang::detection::MINIFIED_CHECK_THRESHOLD
            && crate::lang::detection::is_minified_by_content(&bytes))
    {
        Some(ViewMode::Minified)
    } else {
        None
    };
    if let Some(mode) = skipped {
        return Ok(page.render(
            &crate::format::file_header(
                path,
                bytes.len() as u64,
                bytes.split(|byte| *byte == b'\n').count() as u32,
                mode,
            ),
            &[],
            "entries",
            mcp,
        ));
    }
    let content = String::from_utf8_lossy(&bytes);
    let count = u32::try_from(content.lines().count()).unwrap_or(u32::MAX);
    let file_type = crate::lang::detect_file_type(path);
    if !full && matches!(file_type, FileType::Markdown) {
        return super::read_markdown_outline_page(path, page, mcp);
    }
    if let FileType::Code(lang) = file_type {
        if !full {
            let entries = super::outline::code::page_blocks(&content, lang);
            if !entries.is_empty() {
                let header =
                    crate::format::file_header(path, bytes.len() as u64, count, ViewMode::Outline);
                return Ok(page.render(&header, &entries, "declarations", mcp));
            }
        }
    }
    let header = crate::format::file_header(path, bytes.len() as u64, count, ViewMode::Full);
    let (start, end) = page.bounds(content.lines().count());
    let mut out = format!(
        "{header}\n\n{}\n\n",
        page.summary(content.lines().count(), "source lines")
    );
    for (index, line) in content.lines().enumerate().skip(start).take(end - start) {
        use std::fmt::Write as _;
        if edit {
            let hash = crate::format::line_hash(line.as_bytes());
            let _ = writeln!(out, "{}:{hash:03x}|{line}", index + 1);
        } else {
            let _ = writeln!(out, "{}  {line}", index + 1);
        }
    }
    page.append_navigation(&mut out, content.lines().count(), mcp);
    Ok(out)
}
