pub mod code;
pub mod fallback;
pub mod markdown;
pub mod structured;
pub mod tabular;
pub mod test_file;

use std::path::Path;

use crate::types::FileType;

const OUTLINE_CAP: usize = 100; // max outline lines for huge files

/// Generate a smart view based on file type.
pub fn generate(
    path: &Path,
    file_type: FileType,
    content: &str,
    buf: &[u8],
    capped: bool,
) -> String {
    let max_lines = if capped { OUTLINE_CAP } else { usize::MAX };

    // Test files get special treatment regardless of language
    if crate::types::is_test_file(path) {
        if let FileType::Code(lang) = file_type {
            if let Some(outline) = test_file::outline(content, lang, max_lines) {
                return with_omission_note(outline, max_lines);
            }
        }
    }

    let outline = match file_type {
        FileType::Code(lang) => code::outline(content, lang, max_lines),
        FileType::Markdown => markdown::outline(buf, max_lines),
        FileType::StructuredData => structured::outline(path, content, max_lines),
        FileType::Tabular => tabular::outline(content, max_lines),
        FileType::Log => fallback::log_view(content),
        FileType::Other => fallback::head_tail(content),
    };
    if matches!(file_type, FileType::Markdown) {
        // Table headers and selector hints are not heading entries.
        let entries = outline.lines().filter(|line| line.starts_with('[')).count();
        with_omission_note_for_entries(outline, max_lines, entries)
    } else {
        with_omission_note(outline, max_lines)
    }
}

/// Append a note when the outline likely hit `max_lines` and more symbols
/// exist below. Without this note, agents read the outline as exhaustive
/// and miss symbols below the cap.
///
/// Most backends emit one line per entry. Markdown tables include a header
/// and selector hint, so `generate()` supplies their heading-row count directly.
/// This remains a heuristic: reaching the cap can mean more entries exist;
/// avoid claiming a specific omitted count.
fn with_omission_note(outline: String, max_lines: usize) -> String {
    let entries = outline.lines().count();
    with_omission_note_for_entries(outline, max_lines, entries)
}

fn with_omission_note_for_entries(outline: String, max_lines: usize, entries: usize) -> String {
    if max_lines == usize::MAX {
        return outline;
    }
    if entries < max_lines {
        return outline;
    }
    format!(
        "{outline}\n\n> outline truncated — more symbols exist below the cap. \
         Use section=\"<start>-<end>\" with the line numbers shown in [...] \
         brackets above, or tilth_search \"<name>\" for a specific symbol."
    )
}

#[cfg(test)]
mod tests {
    use super::with_omission_note;
    use std::fmt::Write as _;

    #[test]
    fn markdown_table_header_and_hint_do_not_consume_heading_cap() {
        for count in [98, 99, 100, 101] {
            let content = (0..count).fold(String::new(), |mut text, i| {
                let _ = writeln!(text, "# Heading {i}\nbody");
                text
            });
            let result = super::generate(
                std::path::Path::new("guide.md"),
                crate::types::FileType::Markdown,
                &content,
                content.as_bytes(),
                true,
            );
            assert_eq!(
                result.lines().filter(|line| line.starts_with('[')).count(),
                count.min(100)
            );
            assert_eq!(
                result.contains("outline truncated"),
                count >= 100,
                "{count}: {result}"
            );
        }
    }

    #[test]
    fn note_appended_when_at_cap() {
        let outline = (0..100)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = with_omission_note(outline, 100);
        assert!(result.contains("outline truncated"));
    }

    #[test]
    fn no_note_when_under_cap() {
        let outline = (0..50)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = with_omission_note(outline.clone(), 100);
        assert_eq!(result, outline);
    }

    #[test]
    fn no_note_when_uncapped() {
        let outline = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let result = with_omission_note(outline.clone(), usize::MAX);
        assert_eq!(result, outline);
    }

    /// Integration test: drive the full `generate()` pipeline with a
    /// real Rust source containing more than `OUTLINE_CAP` top-level
    /// functions. Verifies the cap actually fires and that
    /// `with_omission_note` is wired into the pipeline correctly —
    /// not just exercised in isolation.
    #[test]
    fn integration_note_on_capped_code_file() {
        let src = (0..150).fold(String::new(), |mut s, i| {
            let _ = writeln!(s, "pub fn func_{i}() {{}}");
            s
        });
        let path = std::path::Path::new("fake.rs");
        let file_type = crate::types::FileType::Code(crate::types::Lang::Rust);
        let result = super::generate(path, file_type, &src, src.as_bytes(), true);
        assert!(
            result.contains("outline truncated"),
            "expected truncation note for 150 funcs over OUTLINE_CAP=100, got:\n{result}"
        );
    }

    /// Integration test: a small file (5 functions) must NOT produce
    /// the truncation note even when `capped=true` is passed, because
    /// the actual entry count is well below the cap.
    #[test]
    fn integration_no_note_on_small_code_file() {
        let src = (0..5).fold(String::new(), |mut s, i| {
            let _ = writeln!(s, "pub fn func_{i}() {{}}");
            s
        });
        let path = std::path::Path::new("fake.rs");
        let file_type = crate::types::FileType::Code(crate::types::Lang::Rust);
        let result = super::generate(path, file_type, &src, src.as_bytes(), true);
        assert!(
            !result.contains("outline truncated"),
            "spurious truncation note for 5 funcs (under OUTLINE_CAP=100):\n{result}"
        );
    }
}
